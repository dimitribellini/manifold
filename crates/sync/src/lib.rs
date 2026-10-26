//! Directory Synchronization, Diff Engine, and Multi-Rename (Batch Renamer)
//! for Project Manifold (`Sections 3.5 & 3.6` of `BRIEF.md`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use heck::{ToKebabCase, ToLowerCamelCase, ToSnakeCase, ToTitleCase};
use manifold_vfs::{FileMetadata, FileSystemBackend, VfsError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Synchronization strategy (`Section 3.5` of `BRIEF.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncStrategy {
    LeftToRight,
    RightToLeft,
    Bidirectional,
}

/// File comparison mode (`Section 3.5` of `BRIEF.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonMode {
    Metadata,
    ContentHash,
    BinaryDiff,
}

/// Planned operation for a single relative path in the synchronization changeset tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncOperation {
    CopyLeftToRight,
    CopyRightToLeft,
    DeleteLeft,
    DeleteRight,
    Conflict,
    Identical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncChangesetEntry {
    pub relative_path: String,
    pub left_meta: Option<FileMetadata>,
    pub right_meta: Option<FileMetadata>,
    pub operation: SyncOperation,
    pub approved: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncExecutionPlan {
    pub left_root: String,
    pub right_root: String,
    pub strategy: Option<SyncStrategy>,
    pub comparison_mode: Option<ComparisonMode>,
    pub entries: Vec<SyncChangesetEntry>,
}

impl SyncExecutionPlan {
    /// Builds an inspectable operational changeset tree between `left_vfs` and `right_vfs`.
    pub fn build(
        left_vfs: &dyn FileSystemBackend,
        left_root: &str,
        right_vfs: &dyn FileSystemBackend,
        right_root: &str,
        strategy: SyncStrategy,
        mode: ComparisonMode,
    ) -> Result<Self, VfsError> {
        let left_items = left_vfs.list_directory(left_root)?;
        let right_items = right_vfs.list_directory(right_root)?;

        let mut left_map: HashMap<String, FileMetadata> = HashMap::new();
        for item in left_items {
            left_map.insert(item.name.clone(), item);
        }

        let mut right_map: HashMap<String, FileMetadata> = HashMap::new();
        for item in right_items {
            right_map.insert(item.name.clone(), item);
        }

        let mut all_names = BTreeSet::new();
        all_names.extend(left_map.keys().cloned());
        all_names.extend(right_map.keys().cloned());

        let mut entries = Vec::new();
        for name in all_names {
            let l = left_map.get(&name).cloned();
            let r = right_map.get(&name).cloned();

            let operation = match (&l, &r) {
                (Some(_), None) => match strategy {
                    SyncStrategy::LeftToRight | SyncStrategy::Bidirectional => {
                        SyncOperation::CopyLeftToRight
                    }
                    SyncStrategy::RightToLeft => SyncOperation::DeleteLeft,
                },
                (None, Some(_)) => match strategy {
                    SyncStrategy::LeftToRight => SyncOperation::DeleteRight,
                    SyncStrategy::RightToLeft | SyncStrategy::Bidirectional => {
                        SyncOperation::CopyRightToLeft
                    }
                },
                (Some(lm), Some(rm)) => {
                    let same = Self::files_equal(left_vfs, lm, right_vfs, rm, mode)?;
                    if same {
                        SyncOperation::Identical
                    } else {
                        match strategy {
                            SyncStrategy::LeftToRight => SyncOperation::CopyLeftToRight,
                            SyncStrategy::RightToLeft => SyncOperation::CopyRightToLeft,
                            SyncStrategy::Bidirectional => match (lm.modified, rm.modified) {
                                (Some(lt), Some(rt)) if lt > rt => SyncOperation::CopyLeftToRight,
                                (Some(lt), Some(rt)) if rt > lt => SyncOperation::CopyRightToLeft,
                                _ => SyncOperation::Conflict,
                            },
                        }
                    }
                }
                (None, None) => unreachable!(),
            };

            let is_noop = matches!(
                operation,
                SyncOperation::Identical | SyncOperation::Conflict
            );
            let approved = !is_noop;
            entries.push(SyncChangesetEntry {
                relative_path: name,
                left_meta: l,
                right_meta: r,
                operation,
                approved,
            });
        }

        Ok(Self {
            left_root: left_root.to_string(),
            right_root: right_root.to_string(),
            strategy: Some(strategy),
            comparison_mode: Some(mode),
            entries,
        })
    }

    fn files_equal(
        left_vfs: &dyn FileSystemBackend,
        lm: &FileMetadata,
        right_vfs: &dyn FileSystemBackend,
        rm: &FileMetadata,
        mode: ComparisonMode,
    ) -> Result<bool, VfsError> {
        if lm.is_dir && rm.is_dir {
            return Ok(true);
        }
        if lm.is_dir != rm.is_dir {
            return Ok(false);
        }
        match mode {
            ComparisonMode::Metadata => Ok(lm.size == rm.size && lm.modified == rm.modified),
            ComparisonMode::ContentHash => {
                if lm.size != rm.size {
                    return Ok(false);
                }
                let lb = left_vfs.read_bytes(&lm.path)?;
                let rb = right_vfs.read_bytes(&rm.path)?;
                Ok(Sha256::digest(&lb) == Sha256::digest(&rb))
            }
            ComparisonMode::BinaryDiff => {
                if lm.size != rm.size {
                    return Ok(false);
                }
                let lb = left_vfs.read_bytes(&lm.path)?;
                let rb = right_vfs.read_bytes(&rm.path)?;
                Ok(lb == rb)
            }
        }
    }

    /// Executes all approved changeset entries across `left_vfs` and `right_vfs`.
    pub fn execute(
        &self,
        left_vfs: &dyn FileSystemBackend,
        right_vfs: &dyn FileSystemBackend,
    ) -> Result<usize, VfsError> {
        let mut applied = 0usize;
        for entry in &self.entries {
            if !entry.approved {
                continue;
            }
            let l_path = join_vfs_path(&self.left_root, &entry.relative_path);
            let r_path = join_vfs_path(&self.right_root, &entry.relative_path);

            match entry.operation {
                SyncOperation::CopyLeftToRight => {
                    let bytes = left_vfs.read_bytes(&l_path)?;
                    right_vfs.write_bytes(&r_path, &bytes)?;
                    applied += 1;
                }
                SyncOperation::CopyRightToLeft => {
                    let bytes = right_vfs.read_bytes(&r_path)?;
                    left_vfs.write_bytes(&l_path, &bytes)?;
                    applied += 1;
                }
                SyncOperation::DeleteLeft => {
                    left_vfs.remove(&l_path, true)?;
                    applied += 1;
                }
                SyncOperation::DeleteRight => {
                    right_vfs.remove(&r_path, true)?;
                    applied += 1;
                }
                SyncOperation::Conflict | SyncOperation::Identical => {}
            }
        }
        Ok(applied)
    }
}

fn join_vfs_path(root: &str, child: &str) -> String {
    let trimmed_root = root.trim_end_matches('/');
    let trimmed_child = child.trim_start_matches('/');
    if trimmed_root.is_empty() {
        format!("/{trimmed_child}")
    } else {
        format!("{trimmed_root}/{trimmed_child}")
    }
}

/// Case conversion styles supported by the Multi-Rename tool (`Section 3.6` of `BRIEF.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaseConversionStyle {
    UpperCase,
    LowerCase,
    TitleCase,
    CamelCase,
    KebabCase,
    SnakeCase,
}

/// Rule-based transformation pipeline steps for Multi-Rename (`Section 3.6` of `BRIEF.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenameRule {
    TextReplace {
        find: String,
        replace: String,
        case_sensitive: bool,
    },
    /// Matches a prefix + digits capture group (e.g. `IMG_(\d+)` -> `Vacation_$1`).
    DigitCaptureReplace {
        prefix: String,
        replacement_template: String,
    },
    CaseConversion(CaseConversionStyle),
    IncrementalCounter {
        prefix: String,
        suffix: String,
        start: usize,
        step: usize,
        padding: usize,
    },
    DateTimeInjection {
        prefix: String,
        formatted_timestamp: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamePreviewItem {
    pub original_name: String,
    pub proposed_name: String,
    pub has_collision: bool,
}

/// Multi-Rename engine applying a pipeline of [`RenameRule`]s with live collision validation.
pub struct BatchRenamer {
    pub rules: Vec<RenameRule>,
}

impl BatchRenamer {
    pub fn new(rules: Vec<RenameRule>) -> Self {
        Self { rules }
    }

    /// Evaluates the transformation pipeline over `original_names` and checks for collisions
    /// both within the batch and against `existing_directory_names`.
    pub fn preview(
        &self,
        original_names: &[String],
        existing_directory_names: &HashSet<String>,
    ) -> Vec<RenamePreviewItem> {
        let original_set: HashSet<String> = original_names.iter().cloned().collect();
        let mut proposed_list = Vec::with_capacity(original_names.len());
        let mut counts: HashMap<String, usize> = HashMap::new();

        for (index, orig) in original_names.iter().enumerate() {
            let proposed = self.apply_to_single(orig, index);
            *counts.entry(proposed.clone()).or_insert(0) += 1;
            proposed_list.push((orig.clone(), proposed));
        }

        proposed_list
            .into_iter()
            .map(|(original_name, proposed_name)| {
                let duplicate_in_batch = counts.get(&proposed_name).copied().unwrap_or(0) > 1;
                let collides_with_unselected = proposed_name != original_name
                    && existing_directory_names.contains(&proposed_name)
                    && !original_set.contains(&proposed_name);
                RenamePreviewItem {
                    original_name,
                    proposed_name,
                    has_collision: duplicate_in_batch || collides_with_unselected,
                }
            })
            .collect()
    }

    fn apply_to_single(&self, filename: &str, index: usize) -> String {
        let path = Path::new(filename);
        let ext = path.extension().and_then(|e| e.to_str());
        let raw_stem = path.file_stem().and_then(|s| s.to_str());
        let mut stem = raw_stem.unwrap_or(filename).to_string();

        for rule in &self.rules {
            match rule {
                RenameRule::TextReplace {
                    find,
                    replace,
                    case_sensitive,
                } => {
                    if !find.is_empty() {
                        stem = if *case_sensitive {
                            stem.replace(find, replace)
                        } else {
                            replace_case_insensitive(&stem, find, replace)
                        };
                    }
                }
                RenameRule::DigitCaptureReplace {
                    prefix,
                    replacement_template,
                } => {
                    if let Some(rest) = stem.strip_prefix(prefix) {
                        let digits: String =
                            rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                        let tail: String =
                            rest.chars().skip_while(|c| c.is_ascii_digit()).collect();
                        if !digits.is_empty() {
                            let expanded = replacement_template.replace("$1", &digits);
                            stem = format!("{expanded}{tail}");
                        }
                    }
                }
                RenameRule::CaseConversion(style) => {
                    stem = match style {
                        CaseConversionStyle::UpperCase => stem.to_uppercase(),
                        CaseConversionStyle::LowerCase => stem.to_lowercase(),
                        CaseConversionStyle::TitleCase => stem.to_title_case(),
                        CaseConversionStyle::CamelCase => stem.to_lower_camel_case(),
                        CaseConversionStyle::KebabCase => stem.to_kebab_case(),
                        CaseConversionStyle::SnakeCase => stem.to_snake_case(),
                    };
                }
                RenameRule::IncrementalCounter {
                    prefix,
                    suffix,
                    start,
                    step,
                    padding,
                } => {
                    let num = start + index * step;
                    let formatted_num = format!("{num:0width$}", width = *padding);
                    stem = format!("{prefix}{stem}_{formatted_num}{suffix}");
                }
                RenameRule::DateTimeInjection {
                    prefix,
                    formatted_timestamp,
                } => {
                    stem = format!("{prefix}{stem}_{formatted_timestamp}");
                }
            }
        }

        match ext {
            Some(e) if !e.is_empty() => format!("{stem}.{e}"),
            _ => stem,
        }
    }
}

fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let lower_h = haystack.to_lowercase();
    let lower_n = needle.to_lowercase();
    let mut result = String::new();
    let mut last_end = 0;
    for (idx, _) in lower_h.match_indices(&lower_n) {
        result.push_str(&haystack[last_end..idx]);
        result.push_str(replacement);
        last_end = idx + needle.len();
    }
    result.push_str(&haystack[last_end..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifold_vfs::{ArchiveBackend, ArchiveFormat};

    #[test]
    fn test_directory_sync_sha256_changeset_and_execution() {
        let left = ArchiveBackend::new("left.zip", ArchiveFormat::Zip);
        let right = ArchiveBackend::new("right.zip", ArchiveFormat::Zip);

        left.write_bytes("/activate.php", b"v2").unwrap();
        left.write_bytes("/new_left.php", b"new").unwrap();
        right.write_bytes("/activate.php", b"v1").unwrap();
        right.write_bytes("/stale_right.php", b"old").unwrap();

        let plan = SyncExecutionPlan::build(
            &left,
            "/",
            &right,
            "/",
            SyncStrategy::LeftToRight,
            ComparisonMode::ContentHash,
        );
        let plan = plan.unwrap();

        assert_eq!(plan.entries.len(), 3);
        let applied = plan.execute(&left, &right).unwrap();
        assert_eq!(applied, 3);

        assert_eq!(right.read_bytes("/activate.php").unwrap(), b"v2");
        assert_eq!(right.read_bytes("/new_left.php").unwrap(), b"new");
        assert!(right.metadata("/stale_right.php").is_err());
    }

    #[test]
    fn test_batch_renamer_capture_groups_case_and_collision_detection() {
        let renamer = BatchRenamer::new(vec![
            RenameRule::DigitCaptureReplace {
                prefix: "IMG_".to_string(),
                replacement_template: "Vacation_$1".to_string(),
            },
            RenameRule::CaseConversion(CaseConversionStyle::SnakeCase),
        ]);

        let names = ["IMG_0042.jpg".to_string(), "IMG_0043.jpg".to_string()];
        let existing = HashSet::from(["vacation_0043.jpg".to_string()]);

        let preview = renamer.preview(&names, &existing);
        assert_eq!(preview[0].proposed_name, "vacation_0042.jpg");
        assert!(!preview[0].has_collision);
        assert_eq!(preview[1].proposed_name, "vacation_0043.jpg");
        assert!(preview[1].has_collision);
    }
}
