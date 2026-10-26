//! Inspector & Quick Look Preview Engine (`manifold-preview`) for Project Manifold.
//!
//! Models the 3-tab Right Inspector Drawer (`ⓘ Info | ⇅ Activities | ⚠ Logs`) shown in
//! `doc/image1.png` and `doc/image2.png`, including syntax-highlighted code lines, image
//! pan/zoom, PDF/media descriptors, Hex View formatting, and the `General` and `Permissions`
//! collapsible sections.

use std::path::Path;

use manifold_vfs::{FileMetadata, PermissionsMatrix};
use serde::{Deserialize, Serialize};

/// Top 3-tab switcher in the Right Inspector Drawer (`doc/image1.png` & `doc/image2.png`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum InspectorTab {
    /// `ⓘ` Info & Preview Inspector.
    #[default]
    Info,
    /// `⇅` Active & Queued File Transfers (`Activities`).
    Activities,
    /// `⚠` Diagnostics, Warnings & Error Logs.
    Logs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightedLine {
    pub line_number: usize,
    pub language: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PreviewPayload {
    TextCode {
        language: String,
        lines: Vec<HighlightedLine>,
    },
    Image {
        format: String,
        width: u32,
        height: u32,
        zoom: f32,
        pan_x: f32,
        pan_y: f32,
    },
    AudioVideo {
        media_kind: String,
        duration_secs: u64,
        is_playing: bool,
    },
    Pdf {
        page_count: u32,
        current_page: u32,
    },
    HexDump {
        formatted_rows: Vec<String>,
    },
    Folder {
        item_count: usize,
        total_bytes: Option<u64>,
    },
}

/// `General` collapsible section in the Right Inspector (`doc/image1.png` & `doc/image2.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralInspectorSection {
    pub expanded: bool,
    pub path: String,
    pub size: String,
    pub modified: String,
    pub created: String,
    pub added: String,
    pub extension: String,
    pub hide_extension: bool,
}

/// `Permissions` collapsible section in the Right Inspector (`doc/image1.png` & `doc/image2.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsInspectorSection {
    pub expanded: bool,
    pub matrix: PermissionsMatrix,
    pub octal_symbolic: String,
    pub octal_code: String,
    pub owner: String,
    pub group: String,
    pub locked: bool,
}

/// Complete view-model for the `ⓘ` Info tab of the Right Inspector Drawer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInspectorViewModel {
    pub title: String,
    pub subtitle: String,
    pub preview: PreviewPayload,
    pub general: GeneralInspectorSection,
    pub permissions: PermissionsInspectorSection,
}

impl FileInspectorViewModel {
    /// Builds the exact Inspector view-model shown in `doc/image1.png` & `doc/image2.png`
    /// from a [`FileMetadata`] entry and optional sample bytes.
    pub fn from_metadata(
        meta: &FileMetadata,
        formatted_timestamp: &str,
        sample_bytes: Option<&[u8]>,
    ) -> Self {
        let size_str = meta.formatted_size();
        let subtitle = format!("{} - {}", meta.kind, size_str);
        let parent_dir = Path::new(&meta.path)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("/")
            .to_string();

        let matrix = meta.permissions_matrix();
        let preview = Self::build_preview_payload(meta, sample_bytes);
        let default_owner = || "johnappleseed".to_string();
        let owner = meta.owner.clone().unwrap_or_else(default_owner);
        let group = meta.group.clone().unwrap_or_else(|| "staff".to_string());

        Self {
            title: meta.display_name(),
            subtitle,
            preview,
            general: GeneralInspectorSection {
                expanded: true,
                path: parent_dir,
                size: size_str,
                modified: formatted_timestamp.to_string(),
                created: formatted_timestamp.to_string(),
                added: formatted_timestamp.to_string(),
                extension: meta.extension.clone().unwrap_or_default(),
                hide_extension: meta.hide_extension,
            },
            permissions: PermissionsInspectorSection {
                expanded: true,
                matrix,
                octal_symbolic: matrix.to_symbolic_string(),
                octal_code: matrix.to_octal_string(),
                owner,
                group,
                locked: meta.is_locked,
            },
        }
    }

