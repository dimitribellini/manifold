//! Directory Synchronization, Diff Engine, and Multi-Rename (Batch Renamer)
//! for Project Manifold (`Sections 3.5 & 3.6` of `BRIEF.md`).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use heck::{ToKebabCase, ToLowerCamelCase, ToSnakeCase, ToTitleCase};
use manifold_vfs::{FileMetadata, FileSystemBackend, VfsError};
use regex::RegexBuilder;
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

struct SyncBuildCtx<'a> {
    left_vfs: &'a dyn FileSystemBackend,
    right_vfs: &'a dyn FileSystemBackend,
    strategy: SyncStrategy,
    mode: ComparisonMode,
}

impl SyncExecutionPlan {
    /// Builds a recursive, inspectable operational changeset tree between `left_vfs` and `right_vfs` (`GAP-03`).
    pub fn build(
        left_vfs: &dyn FileSystemBackend,
        left_root: &str,
        right_vfs: &dyn FileSystemBackend,
        right_root: &str,
        strategy: SyncStrategy,
        mode: ComparisonMode,
    ) -> Result<Self, VfsError> {
        let mut entries = Vec::new();
        let ctx = SyncBuildCtx {
            left_vfs,
            right_vfs,
            strategy,
            mode,
        };
        Self::build_recursive(&ctx, left_root, right_root, "", 0, &mut entries)?;

        Ok(Self {
            left_root: left_root.to_string(),
            right_root: right_root.to_string(),
            strategy: Some(strategy),
            comparison_mode: Some(mode),
            entries,
        })
    }

