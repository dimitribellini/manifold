//! Dual-Pane UI State Store, Virtualized 500k+ Row Viewport, and Modal Controllers
//! (`manifold-ui`) matching `doc/image1.png` through `doc/image4.png`.

use std::collections::HashSet;
use std::path::Path;

use manifold_config::{
    ConnectionProfile, ConnectionsConfig, PaneViewMode, ProtocolType, SortColumn, SortDirection,
};
use manifold_preview::{FileInspectorViewModel, InspectorTab};
use manifold_transfer::ConflictModalDescriptor;
use manifold_vfs::{ColorLabel, FileMetadata, FileSystemBackend, LocalFsBackend, VolumeStats};
use serde::{Deserialize, Serialize};

pub mod gui;

/// Exact color tokens audited from `doc/image1.png` – `doc/image4.png`.
pub mod theme {
    pub const WINDOW_BG_HEX: &str = "#1E1E1E";
    pub const SIDEBAR_BG_HEX: &str = "#282A2C";
    pub const INSPECTOR_BG_HEX: &str = "#282A2C";
    pub const HAIRLINE_DIVIDER_HEX: &str = "#38383A";
    pub const ACTIVE_PANE_TOP_BAR_HEX: &str = "#4CD964";
    pub const SELECTED_LABEL_PILL_HEX: &str = "#0A84FF";
    pub const POLAROID_BORDER_HEX: &str = "#FFFFFF";
    pub const MODAL_CARD_BG_HEX: &str = "#262628";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ActivePane {
    Left,
    #[default]
    Right,
}

/// Pane layout state machine (`Section 2` of `doc/specification_1.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PanesLayout {
    #[default]
    Single,
    SplitHorizontal,
    SplitVertical,
}

/// Dynamic root volume label resolution (`/` on Unix, `C:` on Windows).
pub fn resolve_dynamic_root_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "C:"
    } else {
        "/"
    }
}

#[cfg(target_os = "linux")]
fn linux_network_mounts(mountinfo: &str) -> Vec<(String, String)> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let fields: Vec<_> = before.split_whitespace().collect();
            let fs = after.split_whitespace().next()?;
            if !matches!(
                fs,
                "nfs" | "nfs4" | "cifs" | "smb3" | "sshfs" | "fuse.sshfs"
            ) {
                return None;
            }
            let path = fields
                .get(4)?
                .replace("\\040", " ")
                .replace("\\011", "\\t")
                .replace("\\134", "\\");
            let label = Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&path)
                .to_string();
            Some((label, path))
        })
        .collect()
}

/// Top Unified Toolbar model (`doc/image1.png` & `doc/image2.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolbarModel {
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub active_folder_title: String,
    pub show_hidden_files: bool,
    pub active_view_mode: PaneViewMode,
    pub search_query: String,
}

/// Sidebar item (`Devices`, `Connections`, `Shares`, `Favorites`, `Work` in `doc/image1.png` & `doc/image2.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarItem {
    pub id: String,
    pub label: String,
    pub vfs_url: String,
    pub icon_kind: String,
    /// Optional right-aligned capacity badge (e.g. `"354,54 GB"` or `"470,07 GB"`).
    pub capacity_badge: Option<String>,
    /// Whether an eject/disconnect icon (`⏏`) is displayed next to the item.
    pub can_eject: bool,
    /// Optional child shares (e.g., `Binarynights` -> `Office`, `Repositories`).
    pub children: Vec<SidebarItem>,
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarSection {
    pub title: String,
    pub collapsed: bool,
    pub items: Vec<SidebarItem>,
}

/// Combined Path and Status Header rendered at the TOP of each pane (`doc/image1.png` & `doc/image2.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathAndStatusHeader {
    pub root_icon: String,
    pub breadcrumb_segments: Vec<String>,
    pub formatted_breadcrumbs: String,
    pub status_subtitle: String,
}