    fn build_preview_payload(meta: &FileMetadata, sample_bytes: Option<&[u8]>) -> PreviewPayload {
        if meta.is_dir {
            return PreviewPayload::Folder {
                item_count: 0,
                total_bytes: meta.size,
            };
        }
        match meta.extension.as_deref() {
            Some("jpg" | "jpeg" | "png" | "svg" | "webp" | "avif") => PreviewPayload::Image {
                format: meta.kind.clone(),
                width: 1920,
                height: 1080,
                zoom: 1.0,
                pan_x: 0.0,
                pan_y: 0.0,
            },
            Some("pdf") => PreviewPayload::Pdf {
                page_count: 1,
                current_page: 1,
            },
            Some("mp3" | "wav" | "flac" | "ogg" | "mp4" | "mkv" | "mov" | "webm") => {
                PreviewPayload::AudioVideo {
                    media_kind: meta.kind.clone(),
                    duration_secs: 120,
                    is_playing: false,
                }
            }
            Some("rs" | "php" | "toml" | "html" | "txt" | "md" | "json") => {
                let utf8 = sample_bytes.and_then(|b| std::str::from_utf8(b).ok());
                let text = utf8.unwrap_or("");
                let lang = meta.kind.clone();
                let lines = text
                    .lines()
                    .enumerate()
                    .map(|(idx, line)| HighlightedLine {
                        line_number: idx + 1,
                        language: lang.clone(),
                        text: line.to_string(),
                    })
                    .collect();
                PreviewPayload::TextCode {
                    language: lang,
                    lines,
                }
            }
            _ => {
                let bytes = sample_bytes.unwrap_or(&[]);
                PreviewPayload::HexDump {
                    formatted_rows: format_hex_dump(bytes, 16),
                }
            }
        }
    }
}

/// Formats binary data into standard 16-byte Hex + ASCII inspector rows (`Section 2.1.5` of `BRIEF.md`).
pub fn format_hex_dump(bytes: &[u8], max_rows: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for (row_idx, chunk) in bytes.chunks(16).take(max_rows).enumerate() {
        let offset = row_idx * 16;
        let mut hex_part = String::with_capacity(48);
        let mut ascii_part = String::with_capacity(16);
        for &b in chunk {
            hex_part.push_str(&format!("{b:02x} "));
            let ch = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            };
            ascii_part.push(ch);
        }
        for _ in chunk.len()..16 {
            hex_part.push_str("   ");
        }
        rows.push(format!("{offset:08x}: {hex_part}| {ascii_part}"));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifold_vfs::ColorLabel;

    #[test]
    fn test_inspector_view_model_matches_waterfall_jpg_in_image1_and_image2() {
        let meta = FileMetadata {
            name: "Waterfall.jpg".to_string(),
            path: "/Users/johnappleseed/Photos/Waterfall.jpg".to_string(),
            extension: Some("jpg".to_string()),
            kind: "JPEG image".to_string(),
            size: Some(9_751_756), // ~9,3 MB
            is_dir: false,
            is_symlink: false,
            is_hidden: false,
            is_locked: false,
            hide_extension: false,
            color_label: ColorLabel::None,
            modified: None,
            created: None,
            added: None,
            permissions: 0o644,
            owner: Some("johnappleseed".to_string()),
            group: Some("staff".to_string()),
        };

        let vm = FileInspectorViewModel::from_metadata(&meta, "2023.08.31, 14:57", None);
        assert_eq!(vm.title, "Waterfall.jpg");
        assert_eq!(vm.subtitle, "JPEG image - 9,3 MB");
        assert_eq!(vm.general.path, "/Users/johnappleseed/Photos");
        assert_eq!(vm.general.size, "9,3 MB");
        assert_eq!(vm.general.extension, "jpg");
        assert!(!vm.general.hide_extension);
        assert_eq!(vm.permissions.octal_symbolic, "rw-r--r--");
        assert_eq!(vm.permissions.octal_code, "644");
        assert_eq!(vm.permissions.owner, "johnappleseed");
        assert_eq!(vm.permissions.group, "staff");
        assert!(!vm.permissions.locked);
    }

    #[test]
    fn test_hex_dump_formatter() {
        let data = b"Manifold VFS Hex Inspector";
        let dump = format_hex_dump(data, 4);
        assert_eq!(dump.len(), 2);
        assert!(dump[0].starts_with("00000000: 4d 61 6e 69 66 6f 6c 64"));
        assert!(dump[0].ends_with("| Manifold VFS Hex"));
    }
}