    fn build_recursive(
        ctx: &SyncBuildCtx<'_>,
        left_dir: &str,
        right_dir: &str,
        rel_prefix: &str,
        depth: usize,
        entries: &mut Vec<SyncChangesetEntry>,
    ) -> Result<(), VfsError> {
        if depth > 32 {
            return Ok(());
        }
        let left_items = match ctx.left_vfs.list_directory(left_dir) {
            Ok(list) => list,
            Err(VfsError::NotFound(_)) if depth > 0 => Vec::new(),
            Err(e) => return Err(e),
        };
        let right_items = match ctx.right_vfs.list_directory(right_dir) {
            Ok(list) => list,
            Err(VfsError::NotFound(_)) if depth > 0 => Vec::new(),
            Err(e) => return Err(e),
        };

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

        for name in all_names {
            let l = left_map.get(&name).cloned();
            let r = right_map.get(&name).cloned();
            let rel_path = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{rel_prefix}/{name}")
            };
            let child_left_dir = join_vfs_path(left_dir, &name);
            let child_right_dir = join_vfs_path(right_dir, &name);

            if l.as_ref().is_some_and(|lm| lm.is_dir) && r.as_ref().is_some_and(|rm| rm.is_dir) {
                Self::build_recursive(
                    ctx,
                    &child_left_dir,
                    &child_right_dir,
                    &rel_path,
                    depth + 1,
                    entries,
                )?;
                continue;
            }

            let operation = match (&l, &r) {
                (Some(_), None) => match ctx.strategy {
                    SyncStrategy::LeftToRight | SyncStrategy::Bidirectional => {
                        SyncOperation::CopyLeftToRight
                    }
                    SyncStrategy::RightToLeft => SyncOperation::DeleteLeft,
                },
                (None, Some(_)) => match ctx.strategy {
                    SyncStrategy::LeftToRight => SyncOperation::DeleteRight,
                    SyncStrategy::RightToLeft | SyncStrategy::Bidirectional => {
                        SyncOperation::CopyRightToLeft
                    }
                },
                (Some(lm), Some(rm)) => {
                    let same = Self::files_equal(ctx.left_vfs, lm, ctx.right_vfs, rm, ctx.mode)?;
                    if same {
                        SyncOperation::Identical
                    } else {
                        match ctx.strategy {
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
            let recurse_left_only = l.as_ref().is_some_and(|m| m.is_dir)
                && !r.as_ref().is_some_and(|m| m.is_dir)
                && operation == SyncOperation::CopyLeftToRight;
            let recurse_right_only = r.as_ref().is_some_and(|m| m.is_dir)
                && !l.as_ref().is_some_and(|m| m.is_dir)
                && operation == SyncOperation::CopyRightToLeft;

            entries.push(SyncChangesetEntry {
                relative_path: rel_path.clone(),
                left_meta: l,
                right_meta: r,
                operation,
                approved,
            });

            if recurse_left_only || recurse_right_only {
                Self::build_recursive(
                    ctx,
                    &child_left_dir,
                    &child_right_dir,
                    &rel_path,
                    depth + 1,
                    entries,
                )?;
            }
        }

        Ok(())
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

    /// Executes all approved changeset entries across `left_vfs` and `right_vfs`,
    /// handling nested subdirectories, file-vs-directory replacements, and bottom-up deletes (`GAP-03`).
    pub fn execute(
        &self,
        left_vfs: &dyn FileSystemBackend,
        right_vfs: &dyn FileSystemBackend,
    ) -> Result<usize, VfsError> {
        let mut applied = 0usize;
        let mut pending_deletes: Vec<&SyncChangesetEntry> = Vec::new();

        for entry in &self.entries {
            if !entry.approved {
                continue;
            }
            if matches!(
                entry.operation,
                SyncOperation::DeleteLeft | SyncOperation::DeleteRight
            ) {
                pending_deletes.push(entry);
                continue;
            }
            let l_path = join_vfs_path(&self.left_root, &entry.relative_path);
            let r_path = join_vfs_path(&self.right_root, &entry.relative_path);

            match entry.operation {
                SyncOperation::CopyLeftToRight => {
                    let is_dir = entry
                        .left_meta
                        .as_ref()
                        .map(|m| m.is_dir)
                        .unwrap_or_else(|| left_vfs.metadata(&l_path).is_ok_and(|m| m.is_dir));
                    if is_dir {
                        if entry.right_meta.as_ref().is_some_and(|m| !m.is_dir) {
                            let _ = right_vfs.remove(&r_path, false);
                        }
                        right_vfs.create_dir(&r_path)?;
                    } else {
                        if let Some((parent, _)) =
                            r_path.rsplit_once('/').filter(|(p, _)| !p.is_empty())
                        {
                            let _ = right_vfs.create_dir(parent);
                        }
                        if entry.right_meta.as_ref().is_some_and(|m| m.is_dir) {
                            let _ = right_vfs.remove(&r_path, true);
                        }
                        let bytes = left_vfs.read_bytes(&l_path)?;
                        right_vfs.write_bytes(&r_path, &bytes)?;
                    }
                    applied += 1;
                }
                SyncOperation::CopyRightToLeft => {
                    let is_dir = entry
                        .right_meta
                        .as_ref()
                        .map(|m| m.is_dir)
                        .unwrap_or_else(|| right_vfs.metadata(&r_path).is_ok_and(|m| m.is_dir));
                    if is_dir {
                        if entry.left_meta.as_ref().is_some_and(|m| !m.is_dir) {
                            let _ = left_vfs.remove(&l_path, false);
                        }
                        left_vfs.create_dir(&l_path)?;
                    } else {
                        if let Some((parent, _)) =
                            l_path.rsplit_once('/').filter(|(p, _)| !p.is_empty())
                        {
                            let _ = left_vfs.create_dir(parent);
                        }
                        if entry.left_meta.as_ref().is_some_and(|m| m.is_dir) {
                            let _ = left_vfs.remove(&l_path, true);
                        }
                        let bytes = right_vfs.read_bytes(&r_path)?;
                        left_vfs.write_bytes(&l_path, &bytes)?;
                    }
                    applied += 1;
                }
                SyncOperation::DeleteLeft
                | SyncOperation::DeleteRight
                | SyncOperation::Conflict
                | SyncOperation::Identical => {}
            }
        }

        // Execute deletions deepest-path-first so child entries are removed before parent folders.
        pending_deletes.sort_by_key(|e| std::cmp::Reverse(e.relative_path.matches('/').count()));
        for entry in pending_deletes {
            let l_path = join_vfs_path(&self.left_root, &entry.relative_path);
            let r_path = join_vfs_path(&self.right_root, &entry.relative_path);
            match entry.operation {
                SyncOperation::DeleteLeft => {
                    match left_vfs.remove(&l_path, true) {
                        Ok(()) | Err(VfsError::NotFound(_)) => {}
                        Err(e) => return Err(e),
                    }
                    applied += 1;
                }
                SyncOperation::DeleteRight => {
                    match right_vfs.remove(&r_path, true) {
                        Ok(()) | Err(VfsError::NotFound(_)) => {}
                        Err(e) => return Err(e),
                    }
                    applied += 1;
                }
                _ => {}
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

/// Portion of the filename targeted by a [`RenameRule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenameTargetScope {
    Name,
    NameWithExtension,
    Extension,
}

/// Occurrence matching mode for [`RenameRule::ScopedReplaceText`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplaceOccurrences {
    All,
    First,
    Last,
    RegularExpression,
}

/// Insertion position for [`RenameRule::ScopedAddSequence`], [`RenameRule::ScopedAddText`],
/// and [`RenameRule::ScopedAddDate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextInsertionPosition {
    AfterName,
    BeforeName,
    ReplaceName,
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
    ScopedReplaceText {
        scope: RenameTargetScope,
        occurrences: ReplaceOccurrences,
        find: String,
        replace: String,
        case_sensitive: bool,
    },
    ScopedChangeCase {
        scope: RenameTargetScope,
        style: CaseConversionStyle,
    },
    ScopedAddSequence {
        scope: RenameTargetScope,
        position: TextInsertionPosition,
        prefix: String,
        start: usize,
        step: usize,
        padding: usize,
    },
    ScopedAddText {
        scope: RenameTargetScope,
        position: TextInsertionPosition,
        text: String,
    },
    ScopedAddDate {
        scope: RenameTargetScope,
        position: TextInsertionPosition,
        formatted_timestamp: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenamePreviewItem {
    pub original_name: String,
    pub proposed_name: String,
    pub has_collision: bool,
}

/// Per-item metadata supplied to [`BatchRenamer::preview_with_metadata`] (`BUG-06`, `UI-07`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRenameInputItem {
    pub name: String,
    pub is_dir: bool,
    pub modified_timestamp: Option<String>,
}

/// Prefix token used in [`RenameRule::ScopedAddDate`] to inject each file's own modified timestamp (`UI-07`).
pub const FILE_MODIFIED_TOKEN_PREFIX: &str = "__FILE_MODIFIED__:";

/// Maximum regular expression pattern length accepted by [`BatchRenamer`] (`2,048` chars).
pub const MAX_RENAME_REGEX_PATTERN_LEN: usize = 2048;

const REGEX_COMPILE_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// Multi-Rename engine applying a pipeline of [`RenameRule`]s with live collision validation (`BUG-06`).
pub struct BatchRenamer {
    pub rules: Vec<RenameRule>,
    pub case_insensitive_fs: bool,
}

impl BatchRenamer {
    pub fn new(rules: Vec<RenameRule>) -> Self {
        Self {
            rules,
            case_insensitive_fs: cfg!(any(target_os = "macos", target_os = "windows")),
        }
    }

    pub const fn with_case_insensitive(mut self, case_insensitive: bool) -> Self {
        self.case_insensitive_fs = case_insensitive;
        self
    }

    /// Evaluates the transformation pipeline over `original_names` and checks for collisions
    /// both within the batch and against `existing_directory_names`.
    pub fn preview(
        &self,
        original_names: &[String],
        existing_directory_names: &HashSet<String>,
    ) -> Vec<RenamePreviewItem> {
        let items: Vec<BatchRenameInputItem> = original_names
            .iter()
            .map(|n| BatchRenameInputItem {
                name: n.clone(),
                is_dir: false,
                modified_timestamp: None,
            })
            .collect();
        self.preview_with_metadata(&items, existing_directory_names)
    }

    /// Evaluates the transformation pipeline with directory awareness (`is_dir` skips extension splitting)
    /// and case-insensitive / Windows-safe collision validation (`BUG-06`).
    pub fn preview_with_dirs(
        &self,
        items: &[(String, bool)],
        existing_directory_names: &HashSet<String>,
    ) -> Vec<RenamePreviewItem> {
        let rich: Vec<BatchRenameInputItem> = items
            .iter()
            .map(|(n, is_dir)| BatchRenameInputItem {
                name: n.clone(),
                is_dir: *is_dir,
                modified_timestamp: None,
            })
            .collect();
        self.preview_with_metadata(&rich, existing_directory_names)
    }

    /// Evaluates the transformation pipeline with directory awareness and per-item modified timestamps (`UI-07`).
    pub fn preview_with_metadata(
        &self,
        items: &[BatchRenameInputItem],
        existing_directory_names: &HashSet<String>,
    ) -> Vec<RenamePreviewItem> {
        let norm_key = |s: &str| -> String {
            if self.case_insensitive_fs {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        };
        let original_set: HashSet<String> = items.iter().map(|it| norm_key(&it.name)).collect();
        let existing_set: HashSet<String> = existing_directory_names
            .iter()
            .map(|n| norm_key(n))
            .collect();
        let mut proposed_list = Vec::with_capacity(items.len());
        let mut counts: HashMap<String, usize> = HashMap::new();

        for (index, item) in items.iter().enumerate() {
            let proposed = self.apply_to_entry_with_date(
                &item.name,
                index,
                item.is_dir,
                item.modified_timestamp.as_deref(),
            );
            *counts.entry(norm_key(&proposed)).or_insert(0) += 1;
            proposed_list.push((item.name.clone(), proposed));
        }

        proposed_list
            .into_iter()
            .map(|(original_name, proposed_name)| {
                let is_invalid = is_invalid_proposed_filename(&proposed_name);
                let prop_key = norm_key(&proposed_name);
                let orig_key = norm_key(&original_name);
                let duplicate_in_batch = counts.get(&prop_key).copied().unwrap_or(0) > 1;
                let collides_with_unselected = prop_key != orig_key
                    && existing_set.contains(&prop_key)
                    && !original_set.contains(&prop_key);
                RenamePreviewItem {
                    original_name,
                    proposed_name,
                    has_collision: is_invalid || duplicate_in_batch || collides_with_unselected,
                }
            })
            .collect()
    }

    pub fn apply_to_single_entry(&self, filename: &str, index: usize, is_dir: bool) -> String {
        self.apply_to_entry_with_date(filename, index, is_dir, None)
    }

    pub fn apply_to_entry_with_date(
        &self,
        filename: &str,
        index: usize,
        is_dir: bool,
        item_modified_timestamp: Option<&str>,
    ) -> String {
        let mut current = filename.to_string();

        for rule in &self.rules {
            let effective_scope = |s: RenameTargetScope| -> Option<RenameTargetScope> {
                if is_dir {
                    match s {
                        RenameTargetScope::Extension => None,
                        RenameTargetScope::Name | RenameTargetScope::NameWithExtension => {
                            Some(RenameTargetScope::NameWithExtension)
                        }
                    }
                } else {
                    Some(s)
                }
            };
            match rule {
                RenameRule::TextReplace {
                    find,
                    replace,
                    case_sensitive,
                } => {
                    if let Some(sc) = effective_scope(RenameTargetScope::Name) {
                        current = apply_scoped_transform(&current, sc, |part| {
                            apply_text_replace(
                                part,
                                ReplaceOccurrences::All,
                                find,
                                replace,
                                *case_sensitive,
                            )
                        });
                    }
                }
                RenameRule::DigitCaptureReplace {
                    prefix,
                    replacement_template,
                } => {
                    if let Some(sc) = effective_scope(RenameTargetScope::Name) {
                        current = apply_scoped_transform(&current, sc, |stem| {
                            if let Some(rest) = stem.strip_prefix(prefix) {
                                let digits: String =
                                    rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                                let tail: String =
                                    rest.chars().skip_while(|c| c.is_ascii_digit()).collect();
                                if !digits.is_empty() {
                                    let expanded = replacement_template.replace("$1", &digits);
                                    return format!("{expanded}{tail}");
                                }
                            }
                            stem.to_string()
                        });
                    }
                }
                RenameRule::CaseConversion(style) => {
                    if let Some(sc) = effective_scope(RenameTargetScope::Name) {
                        current = apply_scoped_transform(&current, sc, |part| {
                            apply_case_style(part, *style)
                        });
                    }
                }
                RenameRule::IncrementalCounter {
                    prefix,
                    suffix,
                    start,
                    step,
                    padding,
                } => {
                    if let Some(sc) = effective_scope(RenameTargetScope::Name) {
                        let num = start + index * step;
                        let formatted_num = format!("{num:0width$}", width = *padding);
                        current = apply_scoped_transform(&current, sc, |stem| {
                            format!("{prefix}{stem}_{formatted_num}{suffix}")
                        });
                    }
                }
                RenameRule::DateTimeInjection {
                    prefix,
                    formatted_timestamp,
                } => {
                    if let Some(sc) = effective_scope(RenameTargetScope::Name) {
                        let ts = resolve_date_token(formatted_timestamp, item_modified_timestamp);
                        current = apply_scoped_transform(&current, sc, |stem| {
                            format!("{prefix}{stem}_{ts}")
                        });
                    }
                }
                RenameRule::ScopedReplaceText {
                    scope,
                    occurrences,
                    find,
                    replace,
                    case_sensitive,
                } => {
                    if let Some(sc) = effective_scope(*scope) {
                        current = apply_scoped_transform(&current, sc, |part| {
                            apply_text_replace(part, *occurrences, find, replace, *case_sensitive)
                        });
                    }
                }
                RenameRule::ScopedChangeCase { scope, style } => {
                    if let Some(sc) = effective_scope(*scope) {
                        current = apply_scoped_transform(&current, sc, |part| {
                            apply_case_style(part, *style)
                        });
                    }
                }
                RenameRule::ScopedAddSequence {
                    scope,
                    position,
                    prefix,
                    start,
                    step,
                    padding,
                } => {
                    if let Some(sc) = effective_scope(*scope) {
                        let num = start + index * step;
                        let formatted_num = format!("{num:0width$}", width = (*padding).max(1));
                        let token = format!("{prefix}{formatted_num}");
                        current = apply_scoped_transform(&current, sc, |part| match position {
                            TextInsertionPosition::AfterName => format!("{part}{token}"),
                            TextInsertionPosition::BeforeName => format!("{token}{part}"),
                            TextInsertionPosition::ReplaceName => token,
                        });
                    }
                }
                RenameRule::ScopedAddText {
                    scope,
                    position,
                    text,
                } => {
                    if let Some(sc) = effective_scope(*scope) {
                        current = apply_scoped_transform(&current, sc, |part| match position {
                            TextInsertionPosition::AfterName => format!("{part}{text}"),
                            TextInsertionPosition::BeforeName => format!("{text}{part}"),
                            TextInsertionPosition::ReplaceName => text.clone(),
                        });
                    }
                }
                RenameRule::ScopedAddDate {
                    scope,
                    position,
                    formatted_timestamp,
                } => {
                    if let Some(sc) = effective_scope(*scope) {
                        let ts = resolve_date_token(formatted_timestamp, item_modified_timestamp);
                        current = apply_scoped_transform(&current, sc, |part| match position {
                            TextInsertionPosition::AfterName => {
                                if part.is_empty() {
                                    ts.to_string()
                                } else {
                                    format!("{part}_{ts}")
                                }
                            }
                            TextInsertionPosition::BeforeName => {
                                if part.is_empty() {
                                    ts.to_string()
                                } else {
                                    format!("{ts}_{part}")
                                }
                            }
                            TextInsertionPosition::ReplaceName => ts.to_string(),
                        });
                    }
                }
            }
        }

        current
    }
}

fn resolve_date_token<'a>(rule_ts: &'a str, item_modified_ts: Option<&'a str>) -> &'a str {
    if let Some(fallback) = rule_ts.strip_prefix(FILE_MODIFIED_TOKEN_PREFIX) {
        if let Some(ts) = item_modified_ts.filter(|s| !s.is_empty()) {
            if fallback.len() == 10 && ts.len() >= 10 {
                return &ts[..10];
            }
            return ts;
        }
        fallback
    } else {
        rule_ts
    }
}

fn is_invalid_proposed_filename(name: &str) -> bool {
    if name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
    {
        return true;
    }
    if cfg!(windows) {
        let has_win_invalid = name
            .chars()
            .any(|c| matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control());
        if name.ends_with(' ') || name.ends_with('.') || has_win_invalid {
            return true;
        }
        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        if matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        ) {
            return true;
        }
    }
    false
}

fn split_stem_and_ext(filename: &str) -> (&str, Option<&str>) {
    let path = Path::new(filename);
    let ext = path.extension().and_then(|e| e.to_str());
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    (stem, ext)
}

fn apply_scoped_transform<F>(filename: &str, scope: RenameTargetScope, transform: F) -> String
where
    F: FnOnce(&str) -> String,
{
    match scope {
        RenameTargetScope::NameWithExtension => transform(filename),
        RenameTargetScope::Name => {
            let (stem, ext) = split_stem_and_ext(filename);
            let new_stem = transform(stem);
            match ext {
                Some(e) if !e.is_empty() => format!("{new_stem}.{e}"),
                _ => new_stem,
            }
        }
        RenameTargetScope::Extension => {
            let (stem, ext) = split_stem_and_ext(filename);
            let new_ext = transform(ext.unwrap_or(""));
            let trimmed_ext = new_ext.trim_start_matches('.');
            if trimmed_ext.is_empty() {
                stem.to_string()
            } else {
                format!("{stem}.{trimmed_ext}")
            }
        }
    }
}

fn apply_case_style(input: &str, style: CaseConversionStyle) -> String {
    match style {
        CaseConversionStyle::UpperCase => input.to_uppercase(),
        CaseConversionStyle::LowerCase => input.to_lowercase(),
        CaseConversionStyle::TitleCase => input.to_title_case(),
        CaseConversionStyle::CamelCase => input.to_lower_camel_case(),
        CaseConversionStyle::KebabCase => input.to_kebab_case(),
        CaseConversionStyle::SnakeCase => input.to_snake_case(),
    }
}

fn build_bounded_regex(pattern: &str, case_insensitive: bool) -> Option<regex::Regex> {
    if pattern.is_empty() || pattern.len() > MAX_RENAME_REGEX_PATTERN_LEN {
        return None;
    }
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_COMPILE_LIMIT_BYTES)
        .dfa_size_limit(REGEX_COMPILE_LIMIT_BYTES)
        .build()
        .ok()
}

fn apply_text_replace(
    target: &str,
    occurrences: ReplaceOccurrences,
    find: &str,
    replace: &str,
    case_sensitive: bool,
) -> String {
    if find.is_empty() {
        return target.to_string();
    }
    match occurrences {
        ReplaceOccurrences::All => {
            if case_sensitive {
                target.replace(find, replace)
            } else {
                replace_case_insensitive(target, find, replace)
            }
        }
        ReplaceOccurrences::First => {
            if case_sensitive {
                target.replacen(find, replace, 1)
            } else {
                replace_first_case_insensitive(target, find, replace)
            }
        }
        ReplaceOccurrences::Last => replace_last(target, find, replace, case_sensitive),
        ReplaceOccurrences::RegularExpression => {
            let norm_rep = normalize_regex_replacement(replace);
            build_bounded_regex(find, !case_sensitive)
                .map(|re| re.replace_all(target, norm_rep.as_str()).into_owned())
                .unwrap_or_else(|| target.to_string())
        }
    }
}

fn normalize_regex_replacement(replace: &str) -> String {
    let mut out = String::with_capacity(replace.len() + 8);
    let mut chars = replace.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            if let Some(&next_ch) = chars.peek() {
                if next_ch.is_ascii_digit() {
                    let digit = chars.next().unwrap();
                    out.push_str("${");
                    out.push(digit);
                    out.push('}');
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let lower_h = haystack.to_lowercase();
    let lower_n = needle.to_lowercase();
    if lower_n.is_empty() || lower_h.len() != haystack.len() || lower_n.len() != needle.len() {
        return build_bounded_regex(&regex::escape(needle), true)
            .map(|re| {
                re.replace_all(haystack, regex::NoExpand(replacement))
                    .into_owned()
            })
            .unwrap_or_else(|| haystack.to_string());
    }
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

fn replace_first_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    build_bounded_regex(&regex::escape(needle), true)
        .map(|re| {
            re.replace(haystack, regex::NoExpand(replacement))
                .into_owned()
        })
        .unwrap_or_else(|| haystack.to_string())
}

fn replace_last(haystack: &str, needle: &str, replacement: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        if let Some(pos) = haystack.rfind(needle) {
            let mut out = String::with_capacity(haystack.len() + replacement.len());
            out.push_str(&haystack[..pos]);
            out.push_str(replacement);
            out.push_str(&haystack[pos + needle.len()..]);
            return out;
        }
        return haystack.to_string();
    }
    let Some(re) = build_bounded_regex(&regex::escape(needle), true) else {
        return haystack.to_string();
    };
    if let Some(m) = re.find_iter(haystack).last() {
        let mut out = String::with_capacity(haystack.len() + replacement.len());
        out.push_str(&haystack[..m.start()]);
        out.push_str(replacement);
        out.push_str(&haystack[m.end()..]);
        out
    } else {
        haystack.to_string()
    }
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

    #[test]
    fn test_scoped_regex_replace_and_multi_rule_pipeline() {
        let renamer = BatchRenamer::new(vec![RenameRule::ScopedReplaceText {
            scope: RenameTargetScope::NameWithExtension,
            occurrences: ReplaceOccurrences::RegularExpression,
            find: "100.*".to_string(),
            replace: "image".to_string(),
            case_sensitive: false,
        }]);
        let names = [
            "manifold_fig3.png".to_string(),
            "100baset hub.jpg".to_string(),
            "100baset hub copy.jpg".to_string(),
            "10700.jpg".to_string(),
        ];
        let existing = HashSet::new();
        let preview = renamer.preview(&names, &existing);
        assert_eq!(preview[0].proposed_name, "manifold_fig3.png");
        assert!(!preview[0].has_collision);
        assert_eq!(preview[1].proposed_name, "image");
        assert!(preview[1].has_collision);
        assert_eq!(preview[2].proposed_name, "image");
        assert!(preview[2].has_collision);
        assert_eq!(preview[3].proposed_name, "10700.jpg");
        assert!(!preview[3].has_collision);

        let seq_renamer = BatchRenamer::new(vec![
            RenameRule::ScopedReplaceText {
                scope: RenameTargetScope::Name,
                occurrences: ReplaceOccurrences::RegularExpression,
                find: r"^(\d+)\s+(.*)".to_string(),
                replace: "$2_$1".to_string(),
                case_sensitive: false,
            },
            RenameRule::ScopedAddSequence {
                scope: RenameTargetScope::Name,
                position: TextInsertionPosition::AfterName,
                prefix: "_v".to_string(),
                start: 1,
                step: 1,
                padding: 2,
            },
        ]);
        let seq_names = [
            "3174 cluster.jpg".to_string(),
            "3200 router.jpg".to_string(),
        ];
        let seq_preview = seq_renamer.preview(&seq_names, &existing);
        assert_eq!(seq_preview[0].proposed_name, "cluster_3174_v01.jpg");
        assert!(!seq_preview[0].has_collision);
        assert_eq!(seq_preview[1].proposed_name, "router_3200_v02.jpg");
        assert!(!seq_preview[1].has_collision);

        // Verify case-insensitive collision detection & directory dot preservation (BUG-06)
        let ci_renamer = BatchRenamer::new(vec![RenameRule::ScopedAddText {
            scope: RenameTargetScope::Name,
            position: TextInsertionPosition::AfterName,
            text: "_tag".to_string(),
        }])
        .with_case_insensitive(true);
        let dir_items = [
            ("v1.2.release".to_string(), true),
            ("Photo.jpg".to_string(), false),
        ];
        let existing_ci = HashSet::from(["PHOTO_TAG.JPG".to_string()]);
        let ci_prev = ci_renamer.preview_with_dirs(&dir_items, &existing_ci);
        assert_eq!(ci_prev[0].proposed_name, "v1.2.release_tag");
        assert!(!ci_prev[0].has_collision);
        assert_eq!(ci_prev[1].proposed_name, "Photo_tag.jpg");
        assert!(ci_prev[1].has_collision);
    }

    #[test]
    fn test_recursive_directory_sync_subfolders() {
        let left = ArchiveBackend::new("left.zip", ArchiveFormat::Zip);
        let right = ArchiveBackend::new("right.zip", ArchiveFormat::Zip);

        left.write_bytes("/sub/nested/app.rs", b"fn main() {}")
            .unwrap();
        right.write_bytes("/sub/old.txt", b"remove_me").unwrap();

        let plan = SyncExecutionPlan::build(
            &left,
            "/",
            &right,
            "/",
            SyncStrategy::LeftToRight,
            ComparisonMode::ContentHash,
        )
        .unwrap();
        let applied = plan.execute(&left, &right).unwrap();
        assert!(applied >= 2);
        assert_eq!(
            right.read_bytes("/sub/nested/app.rs").unwrap(),
            b"fn main() {}"
        );
        assert!(right.metadata("/sub/old.txt").is_err());

        // Verify directory replacing a conflicting regular file + per-item modified dates + regex cap
        let left2 = ArchiveBackend::new("l2.zip", ArchiveFormat::Zip);
        let right2 = ArchiveBackend::new("r2.zip", ArchiveFormat::Zip);
        left2.write_bytes("/swap/inner.txt", b"hello").unwrap();
        right2.write_bytes("/swap", b"was_a_file").unwrap();
        let plan2 = SyncExecutionPlan::build(
            &left2,
            "/",
            &right2,
            "/",
            SyncStrategy::LeftToRight,
            ComparisonMode::Metadata,
        )
        .unwrap();
        plan2.execute(&left2, &right2).unwrap();
        assert_eq!(right2.read_bytes("/swap/inner.txt").unwrap(), b"hello");

        let mod_renamer = BatchRenamer::new(vec![
            RenameRule::ScopedChangeCase {
                scope: RenameTargetScope::Extension,
                style: CaseConversionStyle::LowerCase,
            },
            RenameRule::ScopedAddDate {
                scope: RenameTargetScope::Name,
                position: TextInsertionPosition::AfterName,
                formatted_timestamp: format!("{FILE_MODIFIED_TOKEN_PREFIX}2026-01-01_00-00"),
            },
        ]);
        let meta_items = [
            BatchRenameInputItem {
                name: "Release.V2".to_string(),
                is_dir: true,
                modified_timestamp: Some("2025-07-04_14-30".to_string()),
            },
            BatchRenameInputItem {
                name: "Report.PDF".to_string(),
                is_dir: false,
                modified_timestamp: Some("2026-11-10_09-15".to_string()),
            },
        ];
        let meta_prev = mod_renamer.preview_with_metadata(&meta_items, &HashSet::new());
        assert_eq!(meta_prev[0].proposed_name, "Release.V2_2025-07-04_14-30");
        assert_eq!(meta_prev[1].proposed_name, "Report_2026-11-10_09-15.pdf");

        let huge_pat = "a".repeat(MAX_RENAME_REGEX_PATTERN_LEN + 10);
        let huge_renamer = BatchRenamer::new(vec![RenameRule::ScopedReplaceText {
            scope: RenameTargetScope::Name,
            occurrences: ReplaceOccurrences::RegularExpression,
            find: huge_pat,
            replace: "x".to_string(),
            case_sensitive: false,
        }]);
        assert_eq!(
            huge_renamer.apply_to_single_entry("aaa.txt", 0, false),
            "aaa.txt"
        );
    }
}