impl PathAndStatusHeader {
    /// Builds the exact combined top header shown in `doc/image1.png` and `doc/image2.png`:
    /// - Unselected: `"19 items, 354,54 GB available"`
    /// - Selected: `"1 of 9 selected (9,3 MB), 353,04 GB available"`
    pub fn build(
        volume_name: Option<&str>,
        path: &str,
        total_items: usize,
        selected_items: &[&FileMetadata],
        volume_stats: Option<VolumeStats>,
    ) -> Self {
        let mut segments = Vec::new();
        if let Some(vol) = volume_name {
            let has_vol = !vol.is_empty();
            if has_vol {
                segments.push(vol.to_string());
            }
        }
        for part in path.split('/').filter(|s| !s.is_empty()) {
            segments.push(part.to_string());
        }
        let formatted_breadcrumbs = segments.join(" > ");

        let avail_suffix = match volume_stats {
            Some(v) => format!(", {} available", v.format_available_gb()),
            None => String::new(),
        };

        let status_subtitle = if selected_items.is_empty() {
            match volume_stats {
                Some(v) => format!("{total_items} items, {} available", v.format_available_gb()),
                None => format!("{total_items} items"),
            }
        } else {
            let selected_bytes: u64 = selected_items.iter().filter_map(|m| m.size).sum();
            let formatted_sel_size = manifold_vfs::format_bytes_comma(selected_bytes);
            format!(
                "{} of {} selected ({}){}",
                selected_items.len(),
                total_items,
                formatted_sel_size,
                avail_suffix
            )
        };

        Self {
            root_icon: if volume_name.is_some() {
                "drive".to_string()
            } else {
                "home".to_string()
            },
            breadcrumb_segments: segments,
            formatted_breadcrumbs,
            status_subtitle,
        }
    }

    /// Computes clickable `(segment_label, target_path)` pairs for the Dual-Mode Path Bar.
    pub fn clickable_segments(root_label: &str, path: &str) -> Vec<(String, String)> {
        if let Some((scheme, rest)) = path.split_once("://") {
            let (authority, subpath) = match rest.split_once('/') {
                Some((auth, sub)) => (auth, sub),
                None => (rest, ""),
            };
            let prefix = format!("{scheme}://{authority}");
            let mut nodes = vec![(authority.to_string(), format!("{prefix}/"))];
            let mut accum = String::new();
            for part in subpath.split('/').filter(|s| !s.is_empty()) {
                accum.push('/');
                accum.push_str(part);
                nodes.push((part.to_string(), format!("{prefix}{accum}")));
            }
            return nodes;
        }
        let mut nodes = vec![(root_label.to_string(), "/".to_string())];
        let mut accum = String::new();
        for part in path.split('/').filter(|s| !s.is_empty()) {
            accum.push('/');
            accum.push_str(part);
            nodes.push((part.to_string(), accum.clone()));
        }
        nodes
    }
}

/// Converts a [`ConnectionProfile`] into its canonical VFS URI (e.g. `sftp://root@192.168.8.110:22/root`).
pub fn profile_to_vfs_url(profile: &ConnectionProfile) -> String {
    let scheme = match profile.protocol {
        ProtocolType::Sftp => "sftp",
        ProtocolType::Ftp => "ftp",
        ProtocolType::Ftps => "ftps",
        ProtocolType::S3 => "s3",
        ProtocolType::GoogleDrive => "gdrive",
        ProtocolType::Gcs => "gcs",
        ProtocolType::AzureBlob => "azure",
        ProtocolType::WebDav => "webdav",
        ProtocolType::Smb => "smb",
    };
    let host = profile
        .host
        .as_deref()
        .or(profile.bucket.as_deref())
        .unwrap_or("localhost");
    let user_prefix = if profile.username.trim().is_empty() {
        String::new()
    } else {
        format!("{}@", profile.username.trim())
    };
    let port_suffix = match profile.port {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let clean_path = profile.base_path.trim().trim_start_matches('/');
    if clean_path.is_empty() {
        format!("{scheme}://{user_prefix}{host}{port_suffix}/")
    } else {
        format!("{scheme}://{user_prefix}{host}{port_suffix}/{clean_path}")
    }
}

/// Flattened row in the virtualized List View (supports inline expandable folders `> 📁 admin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualizedListRow {
    pub metadata: FileMetadata,
    pub depth: usize,
    pub is_expanded: bool,
    pub is_selected: bool,
}

/// High-performance `O(1)` virtualized table model capable of slicing 500,000+ entries
/// without blocking the UI thread (`Phase 3` of `BRIEF.md`).
#[derive(Debug, Clone, Default)]
pub struct VirtualizedTableViewport {
    rows: Vec<VirtualizedListRow>,
    row_height_px: f64,
}

impl VirtualizedTableViewport {
    pub fn new(rows: Vec<VirtualizedListRow>, row_height_px: f64) -> Self {
        Self {
            rows,
            row_height_px: row_height_px.max(16.0),
        }
    }

    pub fn total_rows(&self) -> usize {
        self.rows.len()
    }

    pub fn total_content_height_px(&self) -> f64 {
        (self.rows.len() as f64) * self.row_height_px
    }

    /// Computes the visible row range `[start_idx..end_idx]` in `O(1)` time for a given scroll offset
    /// and viewport height (plus overscan buffer), ensuring 60/120 FPS even at 500,000+ entries.
    pub fn visible_range(
        &self,
        scroll_offset_px: f64,
        viewport_height_px: f64,
        overscan_rows: usize,
    ) -> std::ops::Range<usize> {
        if self.rows.is_empty() {
            return 0..0;
        }
        let first_visible = (scroll_offset_px.max(0.0) / self.row_height_px).floor() as usize;
        let visible_count = (viewport_height_px.max(0.0) / self.row_height_px).ceil() as usize + 1;
        let start = first_visible
            .saturating_sub(overscan_rows)
            .min(self.rows.len());
        let end = (first_visible + visible_count + overscan_rows).min(self.rows.len());
        start..end
    }

    pub fn visible_slice(
        &self,
        scroll_offset_px: f64,
        viewport_height_px: f64,
        overscan_rows: usize,
    ) -> &[VirtualizedListRow] {
        let range = self.visible_range(scroll_offset_px, viewport_height_px, overscan_rows);
        &self.rows[range]
    }
}

/// Tab descriptor inside a Pane's Tab Bar (`doc/image2.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneTabModel {
    pub id: String,
    pub title: String,
    pub icon_kind: String,
    pub vfs_url: String,
    pub current_path: String,
    pub view_mode: PaneViewMode,
    pub sort_column: SortColumn,
    pub sort_direction: SortDirection,
    pub inline_filter: String,
    pub expanded_folders: HashSet<String>,
    pub selected_paths: HashSet<String>,
}

/// SFTP / Cloud Connection Dialog model (`doc/image3.png`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectDialogModel {
    pub is_open: bool,
    pub title: String,
    pub protocol: ProtocolType,
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password_masked: String,
    pub key_path: Option<String>,
    pub remote_path: String,
    pub local_path: Option<String>,
    pub base_url: String,
    pub text_encoding: String,
    pub note: String,
    pub color_label: ColorLabel,
    pub color_swatches: [ColorLabel; 8],
    pub show_advanced: bool,
}

impl Default for ConnectDialogModel {
    fn default() -> Self {
        Self::new_sftp_sheet()
    }
}

impl ConnectDialogModel {
    /// Constructs the exact SFTP Connect Dialog state shown in `doc/image3.png`.
    pub fn new_sftp_sheet() -> Self {
        Self {
            is_open: true,
            title: "SFTP".to_string(),
            protocol: ProtocolType::Sftp,
            server: "johnappleseed.com".to_string(),
            port: 22,
            username: "admin".to_string(),
            password_masked: "••••••••".to_string(),
            key_path: None,
            remote_path: "/User/Shared/Website".to_string(),
            local_path: None,
            base_url: "https://johnappleseed.com/".to_string(),
            text_encoding: "Unicode (UTF-8)".to_string(),
            note: String::new(),
            color_label: ColorLabel::Purple,
            color_swatches: ColorLabel::all_swatches(),
            show_advanced: true,
        }
    }

    pub fn to_connection_profile(&self, id: impl Into<String>) -> ConnectionProfile {
        ConnectionProfile {
            id: id.into(),
            name: format!("{} ({})", self.server, self.protocol.display_name()),
            group: Some("Favorites".to_string()),
            protocol: self.protocol,
            host: Some(self.server.clone()),
            port: Some(self.port),
            endpoint: None,
            bucket: None,
            region: None,
            username: self.username.clone(),
            auth_type: if self.key_path.is_some() {
                manifold_config::AuthType::PublicKey
            } else {
                manifold_config::AuthType::Password
            },
            key_path: self.key_path.clone(),
            base_path: self.remote_path.clone(),
            local_path: self.local_path.clone(),
            base_url: if self.base_url.is_empty() {
                None
            } else {
                Some(self.base_url.clone())
            },
            text_encoding: self.text_encoding.clone(),
            note: self.note.clone(),
            color_label: self.color_label,
            show_advanced: self.show_advanced,
        }
    }
}

/// Redux-style Application State for Project Manifold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifoldAppState {
    pub toolbar: ToolbarModel,
    pub sidebar_sections: Vec<SidebarSection>,
    pub panes_layout: PanesLayout,
    pub split_ratio: f32,
    pub dual_pane_enabled: bool,
    pub active_pane: ActivePane,
    pub active_pane_top_accent_hex: String,
    pub left_tabs: Vec<PaneTabModel>,
    pub left_active_tab: usize,
    pub left_header: PathAndStatusHeader,
    pub right_tabs: Vec<PaneTabModel>,
    pub right_active_tab: usize,
    pub right_header: PathAndStatusHeader,
    pub inspector_open: bool,
    pub inspector_active_tab: InspectorTab,
    pub inspector_vm: Option<FileInspectorViewModel>,
    pub connect_dialog: Option<ConnectDialogModel>,
    pub conflict_modal: Option<ConflictModalDescriptor>,
}

impl Default for ManifoldAppState {
    fn default() -> Self {
        Self::new_reference_workspace()
    }
}

impl ManifoldAppState {
    /// Initializes the workspace state matching the reference screenshots (`doc/image1.png` & `doc/image2.png`).
    pub fn new_reference_workspace() -> Self {
        let vol_stats = VolumeStats {
            total_bytes: 500 * 1024 * 1024 * 1024,
            available_bytes: (354.54 * 1024.0 * 1024.0 * 1024.0) as u64,
        };
        let waterfall_meta = FileMetadata {
            name: "Waterfall.jpg".to_string(),
            path: "/Users/johnappleseed/Photos/Waterfall.jpg".to_string(),
            extension: Some("jpg".to_string()),
            kind: "JPEG image".to_string(),
            size: Some(9_751_756),
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

        let left_header = PathAndStatusHeader::build(
            Some("Macintosh HD"),
            "/Users/johnappleseed/Documents/Website Backup",
            19,
            &[],
            Some(vol_stats),
        );
        let right_header = PathAndStatusHeader::build(
            Some("Macintosh HD"),
            "/Users/johnappleseed/Photos",
            9,
            &[&waterfall_meta],
            Some(vol_stats),
        );

        let sidebar_sections = vec![
            SidebarSection {
                title: "Devices".to_string(),
                collapsed: false,
                items: vec![SidebarItem {
                    id: "dev-mac-hd".to_string(),
                    label: "Macintosh HD".to_string(),
                    vfs_url: "file:///".to_string(),
                    icon_kind: "drive".to_string(),
                    capacity_badge: Some("354,54 GB".to_string()),
                    can_eject: false,
                    children: vec![],
                    expanded: false,
                }],
            },
            SidebarSection {
                title: "Connections".to_string(),
                collapsed: false,
                items: vec![SidebarItem {
                    id: "conn-gdrive".to_string(),
                    label: "Google Drive".to_string(),
                    vfs_url: "gdrive:///".to_string(),
                    icon_kind: "gdrive".to_string(),
                    capacity_badge: None,
                    can_eject: true,
                    children: vec![],
                    expanded: false,
                }],
            },
            SidebarSection {
                title: "Shares".to_string(),
                collapsed: false,
                items: vec![SidebarItem {
                    id: "share-binarynights".to_string(),
                    label: "Binarynights".to_string(),
                    vfs_url: "smb://binarynights".to_string(),
                    icon_kind: "server".to_string(),
                    capacity_badge: None,
                    can_eject: false,
                    expanded: true,
                    children: vec![
                        SidebarItem {
                            id: "share-office".to_string(),
                            label: "Office".to_string(),
                            vfs_url: "smb://binarynights/Office".to_string(),
                            icon_kind: "share-mount".to_string(),
                            capacity_badge: Some("470,07 GB".to_string()),
                            can_eject: true,
                            children: vec![],
                            expanded: false,
                        },
                        SidebarItem {
                            id: "share-repos".to_string(),
                            label: "Repositories".to_string(),
                            vfs_url: "smb://binarynights/Repositories".to_string(),
                            icon_kind: "share-mount".to_string(),
                            capacity_badge: Some("470,07 GB".to_string()),
                            can_eject: true,
                            children: vec![],
                            expanded: false,
                        },
                    ],
                }],
            },
            SidebarSection {
                title: "Favorites".to_string(),
                collapsed: false,
                items: vec![
                    SidebarItem {
                        id: "fav-home".to_string(),
                        label: "johnappleseed".to_string(),
                        vfs_url: "file:///Users/johnappleseed".to_string(),
                        icon_kind: "home".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                    SidebarItem {
                        id: "fav-desktop".to_string(),
                        label: "Desktop".to_string(),
                        vfs_url: "file:///Users/johnappleseed/Desktop".to_string(),
                        icon_kind: "desktop".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                    SidebarItem {
                        id: "fav-downloads".to_string(),
                        label: "Downloads".to_string(),
                        vfs_url: "file:///Users/johnappleseed/Downloads".to_string(),
                        icon_kind: "downloads".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                    SidebarItem {
                        id: "fav-apps".to_string(),
                        label: "Applications".to_string(),
                        vfs_url: "file:///Applications".to_string(),
                        icon_kind: "applications".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                    SidebarItem {
                        id: "fav-s3".to_string(),
                        label: "Amazon S3".to_string(),
                        vfs_url: "s3://company-assets".to_string(),
                        icon_kind: "s3".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                ],
            },
            SidebarSection {
                title: "Work".to_string(),
                collapsed: false,
                items: vec![
                    SidebarItem {
                        id: "work-gdrive".to_string(),
                        label: "Google Drive".to_string(),
                        vfs_url: "gdrive:///Work".to_string(),
                        icon_kind: "gdrive".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                    SidebarItem {
                        id: "work-sftp".to_string(),
                        label: "SFTP".to_string(),
                        vfs_url: "sftp://admin@johnappleseed.com:22/User/Shared/Website".into(),
                        icon_kind: "sftp-lock".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                    SidebarItem {
                        id: "work-docs".to_string(),
                        label: "Documents".to_string(),
                        vfs_url: "file:///Users/johnappleseed/Documents".to_string(),
                        icon_kind: "documents".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    },
                ],
            },
        ];

        let left_tabs = vec![
            PaneTabModel {
                id: "l-tab-1".to_string(),
                title: "Downloads".to_string(),
                icon_kind: "downloads".to_string(),
                vfs_url: "file:///Users/johnappleseed/Downloads".to_string(),
                current_path: "/Users/johnappleseed/Downloads".to_string(),
                view_mode: PaneViewMode::List,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
            },
            PaneTabModel {
                id: "l-tab-2".to_string(),
                title: "Desktop".to_string(),
                icon_kind: "desktop".to_string(),
                vfs_url: "file:///Users/johnappleseed/Desktop".to_string(),
                current_path: "/Users/johnappleseed/Desktop".to_string(),
                view_mode: PaneViewMode::List,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
            },
            PaneTabModel {
                id: "l-tab-3".to_string(),
                title: "Website Backup".to_string(),
                icon_kind: "folder".to_string(),
                vfs_url: "file:///Users/johnappleseed/Documents/Website Backup".to_string(),
                current_path: "/Users/johnappleseed/Documents/Website Backup".to_string(),
                view_mode: PaneViewMode::List,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
            },
        ];

        let right_tabs = vec![
            PaneTabModel {
                id: "r-tab-1".to_string(),
                title: "Favorites".to_string(),
                icon_kind: "star".to_string(),
                vfs_url: "favorites://root".to_string(),
                current_path: "/Favorites".to_string(),
                view_mode: PaneViewMode::Icons,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
            },
            PaneTabModel {
                id: "r-tab-2".to_string(),
                title: "johnappleseed".to_string(),
                icon_kind: "home".to_string(),
                vfs_url: "file:///Users/johnappleseed".to_string(),
                current_path: "/Users/johnappleseed".to_string(),
                view_mode: PaneViewMode::Icons,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
            },
            PaneTabModel {
                id: "r-tab-3".to_string(),
                title: "Photos".to_string(),
                icon_kind: "folder".to_string(),
                vfs_url: "file:///Users/johnappleseed/Photos".to_string(),
                current_path: "/Users/johnappleseed/Photos".to_string(),
                view_mode: PaneViewMode::Icons,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::from([waterfall_meta.path.clone()]),
            },
        ];

        let ts = "2023.08.31, 14:57";
        let inspector = FileInspectorViewModel::from_metadata(&waterfall_meta, ts, None);
        let inspector_vm = Some(inspector);

        Self {
            toolbar: ToolbarModel {
                can_go_back: true,
                can_go_forward: false,
                active_folder_title: "Photos".to_string(),
                show_hidden_files: false,
                active_view_mode: PaneViewMode::Icons,
                search_query: String::new(),
            },
            sidebar_sections,
            panes_layout: PanesLayout::SplitHorizontal,
            split_ratio: 0.5,
            dual_pane_enabled: true,
            active_pane: ActivePane::Right,
            active_pane_top_accent_hex: theme::ACTIVE_PANE_TOP_BAR_HEX.to_string(),
            left_tabs,
            left_active_tab: 2,
            left_header,
            right_tabs,
            right_active_tab: 2,
            right_header,
            inspector_open: true,
            inspector_active_tab: InspectorTab::Info,
            inspector_vm,
            connect_dialog: None,
            conflict_modal: None,
        }
    }

    /// Initializes the live desktop workspace bound strictly to real host OS directories,
    /// mounted volumes, and the current user's `$HOME` folders (no dummy/snapshot placeholders).
    pub fn new_live_workspace(left_dir: Option<&Path>, right_dir: Option<&Path>) -> Self {
        let vfs = LocalFsBackend::new();
        let home_dir = std::env::var("HOME")
            .ok()
            .filter(|s| !s.is_empty() && Path::new(s).exists())
            .unwrap_or_else(|| "/".to_string());

        let user_label = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                Path::new(&home_dir)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Home")
                    .to_string()
            });

        let root_vol_name = resolve_dynamic_root_label();
        let root_stats = vfs.volume_stats("/").ok();
        let root_badge = root_stats.map(|s| s.format_available_gb());

        let mut device_items = vec![SidebarItem {
            id: "dev-root".to_string(),
            label: root_vol_name.to_string(),
            vfs_url: "file:///".to_string(),
            icon_kind: "drive".to_string(),
            capacity_badge: root_badge,
            can_eject: false,
            children: vec![],
            expanded: false,
        }];

        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let canon = std::fs::canonicalize(&path);
                let is_root_link = canon.is_ok_and(|p| p == Path::new("/"));
                let is_root = name == root_vol_name || name == "Macintosh HD" || is_root_link;
                let is_valid = !name.starts_with('.') && !is_root && path.is_dir();
                if is_valid {
                    let path_str = path.to_string_lossy().to_string();
                    let badge = vfs
                        .volume_stats(&path_str)
                        .ok()
                        .map(|s| s.format_available_gb());
                    device_items.push(SidebarItem {
                        id: format!("dev-vol-{name}"),
                        label: name,
                        vfs_url: format!("file://{path_str}"),
                        icon_kind: "drive".to_string(),
                        capacity_badge: badge,
                        can_eject: true,
                        children: vec![],
                        expanded: false,
                    });
                }
            }
        }

        let mut favorite_items = vec![SidebarItem {
            id: "fav-home".to_string(),
            label: user_label.clone(),
            vfs_url: format!("file://{home_dir}"),
            icon_kind: "home".to_string(),
            capacity_badge: None,
            can_eject: false,
            children: vec![],
            expanded: false,
        }];

        let candidate_dirs = [
            ("fav-desktop", "Desktop", "desktop"),
            ("fav-documents", "Documents", "documents"),
            ("fav-downloads", "Downloads", "downloads"),
            ("fav-pictures", "Pictures", "pictures"),
            ("fav-music", "Music", "music"),
            ("fav-movies", "Movies", "movies"),
        ];
        for (id, folder, icon) in candidate_dirs {
            let full = format!("{}/{}", home_dir.trim_end_matches('/'), folder);
            if Path::new(&full).is_dir() {
                favorite_items.push(SidebarItem {
                    id: id.to_string(),
                    label: folder.to_string(),
                    vfs_url: format!("file://{full}"),
                    icon_kind: icon.to_string(),
                    capacity_badge: None,
                    can_eject: false,
                    children: vec![],
                    expanded: false,
                });
            }
        }

        if Path::new("/Applications").is_dir() {
            favorite_items.push(SidebarItem {
                id: "fav-apps".to_string(),
                label: "Applications".to_string(),
                vfs_url: "file:///Applications".to_string(),
                icon_kind: "applications".to_string(),
                capacity_badge: None,
                can_eject: false,
                children: vec![],
                expanded: false,
            });
        }

        let cloud_dir = format!("{}/Library/CloudStorage", home_dir.trim_end_matches('/'));
        if let Ok(entries) = std::fs::read_dir(&cloud_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with('.') && path.is_dir() {
                    let path_str = path.to_string_lossy().to_string();
                    favorite_items.push(SidebarItem {
                        id: format!("fav-cloud-{name}"),
                        label: name,
                        vfs_url: format!("file://{path_str}"),
                        icon_kind: "gdrive".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    });
                }
            }
        }

        let saved_cfg = ConnectionsConfig::load_default();
        let mut connection_items: Vec<SidebarItem> = saved_cfg
            .connections
            .iter()
            .map(|p| SidebarItem {
                id: p.id.clone(),
                label: p.name.clone(),
                vfs_url: profile_to_vfs_url(p),
                icon_kind: "globe".to_string(),
                capacity_badge: None,
                can_eject: true,
                children: vec![],
                expanded: false,
            })
            .collect();
        #[cfg(target_os = "linux")]
        if let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") {
            connection_items.extend(linux_network_mounts(&mountinfo).into_iter().map(
                |(label, path)| SidebarItem {
                    id: format!("linux-mount-{path}"),
                    label,
                    vfs_url: format!("file://{path}"),
                    icon_kind: "share-mount".to_string(),
                    capacity_badge: None,
                    can_eject: false,
                    children: vec![],
                    expanded: false,
                },
            ));
        }

        let sidebar_sections = vec![
            SidebarSection {
                title: "Devices".to_string(),
                collapsed: false,
                items: device_items,
            },
            SidebarSection {
                title: "Connections".to_string(),
                collapsed: false,
                items: connection_items,
            },
            SidebarSection {
                title: "Favorites".to_string(),
                collapsed: false,
                items: favorite_items,
            },
        ];

        let left_initial = left_dir
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| home_dir.clone());

        let default_right = {
            let pics = format!("{}/Pictures", home_dir.trim_end_matches('/'));
            let docs = format!("{}/Documents", home_dir.trim_end_matches('/'));
            if Path::new(&pics).is_dir() {
                pics
            } else if Path::new(&docs).is_dir() {
                docs
            } else {
                home_dir.clone()
            }
        };
        let right_initial = right_dir
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or(default_right);

        let mut left_tabs = Vec::new();
        let left_candidates = [
            (user_label.clone(), home_dir.clone(), "home"),
            (
                "Documents".to_string(),
                format!("{}/Documents", home_dir.trim_end_matches('/')),
                "documents",
            ),
            (
                "Downloads".to_string(),
                format!("{}/Downloads", home_dir.trim_end_matches('/')),
                "downloads",
            ),
        ];
        let left_iter = left_candidates.into_iter().enumerate();
        for (idx, (title, path, icon)) in left_iter {
            if idx == 0 || Path::new(&path).is_dir() {
                left_tabs.push(PaneTabModel {
                    id: format!("l-tab-{}", idx + 1),
                    title,
                    icon_kind: icon.to_string(),
                    vfs_url: format!("file://{path}"),
                    current_path: path,
                    view_mode: PaneViewMode::List,
                    sort_column: SortColumn::Name,
                    sort_direction: SortDirection::Ascending,
                    inline_filter: String::new(),
                    expanded_folders: HashSet::new(),
                    selected_paths: HashSet::new(),
                });
            }
        }
        if let Some(first_left) = left_tabs.first_mut() {
            let first_title = Path::new(&left_initial)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&user_label)
                .to_string();
            first_left.current_path = left_initial.clone();
            first_left.vfs_url = format!("file://{left_initial}");
            first_left.title = first_title;
        }

        let right_title = Path::new(&right_initial)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&user_label)
            .to_string();
        let mut right_tabs = vec![PaneTabModel {
            id: "r-tab-1".to_string(),
            title: right_title.clone(),
            icon_kind: "folder".to_string(),
            vfs_url: format!("file://{right_initial}"),
            current_path: right_initial.clone(),
            view_mode: PaneViewMode::Icons,
            sort_column: SortColumn::Name,
            sort_direction: SortDirection::Ascending,
            inline_filter: String::new(),
            expanded_folders: HashSet::new(),
            selected_paths: HashSet::new(),
        }];
        let desktop_dir = format!("{}/Desktop", home_dir.trim_end_matches('/'));
        if Path::new(&desktop_dir).is_dir() && desktop_dir != right_initial {
            right_tabs.push(PaneTabModel {
                id: "r-tab-2".to_string(),
                title: "Desktop".to_string(),
                icon_kind: "desktop".to_string(),
                vfs_url: format!("file://{desktop_dir}"),
                current_path: desktop_dir,
                view_mode: PaneViewMode::Icons,
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
            });
        }

        let left_header =
            PathAndStatusHeader::build(Some(root_vol_name), &left_initial, 0, &[], root_stats);
        let right_header =
            PathAndStatusHeader::build(Some(root_vol_name), &right_initial, 0, &[], root_stats);

        let dual_pane_enabled = right_dir.is_some();
        let panes_layout = if dual_pane_enabled {
            PanesLayout::SplitHorizontal
        } else {
            PanesLayout::Single
        };
        let active_pane = if dual_pane_enabled {
            ActivePane::Right
        } else {
            ActivePane::Left
        };
        let active_folder_title = Path::new(&left_initial)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&user_label)
            .to_string();

        Self {
            toolbar: ToolbarModel {
                can_go_back: false,
                can_go_forward: false,
                active_folder_title,
                show_hidden_files: false,
                active_view_mode: PaneViewMode::List,
                search_query: String::new(),
            },
            sidebar_sections,
            panes_layout,
            split_ratio: 0.5,
            dual_pane_enabled,
            active_pane,
            active_pane_top_accent_hex: theme::ACTIVE_PANE_TOP_BAR_HEX.to_string(),
            left_tabs,
            left_active_tab: 0,
            left_header,
            right_tabs,
            right_active_tab: 0,
            right_header,
            inspector_open: false,
            inspector_active_tab: InspectorTab::Info,
            inspector_vm: None,
            connect_dialog: None,
            conflict_modal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_network_mounts_reads_mountinfo_and_unescapes_paths() {
        let mounts = linux_network_mounts("36 25 0:32 / /mnt/team\\040share rw - cifs //server/team rw\n37 25 0:33 / / rw - ext4 /dev/sda1 rw");
        assert_eq!(
            mounts,
            vec![("team share".to_string(), "/mnt/team share".to_string())]
        );
    }

    #[test]
    fn test_reference_workspace_matches_image1_image2_image3() {
        let state = ManifoldAppState::new_reference_workspace();

        assert_eq!(state.toolbar.active_folder_title, "Photos");
        assert_eq!(state.active_pane, ActivePane::Right);
        assert_eq!(state.active_pane_top_accent_hex, "#4CD964");

        // Verify Combined Path & Status headers at top of each pane (doc/image1.png & doc/image2.png)
        assert_eq!(
            state.left_header.formatted_breadcrumbs,
            "Macintosh HD > Users > johnappleseed > Documents > Website Backup"
        );
        assert_eq!(
            state.left_header.status_subtitle,
            "19 items, 354,54 GB available"
        );
        assert_eq!(
            state.right_header.formatted_breadcrumbs,
            "Macintosh HD > Users > johnappleseed > Photos"
        );
        assert_eq!(
            state.right_header.status_subtitle,
            "1 of 9 selected (9,3 MB), 354,54 GB available"
        );

        // Verify Sidebar sections (Devices, Connections, Shares, Favorites, Work)
        let secs = &state.sidebar_sections;
        let section_titles: Vec<&str> = secs.iter().map(|s| s.title.as_str()).collect();
        let expected_sections = ["Devices", "Connections", "Shares", "Favorites", "Work"];
        assert_eq!(section_titles, expected_sections);

        // Verify SFTP Connect Dialog defaults (doc/image3.png)
        let dialog = ConnectDialogModel::new_sftp_sheet();
        assert_eq!(dialog.protocol, ProtocolType::Sftp);
        assert_eq!(dialog.server, "johnappleseed.com");
        assert_eq!(dialog.port, 22);
        assert_eq!(dialog.username, "admin");
        assert_eq!(dialog.remote_path, "/User/Shared/Website");
        assert_eq!(dialog.base_url, "https://johnappleseed.com/");
        assert_eq!(dialog.text_encoding, "Unicode (UTF-8)");
        assert_eq!(dialog.color_label, ColorLabel::Purple);
        assert!(dialog.show_advanced);
    }

    #[test]
    fn test_virtualized_table_500k_entries_without_blocking() {
        // Verify Phase 3 requirement: handle 500,000+ entries with O(1) viewport slicing
        let total = 500_000usize;
        let rows: Vec<VirtualizedListRow> = (0..total)
            .map(|i| VirtualizedListRow {
                metadata: FileMetadata {
                    name: format!("entry_{i:06}.php"),
                    path: format!("/Website Backup/entry_{i:06}.php"),
                    extension: Some("php".to_string()),
                    kind: "PHP Source".to_string(),
                    size: Some(4096),
                    is_dir: false,
                    is_symlink: false,
                    is_hidden: false,
                    is_locked: false,
                    hide_extension: false,
                    color_label: if i.is_multiple_of(5) {
                        ColorLabel::Orange
                    } else {
                        ColorLabel::None
                    },
                    modified: None,
                    created: None,
                    added: None,
                    permissions: 0o644,
                    owner: None,
                    group: None,
                },
                depth: 0,
                is_expanded: false,
                is_selected: i == 0,
            })
            .collect();

        let viewport = VirtualizedTableViewport::new(rows, 24.0);
        assert_eq!(viewport.total_rows(), 500_000);

        // Scroll to the middle of the 500,000-entry directory (row 250,000 -> offset 6,000,000px)
        let slice = viewport.visible_slice(6_000_000.0, 720.0, 5);
        // 720px / 24px = 30 visible rows + 1 + 2*5 overscan = 41 rows rendered
        assert_eq!(slice.len(), 41);
        assert_eq!(slice[5].metadata.name, "entry_250000.php");
    }

    #[test]
    fn test_live_workspace_has_no_snapshot_placeholders() {
        let state = ManifoldAppState::new_live_workspace(None, None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("johnappleseed"));
        assert!(!json.contains("Binarynights"));
        assert!(!json.contains("Website Backup"));
        assert!(!json.contains("Macintosh HD"));
        assert_eq!(state.panes_layout, PanesLayout::Single);
        assert!(!state.left_tabs.is_empty());
        assert!(!state.right_tabs.is_empty());
    }
}
