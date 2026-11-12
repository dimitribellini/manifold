//! Interactive Slint Desktop GUI Controller for Project Manifold.
//!
//! Binds [`ManifoldAppState`], [`LocalFsBackend`], [`RemoteVfsBackend`], [`TransferEngine`],
//! [`SyncExecutionPlan`], [`BatchRenamer`], and [`FileInspectorViewModel`] to the declarative
//! Slint window defined in `crates/ui/ui/manifold.slint`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use manifold_config::{
    AppPreferences, AuthType, ConnectionProfile, ConnectionsConfig, CredentialStore,
    FavoriteFolderEntry, FolderViewOptions, PaneSessionConfig, PaneViewMode, ProtocolType,
    SessionState, SortColumn, SortDirection, SplitDirection, TabSessionConfig, ViewOptionsScope,
    WindowSessionConfig,
};
use manifold_preview::FileInspectorViewModel;
use manifold_sync::{
    BatchRenamer, CaseConversionStyle, ComparisonMode, RenameRule, RenameTargetScope,
    ReplaceOccurrences, SyncExecutionPlan, SyncOperation, SyncStrategy, TextInsertionPosition,
};
use manifold_transfer::{
    ConflictModalDescriptor, ConflictResolution, TokenBucketLimiter, TransferControlHandle,
    TransferPriority, TransferStatus, TransferTask, DEFAULT_TRANSFER_CHUNK_SIZE,
};
use manifold_vfs::{
    ArchiveBackend, ArchiveFormat, ColorLabel, FileMetadata, FileSystemBackend, LocalFsBackend,
    PermissionsMatrix,
};
use manifold_vfs_remote::{RemoteEditStagingManager, RemoteVfsBackend};
use slint::{Color, Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::terminal::{
    encode_terminal_key_event, NativeShellSession, RemoteSshSpec, StyledSpan, TerminalEmulator,
};
use crate::{profile_to_vfs_url, ActivePane, ManifoldAppState, PanesLayout, SidebarItem};

slint::slint! {
    export {
        ManifoldWindow,
        SidebarEntrySlint,
        TabEntrySlint,
        BreadcrumbSegmentSlint,
        FileRowSlint,
        IconGridTripleSlint,
        TransferRowSlint,
        OpenWithAppSlint,
        BatchRenameRuleSlint,
        BatchRenamePreviewRowSlint,
        SyncPreviewRowSlint,
        TerminalSpanSlint,
        TerminalLineSlint
    } from "ui/manifold.slint";
}

mod archive;
mod batch_rename;
mod clipboard;
mod drag_drop;
mod shortcuts;
mod sync_modal;
mod transfer_worker;
mod trash;

pub use self::clipboard::{current_app_version_label, format_app_version_label};
use self::{
    archive::*, batch_rename::*, clipboard::*, shortcuts::*, sync_modal::*, transfer_worker::*,
    trash::*,
};

const VIRTUAL_WINDOW_CAP: usize = 300;

#[derive(Clone)]
struct FlattenedRowEntry {
    meta: FileMetadata,
    depth: i32,
    is_expanded: bool,
}

enum BgWorkerEvent {
    Progress {
        task_id: String,
        transferred: u64,
        total: u64,
        label: String,
    },
    Finished {
        task_id: String,
        ok_count: usize,
        err_msg: Option<String>,
        toast_msg: String,
    },
}

enum BgSearchEvent {
    Batch {
        is_right: bool,
        gen_id: u64,
        items: Vec<FileMetadata>,
    },
    Finished {
        is_right: bool,
        gen_id: u64,
    },
}

#[derive(Clone)]
struct PendingBatchTransfer {
    queue: VecDeque<FileMetadata>,
    current_collision: Option<(FileMetadata, FileMetadata)>,
    dst_dir: String,
    is_move: bool,
    clear_cut_clipboard: bool,
    apply_all_policy: Option<ConflictResolution>,
    sync_count: usize,
    sync_bytes: u64,
    first_name: String,
    bg_pairs: Vec<(String, String, u64)>,
}

struct GuiRuntime {
    app_state: ManifoldAppState,
    local_vfs: LocalFsBackend,
    remote_backends: HashMap<String, RemoteVfsBackend>,
    archive_backends: HashMap<String, ArchiveBackend>,
    remote_edit_staging: RemoteEditStagingManager,
    transfer_controls: HashMap<String, TransferControlHandle>,
    shared_limiter: Arc<Mutex<TokenBucketLimiter>>,
    active_sync_plan: Option<SyncExecutionPlan>,
    sync_tx: mpsc::Sender<BgSyncEvent>,
    sync_rx: mpsc::Receiver<BgSyncEvent>,
    sync_running: bool,
    dir_size_tx: mpsc::Sender<(String, u64)>,
    dir_size_rx: mpsc::Receiver<(String, u64)>,
    pending_dir_sizes: RefCell<HashSet<String>>,
    thumb_queue: RefCell<VecDeque<String>>,
    pending_thumbs: RefCell<HashSet<String>>,
    left_virtual_top_row: usize,
    right_virtual_top_row: usize,
    saved_connections: ConnectionsConfig,
    prefs: AppPreferences,
    credential_store: CredentialStore,
    worker_tx: mpsc::Sender<BgWorkerEvent>,
    worker_rx: mpsc::Receiver<BgWorkerEvent>,
    bg_tasks_in_flight: usize,
    search_tx: mpsc::Sender<BgSearchEvent>,
    search_rx: mpsc::Receiver<BgSearchEvent>,
    left_search_gen: Arc<AtomicU64>,
    right_search_gen: Arc<AtomicU64>,
    left_search_running: bool,
    right_search_running: bool,
    left_search_key: RefCell<(String, String, bool)>,
    right_search_key: RefCell<(String, String, bool)>,
    left_search_results: Vec<FileMetadata>,
    right_search_results: Vec<FileMetadata>,
    left_entries: Vec<FlattenedRowEntry>,
    right_entries: Vec<FlattenedRowEntry>,
    left_parent_entries: Vec<FileMetadata>,
    right_parent_entries: Vec<FileMetadata>,
    left_selected_idx: Option<usize>,
    right_selected_idx: Option<usize>,
    left_selected_indices: HashSet<usize>,
    right_selected_indices: HashSet<usize>,
    left_anchor_idx: Option<usize>,
    right_anchor_idx: Option<usize>,
    last_nav_instant: Option<Instant>,
    last_row_click: Option<(bool, usize, Instant)>,
    pending_rename_click: Option<(bool, usize, Instant)>,
    #[allow(dead_code)]
    last_hovered_cell: Option<(bool, i32)>,
    #[allow(dead_code)]
    hover_candidate: Option<(bool, i32, f32, f32, Instant)>,
    #[allow(dead_code)]
    hover_shown_at: Option<Instant>,
    clipboard_paths: Vec<String>,
    clipboard_cut: bool,
    sidebar_targets: Vec<(String, String)>,
    sidebar_colors: HashMap<String, ColorLabel>,
    sidebar_context_idx: usize,
    sidebar_scroll_y: f32,
    selected_sidebar_id: Option<String>,
    transfers: Vec<TransferTask>,
    logs: Vec<String>,
    pending_batch: Option<PendingBatchTransfer>,
    pending_delete_permanent: bool,
    batch_rename_rules: Vec<BatchRenameRuleSlint>,
    spinner_frame: i32,
    active_task_ticks: u32,
    active_task_label: String,
    active_task_progress: i32,
    status_banner_text: String,
    status_banner_is_error: bool,
    status_banner_ticks: u32,
    terminal_cwd: String,
    last_pane_sync_path: String,
    active_shell_remote_prefix: Option<String>,
    term_emulator: TerminalEmulator,
    shell_session: Option<NativeShellSession>,
    term_last_size: (u16, u16),
    thumbnail_cache: HashMap<String, Option<Image>>,
    dir_cache: RefCell<HashMap<String, Vec<FileMetadata>>>,
    dir_size_cache: RefCell<HashMap<String, u64>>,
    dir_fingerprints: RefCell<HashMap<String, u64>>,
    fs_poll_tick: u32,
    staging_poll_tick: u32,
    session_save_tick: u32,
    session_dirty: bool,
    native_os_drag_handed_off: bool,
}

impl GuiRuntime {
    fn new(mut app_state: ManifoldAppState) -> Self {
        let saved_connections = ConnectionsConfig::load_default();
        let prefs = AppPreferences::load_default();
        let mut sidebar_colors = HashMap::new();
        for prof in &saved_connections.connections {
            if prof.color_label != ColorLabel::None {
                sidebar_colors.insert(prof.id.clone(), prof.color_label);
            }
        }
        if !prefs.removed_favorites.is_empty() {
            for sec_title in ["Favorites", "Connections"] {
                if let Some(sec) = app_state
                    .sidebar_sections
                    .iter_mut()
                    .find(|s| s.title == sec_title)
                {
                    sec.items.retain(|i| {
                        let norm = normalize_fav_target(&i.vfs_url);
                        !prefs
                            .removed_favorites
                            .iter()
                            .any(|r| r == &i.id || r == &i.vfs_url || r == &norm)
                    });
                }
            }
        }
        for fav in &prefs.custom_favorites {
            let norm_fav = normalize_fav_target(&fav.vfs_url);
            let is_removed = prefs
                .removed_favorites
                .iter()
                .any(|r| r == &fav.id || r == &fav.vfs_url || r == &norm_fav);
            if is_removed {
                continue;
            }
            if fav.color_label != ColorLabel::None {
                sidebar_colors.insert(fav.id.clone(), fav.color_label);
            }
            if let Some(sec) = app_state
                .sidebar_sections
                .iter_mut()
                .find(|s| s.title == "Favorites")
            {
                let already_present = sec
                    .items
                    .iter()
                    .any(|i| i.id == fav.id || normalize_fav_target(&i.vfs_url) == norm_fav);
                if !already_present {
                    sec.items.push(SidebarItem {
                        id: fav.id.clone(),
                        label: fav.label.clone(),
                        vfs_url: fav.vfs_url.clone(),
                        icon_kind: fav.icon_kind.clone(),
                        capacity_badge: None,
                        can_eject: true,
                        children: vec![],
                        expanded: false,
                    });
                }
            }
        }
        for (sb_id, color_lbl) in &prefs.sidebar_colors {
            if *color_lbl != ColorLabel::None {
                sidebar_colors.insert(sb_id.clone(), *color_lbl);
            }
        }
        if !prefs.sidebar_labels.is_empty() {
            for sec in &mut app_state.sidebar_sections {
                for item in &mut sec.items {
                    if let Some(lbl) = prefs.sidebar_labels.get(&item.id) {
                        item.label = lbl.clone();
                    }
                    for child in &mut item.children {
                        if let Some(lbl) = prefs.sidebar_labels.get(&child.id) {
                            child.label = lbl.clone();
                        }
                    }
                }
            }
        }
        if !prefs.favorite_order.is_empty() {
            if let Some(sec) = app_state
                .sidebar_sections
                .iter_mut()
                .find(|s| s.title == "Favorites")
            {
                sec.items.sort_by_key(|item| {
                    prefs
                        .favorite_order
                        .iter()
                        .position(|k| k == &item.id || k == &item.vfs_url)
                        .unwrap_or(usize::MAX)
                });
            }
        }
        let local_vfs = LocalFsBackend::new();
        local_vfs.restore_color_labels(&prefs.file_color_labels);
        let credential_store = CredentialStore::with_os_backend();
        let (worker_tx, worker_rx) = mpsc::channel();
        let (search_tx, search_rx) = mpsc::channel();
        let (sync_tx, sync_rx) = mpsc::channel();
        let (dir_size_tx, dir_size_rx) = mpsc::channel();
        let init_rate = mbps_to_rate_bps(prefs.max_bandwidth_mbps);
        let shared_limiter = Arc::new(Mutex::new(TokenBucketLimiter::new(init_rate)));
        let staging_dir = manifold_vfs::secure_runtime_subdir("staging").ok();
        let init_log = "[INFO] Manifold VFS initialized (Local POSIX + Remote VFS).";
        let keyring_log = "[INFO] Keyring service bound to 'manifold'.";
        let mut init_sel = HashSet::new();
        init_sel.insert(0);
        let init_cwd = app_state
            .left_tabs
            .first()
            .map(|t| t.current_path.clone())
            .unwrap_or_else(|| "/".to_string());
        let term_emulator = TerminalEmulator::new(14, 110);
        let shell_session = NativeShellSession::spawn(&init_cwd, 14, 110);
        let mut runtime = Self {
            app_state,
            local_vfs,
            remote_backends: HashMap::new(),
            archive_backends: HashMap::new(),
            remote_edit_staging: RemoteEditStagingManager::new(staging_dir),
            transfer_controls: HashMap::new(),
            shared_limiter,
            active_sync_plan: None,
            sync_tx,
            sync_rx,
            sync_running: false,
            dir_size_tx,
            dir_size_rx,
            pending_dir_sizes: RefCell::new(HashSet::new()),
            thumb_queue: RefCell::new(VecDeque::new()),
            pending_thumbs: RefCell::new(HashSet::new()),
            left_virtual_top_row: 0,
            right_virtual_top_row: 0,
            saved_connections,
            prefs,
            credential_store,
            worker_tx,
            worker_rx,
            bg_tasks_in_flight: 0,
            search_tx,
            search_rx,
            left_search_gen: Arc::new(AtomicU64::new(0)),
            right_search_gen: Arc::new(AtomicU64::new(0)),
            left_search_running: false,
            right_search_running: false,
            left_search_key: RefCell::new((String::new(), String::new(), false)),
            right_search_key: RefCell::new((String::new(), String::new(), false)),
            left_search_results: Vec::new(),
            right_search_results: Vec::new(),
            left_entries: Vec::new(),
            right_entries: Vec::new(),
            left_parent_entries: Vec::new(),
            right_parent_entries: Vec::new(),
            left_selected_idx: Some(0),
            right_selected_idx: Some(0),
            left_selected_indices: init_sel.clone(),
            right_selected_indices: init_sel,
            left_anchor_idx: Some(0),
            right_anchor_idx: Some(0),
            last_nav_instant: None,
            last_row_click: None,
            pending_rename_click: None,
            last_hovered_cell: None,
            hover_candidate: None,
            hover_shown_at: None,
            clipboard_paths: Vec::new(),
            clipboard_cut: false,
            sidebar_targets: Vec::new(),
            sidebar_colors,
            sidebar_context_idx: 0,
            sidebar_scroll_y: 0.0,
            selected_sidebar_id: None,
            transfers: Vec::new(),
            logs: vec![init_log.to_string(), keyring_log.to_string()],
            pending_batch: None,
            pending_delete_permanent: false,
            batch_rename_rules: vec![default_batch_rename_rule()],
            spinner_frame: 0,
            active_task_ticks: 0,
            active_task_label: String::new(),
            active_task_progress: 0,
            status_banner_text: String::new(),
            status_banner_is_error: false,
            status_banner_ticks: 0,
            terminal_cwd: init_cwd.clone(),
            last_pane_sync_path: init_cwd,
            active_shell_remote_prefix: None,
            term_emulator,
            shell_session,
            term_last_size: (14, 110),
            thumbnail_cache: HashMap::new(),
            dir_cache: RefCell::new(HashMap::new()),
            dir_size_cache: RefCell::new(HashMap::new()),
            dir_fingerprints: RefCell::new(HashMap::new()),
            fs_poll_tick: 0,
            staging_poll_tick: 0,
            session_save_tick: 0,
            session_dirty: false,
            native_os_drag_handed_off: false,
        };
        runtime.reload_pane_entries(false);
        runtime.reload_pane_entries(true);
        runtime.sync_selected_sidebar_to_active_dir();
        runtime
    }

    fn restore_saved_session(&mut self, window: &ManifoldWindow) {
        let Some(sess) = SessionState::load_default() else {
            return;
        };
        let home_fallback = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .filter(|s| !s.is_empty() && Path::new(s).is_dir())
            .unwrap_or_else(|| "/".to_string());

        let mut restored_left = Vec::new();
        for (idx, t) in sess.left_pane.tabs.iter().enumerate() {
            let raw_path = if !t.current_path.is_empty() {
                t.current_path.clone()
            } else {
                vfs_url_to_target(&t.vfs_url)
            };
            let is_arch =
                split_archive_uri(&raw_path).is_some_and(|(a, _)| Path::new(&a).is_file());
            let is_valid =
                split_remote_uri(&raw_path).is_some() || is_arch || Path::new(&raw_path).is_dir();
            let valid_path = if is_valid {
                raw_path
            } else if idx == 0 {
                home_fallback.clone()
            } else {
                continue;
            };
            let title = t
                .title
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.format_tab_title(&valid_path));
            let vfs_url = if valid_path.contains("://") {
                valid_path.clone()
            } else {
                format!("file://{valid_path}")
            };
            let exp_set: HashSet<String> = t
                .expanded_folders
                .iter()
                .filter(|p| Path::new(p.as_str()).is_dir())
                .cloned()
                .collect();
            restored_left.push(crate::PaneTabModel {
                id: format!("l-tab-{}", idx + 1),
                title,
                icon_kind: "folder".to_string(),
                vfs_url,
                current_path: valid_path,
                view_mode: t.view_mode,
                sort_column: t.sort_column,
                sort_direction: t.sort_direction,
                inline_filter: String::new(),
                expanded_folders: exp_set,
                selected_paths: HashSet::new(),
                back_stack: Vec::new(),
                forward_stack: Vec::new(),
            });
        }
        if !restored_left.is_empty() {
            let max_l = restored_left.len() - 1;
            self.app_state.left_tabs = restored_left;
            self.app_state.left_active_tab = sess.left_pane.active_tab_index.min(max_l);
        }

        let mut restored_right = Vec::new();
        for (idx, t) in sess.right_pane.tabs.iter().enumerate() {
            let raw_path = if !t.current_path.is_empty() {
                t.current_path.clone()
            } else {
                vfs_url_to_target(&t.vfs_url)
            };
            let is_arch =
                split_archive_uri(&raw_path).is_some_and(|(a, _)| Path::new(&a).is_file());
            let is_valid =
                split_remote_uri(&raw_path).is_some() || is_arch || Path::new(&raw_path).is_dir();
            let valid_path = if is_valid {
                raw_path
            } else if idx == 0 {
                home_fallback.clone()
            } else {
                continue;
            };
            let title = t
                .title
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.format_tab_title(&valid_path));
            let vfs_url = if valid_path.contains("://") {
                valid_path.clone()
            } else {
                format!("file://{valid_path}")
            };
            let exp_set: HashSet<String> = t
                .expanded_folders
                .iter()
                .filter(|p| Path::new(p.as_str()).is_dir())
                .cloned()
                .collect();
            restored_right.push(crate::PaneTabModel {
                id: format!("r-tab-{}", idx + 1),
                title,
                icon_kind: "folder".to_string(),
                vfs_url,
                current_path: valid_path,
                view_mode: t.view_mode,
                sort_column: t.sort_column,
                sort_direction: t.sort_direction,
                inline_filter: String::new(),
                expanded_folders: exp_set,
                selected_paths: HashSet::new(),
                back_stack: Vec::new(),
                forward_stack: Vec::new(),
            });
        }
        if !restored_right.is_empty() {
            let max_r = restored_right.len() - 1;
            self.app_state.right_tabs = restored_right;
            self.app_state.right_active_tab = sess.right_pane.active_tab_index.min(max_r);
        }

        self.app_state.dual_pane_enabled = sess.window.dual_pane_enabled;
        self.app_state.panes_layout = if sess.window.dual_pane_enabled {
            if sess.window.split_direction == SplitDirection::Horizontal {
                PanesLayout::SplitVertical
            } else {
                PanesLayout::SplitHorizontal
            }
        } else {
            PanesLayout::Single
        };
        self.app_state.active_pane =
            if sess.window.dual_pane_enabled && sess.window.active_pane == "right" {
                ActivePane::Right
            } else {
                ActivePane::Left
            };
        self.app_state.split_ratio = (sess.window.split_ratio as f32).clamp(0.20, 0.80);
        self.app_state.toolbar.show_hidden_files = sess.window.show_hidden_files;
        self.app_state.inspector_open = sess.window.inspector_open;

        let w_px = sess.window.width.clamp(920, 3840) as f32;
        let h_px = sess.window.height.clamp(580, 2400) as f32;
        window
            .window()
            .set_size(slint::LogicalSize::new(w_px, h_px));
        window.set_sidebar_width(sess.window.sidebar_width.clamp(155.0, 340.0));
        window.set_inspector_width(sess.window.inspector_width.clamp(256.0, 460.0));
        window.set_size_col_width(sess.window.size_col_width.clamp(68.0, 220.0));
        window.set_date_col_width(sess.window.date_col_width.clamp(110.0, 260.0));
        window.set_miller_col_width(sess.window.miller_parent_width.clamp(120.0, 420.0));
        window.set_terminal_drawer_height(sess.window.terminal_drawer_height.clamp(130.0, 600.0));
        window.set_inspector_tab_index(sess.window.inspector_tab_index.min(1) as i32);
        window.set_show_terminal_drawer(sess.window.terminal_drawer_open);

        self.reload_pane_entries(false);
        self.reload_pane_entries(true);
        self.sync_selected_sidebar_to_active_dir();
        let restored_path = self.current_pane_path(self.is_right_effective());
        if restored_path != self.terminal_cwd {
            self.restart_terminal_in(&restored_path);
        }
    }

    fn capture_and_save_session(&self, window: &ManifoldWindow) {
        let scale = window.window().scale_factor().max(1.0);
        let phys = window.window().size();
        let raw_w = ((phys.width as f32) / scale).round() as u32;
        let raw_h = ((phys.height as f32) / scale).round() as u32;
        let width = if raw_w >= 600 { raw_w } else { 1360 };
        let height = if raw_h >= 400 { raw_h } else { 840 };

        let is_right_pane =
            self.app_state.active_pane == ActivePane::Right && self.app_state.dual_pane_enabled;
        let active_pane_str = if is_right_pane {
            "right".to_string()
        } else {
            "left".to_string()
        };
        let insp_tab = window.get_inspector_tab_index().clamp(0, 1) as usize;
        let split_dir = if self.app_state.panes_layout == PanesLayout::SplitVertical {
            SplitDirection::Horizontal
        } else {
            SplitDirection::Vertical
        };
        let win_cfg = WindowSessionConfig {
            width,
            height,
            split_ratio: f64::from(self.app_state.split_ratio),
            split_direction: split_dir,
            show_hidden_files: self.app_state.toolbar.show_hidden_files,
            show_preview_pane: true,
            dual_pane_enabled: self.app_state.dual_pane_enabled,
            active_pane: active_pane_str,
            sidebar_width: window.get_sidebar_width(),
            inspector_open: self.app_state.inspector_open,
            inspector_width: window.get_inspector_width(),
            size_col_width: window.get_size_col_width(),
            date_col_width: window.get_date_col_width(),
            miller_parent_width: window.get_miller_col_width(),
            terminal_drawer_height: window.get_terminal_drawer_height(),
            inspector_tab_index: insp_tab,
            terminal_drawer_open: window.get_show_terminal_drawer(),
        };

        let left_tabs: Vec<TabSessionConfig> = self
            .app_state
            .left_tabs
            .iter()
            .map(|t| {
                let exp_vec: Vec<String> = t.expanded_folders.iter().cloned().collect();
                TabSessionConfig {
                    vfs_url: t.vfs_url.clone(),
                    current_path: t.current_path.clone(),
                    title: Some(t.title.clone()),
                    view_mode: t.view_mode,
                    sort_column: t.sort_column,
                    sort_direction: t.sort_direction,
                    expanded_folders: exp_vec,
                }
            })
            .collect();

        let right_tabs: Vec<TabSessionConfig> = self
            .app_state
            .right_tabs
            .iter()
            .map(|t| {
                let exp_vec: Vec<String> = t.expanded_folders.iter().cloned().collect();
                TabSessionConfig {
                    vfs_url: t.vfs_url.clone(),
                    current_path: t.current_path.clone(),
                    title: Some(t.title.clone()),
                    view_mode: t.view_mode,
                    sort_column: t.sort_column,
                    sort_direction: t.sort_direction,
                    expanded_folders: exp_vec,
                }
            })
            .collect();

        let state = SessionState {
            window: win_cfg,
            left_pane: PaneSessionConfig {
                active_tab_index: self.app_state.left_active_tab,
                tabs: left_tabs,
            },
            right_pane: PaneSessionConfig {
                active_tab_index: self.app_state.right_active_tab,
                tabs: right_tabs,
            },
        };
        let _ = state.save_default();
    }

    fn poll_external_directory_changes(&mut self) -> bool {
        let mut left_changed = false;
        let mut right_changed = false;

        let left_dir = self.current_pane_path(false);
        let mut left_watch = vec![left_dir];
        if let Some(tab) = self.app_state.left_tabs.get(self.app_state.left_active_tab) {
            left_watch.extend(tab.expanded_folders.iter().cloned());
        }
        for dir in left_watch {
            if let Some(fp) = compute_local_dir_fingerprint(&dir) {
                let prev = self.dir_fingerprints.borrow().get(&dir).copied();
                if prev != Some(fp) {
                    self.dir_fingerprints.borrow_mut().insert(dir.clone(), fp);
                    self.dir_cache.borrow_mut().remove(&dir);
                    left_changed = true;
                }
            }
        }

        if self.app_state.dual_pane_enabled {
            let right_dir = self.current_pane_path(true);
            let mut right_watch = vec![right_dir];
            if let Some(tab) = self
                .app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
            {
                right_watch.extend(tab.expanded_folders.iter().cloned());
            }
            for dir in right_watch {
                if let Some(fp) = compute_local_dir_fingerprint(&dir) {
                    let prev = self.dir_fingerprints.borrow().get(&dir).copied();
                    if prev != Some(fp) {
                        self.dir_fingerprints.borrow_mut().insert(dir.clone(), fp);
                        self.dir_cache.borrow_mut().remove(&dir);
                        right_changed = true;
                    }
                }
            }
        }

        if left_changed {
            let sel_paths: HashSet<String> = self
                .left_selected_indices
                .iter()
                .filter_map(|&i| self.left_entries.get(i).map(|r| r.meta.path.clone()))
                .collect();
            self.reload_pane_entries(false);
            if !sel_paths.is_empty() {
                let mut restored = HashSet::new();
                for (i, r) in self.left_entries.iter().enumerate() {
                    if sel_paths.contains(&r.meta.path) {
                        restored.insert(i);
                    }
                }
                if !restored.is_empty() {
                    let first = restored.iter().copied().min();
                    self.left_selected_indices = restored;
                    self.left_selected_idx = first;
                }
            }
        }

        if right_changed {
            let sel_paths: HashSet<String> = self
                .right_selected_indices
                .iter()
                .filter_map(|&i| self.right_entries.get(i).map(|r| r.meta.path.clone()))
                .collect();
            self.reload_pane_entries(true);
            if !sel_paths.is_empty() {
                let mut restored = HashSet::new();
                for (i, r) in self.right_entries.iter().enumerate() {
                    if sel_paths.contains(&r.meta.path) {
                        restored.insert(i);
                    }
                }
                if !restored.is_empty() {
                    let first = restored.iter().copied().min();
                    self.right_selected_indices = restored;
                    self.right_selected_idx = first;
                }
            }
        }

        let mounts_changed = self.sync_active_network_mounts();
        let staged_changed = self.poll_remote_staged_edits();
        left_changed || right_changed || mounts_changed || staged_changed
    }

    fn poll_remote_staged_edits(&mut self) -> bool {
        self.staging_poll_tick = self.staging_poll_tick.wrapping_add(1);
        if !self.staging_poll_tick.is_multiple_of(2) {
            return false;
        }
        let sessions = self.remote_edit_staging.active_session_ids();
        if sessions.is_empty() {
            return false;
        }
        let mut total_synced = 0usize;
        for sess_id in sessions {
            let remote_opt = self
                .remote_backends
                .values()
                .find(|b| b.profile().id == sess_id)
                .cloned();
            if let Some(remote) = remote_opt {
                if let Ok(count) = self
                    .remote_edit_staging
                    .sync_modified_files_for_session(Some(&sess_id), &remote)
                {
                    total_synced += count;
                }
            }
        }
        if total_synced > 0 {
            let banner = format!("Auto-uploaded {total_synced} modified remote file(s)");
            let msg = format!("[STAGING] {banner}");
            self.logs.push(msg);
            self.show_toast("info", banner);
            self.invalidate_dir_cache();
            self.reload_pane_entries(false);
            if self.app_state.dual_pane_enabled {
                self.reload_pane_entries(true);
            }
            return true;
        }
        false
    }

    fn sync_active_network_mounts(&mut self) -> bool {
        let detected = crate::detect_active_network_mounts();
        let active_ids: HashSet<String> = detected.iter().map(|i| i.id.clone()).collect();
        let Some(sec) = self
            .app_state
            .sidebar_sections
            .iter_mut()
            .find(|s| s.title == "Connections")
        else {
            return false;
        };

        let before_len = sec.items.len();
        sec.items
            .retain(|i| !i.id.starts_with("conn-mnt-") || active_ids.contains(&i.id));
        let mut changed = sec.items.len() != before_len;

        for mut mnt in detected {
            let norm = normalize_fav_target(&mnt.vfs_url);
            let is_dismissed = self
                .prefs
                .removed_favorites
                .iter()
                .any(|r| r == &mnt.id || r == &mnt.vfs_url || r == &norm);
            if is_dismissed {
                continue;
            }
            if let Some(custom_lbl) = self.prefs.sidebar_labels.get(&mnt.id) {
                mnt.label = custom_lbl.clone();
            }
            let exists = sec
                .items
                .iter()
                .any(|c| c.id == mnt.id || c.vfs_url == mnt.vfs_url);
            if !exists {
                sec.items.push(mnt);
                changed = true;
            }
        }
        changed
    }

    fn format_tab_title(&self, path: &str) -> String {
        if let Some((prefix, sub)) = split_remote_uri(path) {
            let conn_name = self
                .saved_connections
                .connections
                .iter()
                .find(|p| profile_to_vfs_url(p).starts_with(&prefix))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| {
                    prefix
                        .split_once("://")
                        .map(|(_, auth)| auth.to_string())
                        .unwrap_or_else(|| prefix.clone())
                });
            let clean_sub = if sub.is_empty() { "/" } else { sub.as_str() };
            return format!("{conn_name}: {clean_sub}");
        }
        vfs_display_title(path)
    }

    fn restart_terminal_in(&mut self, dir_path: &str) {
        self.activate_terminal_for_path(dir_path, true);
    }

    fn build_remote_ssh_spec(&mut self, prefix: &str, sub_path: &str) -> Option<RemoteSshSpec> {
        self.ensure_remote_connected(prefix);
        let clean_pref = prefix.trim_end_matches('/');
        let prof_opt = self
            .remote_backends
            .get(prefix)
            .or_else(|| self.remote_backends.get(clean_pref))
            .map(|b| b.profile().clone())
            .or_else(|| {
                self.saved_connections
                    .connections
                    .iter()
                    .find(|p| profile_to_vfs_url(p).trim_end_matches('/') == clean_pref)
                    .cloned()
            });

        let clean_sub = if sub_path.trim().is_empty() {
            "/".to_string()
        } else {
            sub_path.trim().to_string()
        };

        if let Some(prof) = prof_opt {
            let host = prof.host.as_deref().filter(|h| !h.is_empty())?.to_string();
            let port = prof.port.unwrap_or(22);
            let key_path = if prof.auth_type == AuthType::PublicKey {
                prof.key_path.clone().filter(|k| !k.trim().is_empty())
            } else {
                None
            };
            let secret = self
                .remote_backends
                .get(prefix)
                .or_else(|| self.remote_backends.get(clean_pref))
                .and_then(|b| b.secret().map(str::to_string))
                .or_else(|| self.credential_store.get_secret(&prof.id).ok().flatten());
            return Some(RemoteSshSpec {
                conn_name: prof.name,
                host,
                port,
                username: prof.username,
                key_path,
                secret,
                remote_dir: clean_sub,
            });
        }

        let rest = clean_pref.strip_prefix("sftp://")?;
        let (username, host_port) = match rest.split_once('@') {
            Some((u, hp)) => (u.to_string(), hp),
            None => (String::new(), rest),
        };
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p_str)) => (h.to_string(), p_str.parse::<u16>().unwrap_or(22)),
            None => (host_port.to_string(), 22),
        };
        if host.trim().is_empty() {
            return None;
        }
        Some(RemoteSshSpec {
            conn_name: host.clone(),
            host,
            port,
            username,
            key_path: None,
            secret: None,
            remote_dir: clean_sub,
        })
    }

    fn activate_terminal_for_path(&mut self, target_path: &str, force_restart: bool) {
        let (rows, cols) = self.term_last_size;
        self.last_pane_sync_path = target_path.to_string();
        self.terminal_cwd = target_path.to_string();
        if !force_restart && self.term_emulator.using_alt_screen() {
            return;
        }

        if let Some((prefix, sub)) = split_remote_uri(target_path) {
            let clean_pref = prefix.trim_end_matches('/').to_string();
            let same_remote = !force_restart
                && self.active_shell_remote_prefix.as_deref() == Some(clean_pref.as_str())
                && self.shell_session.is_some();
            if same_remote {
                if let Some(shell) = &self.shell_session {
                    shell.sync_remote_cwd_if_idle(&sub);
                }
                return;
            }

            self.active_shell_remote_prefix = Some(clean_pref.clone());
            self.term_emulator = TerminalEmulator::new(rows as usize, cols as usize);
            let conn_title = self.format_tab_title(target_path);
            if clean_pref.starts_with("sftp://") {
                if let Some(spec) = self.build_remote_ssh_spec(&prefix, &sub) {
                    let cname = &spec.conn_name;
                    let endpoint = format!("{}@{}:{}", spec.username, spec.host, spec.port);
                    let hdr = "\x1b[1;36m● Remote SSH\x1b[0m";
                    let banner = format!("{hdr} — \x1b[1;32m{cname}\x1b[0m ({endpoint})\r\n");
                    self.term_emulator.feed_bytes(banner.as_bytes());
                    match NativeShellSession::spawn_remote_ssh(&spec, rows, cols) {
                        Some(sess) => {
                            self.shell_session = Some(sess);
                        }
                        None => {
                            let err_line = concat!(
                                "\x1b[1;31m✖ Remote SSH session failed to launch ",
                                "(verify OpenSSH client & credentials).\x1b[0m\r\n"
                            );
                            self.term_emulator.feed_bytes(err_line.as_bytes());
                            self.shell_session = None;
                            self.show_toast("error", "Failed to start Remote SSH session");
                        }
                    }
                    return;
                }
            }
            let hdr = "\x1b[1;36m● Remote Session\x1b[0m";
            let banner = format!("{hdr} — \x1b[1;32m{conn_title}\x1b[0m ({clean_pref})\r\n");
            self.term_emulator.feed_bytes(banner.as_bytes());
            self.shell_session = None;
            return;
        }

        let resolved_local = if let Some((arch_path, _)) = split_archive_uri(target_path) {
            Path::new(&arch_path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("/")
                .to_string()
        } else {
            target_path.to_string()
        };

        let keep_local = !force_restart
            && self.active_shell_remote_prefix.is_none()
            && self.shell_session.is_some();
        if keep_local {
            if let Some(shell) = &self.shell_session {
                shell.sync_cwd_if_idle(&resolved_local);
            }
            return;
        }

        self.active_shell_remote_prefix = None;
        self.term_emulator = TerminalEmulator::new(rows as usize, cols as usize);
        self.shell_session = NativeShellSession::spawn(&resolved_local, rows, cols);
    }

    fn open_terminal_at_path(&mut self, target_path: &str) {
        self.activate_terminal_for_path(target_path, false);
    }

    fn sync_terminal_cwd(&mut self) {
        let active_path = self.current_pane_path(self.is_right_effective());
        let remote_pref =
            split_remote_uri(&active_path).map(|(p, _)| p.trim_end_matches('/').to_string());
        let mode_mismatch = self.active_shell_remote_prefix != remote_pref;
        if self.last_pane_sync_path != active_path || mode_mismatch {
            self.activate_terminal_for_path(&active_path, false);
        }
    }

    fn sync_terminal_cwd_explicit(&mut self) {
        let active_path = self.current_pane_path(self.is_right_effective());
        let target_dir = if let Some((_, sub)) = split_remote_uri(&active_path) {
            sub
        } else if let Some((arch_path, _)) = split_archive_uri(&active_path) {
            Path::new(&arch_path)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("/")
                .to_string()
        } else {
            active_path.clone()
        };
        if let Some(shell) = &self.shell_session {
            shell.sync_cwd_explicit(&target_dir);
            self.last_pane_sync_path = active_path.clone();
            self.terminal_cwd = active_path;
        } else {
            self.activate_terminal_for_path(&active_path, true);
        }
    }

    fn sync_terminal_dimensions(&mut self, window: &ManifoldWindow) -> bool {
        let drawer_h = window.get_terminal_drawer_height().max(130.0);
        let delta = self.prefs.font_size_delta as f32;
        let row_h = (18.0 + delta * 1.5).max(15.0);
        let canvas_h = window.get_terminal_canvas_height();
        let avail_h = if canvas_h >= 80.0 {
            (canvas_h - 8.0).max(row_h * 4.0)
        } else {
            (drawer_h - 36.0).max(row_h * 4.0)
        };
        let rows = (avail_h / row_h).floor().clamp(4.0, 80.0) as u16;

        let scale = window.window().scale_factor().max(1.0);
        let phys_w = (window.window().size().width as f32) / scale;
        let win_w = if phys_w >= 600.0 { phys_w } else { 1360.0 };
        let sb_w = window.get_sidebar_width();
        let insp_w = if self.app_state.inspector_open {
            window.get_inspector_width()
        } else {
            0.0
        };
        let est_w = (win_w - sb_w - insp_w - 20.0).max(320.0);
        let measured_w = window.get_terminal_canvas_width();
        let measured_ok = measured_w >= 280.0 && (measured_w - est_w).abs() < 120.0;
        let canvas_w = if measured_ok { measured_w } else { est_w };
        let avail_w = (canvas_w - 24.0).max(280.0);
        let probe_w = window.get_terminal_cell_width();
        let char_w = if (6.0..=18.0).contains(&probe_w) {
            probe_w
        } else {
            (7.8 + delta * 0.6).max(6.2)
        };
        let cols = (avail_w / char_w).floor().clamp(40.0, 240.0) as u16;

        if self.term_last_size != (rows, cols) {
            self.term_last_size = (rows, cols);
            self.term_emulator.resize(rows as usize, cols as usize);
            if let Some(shell) = &self.shell_session {
                shell.resize_pty(rows, cols);
            }
            return true;
        }
        false
    }

    fn poll_pty_output(&mut self) -> bool {
        let Some(shell) = &self.shell_session else {
            return false;
        };
        let mut changed = false;
        while let Some(chunk) = shell.try_recv_bytes() {
            if !chunk.is_empty() {
                self.term_emulator.feed_bytes(&chunk);
                changed = true;
            }
        }
        if changed {
            let responses = self.term_emulator.take_pending_responses();
            for resp in responses {
                let _ = shell.send_bytes(&resp);
            }
            if let Some(osc_cwd) = self.term_emulator.take_detected_cwd() {
                if !osc_cwd.is_empty() {
                    let next_cwd = if let Some(pref) = &self.active_shell_remote_prefix {
                        if osc_cwd.contains("://") {
                            osc_cwd
                        } else {
                            let sub = if osc_cwd.starts_with('/') {
                                osc_cwd
                            } else {
                                format!("/{osc_cwd}")
                            };
                            format!("{}{sub}", pref.trim_end_matches('/'))
                        }
                    } else {
                        osc_cwd
                    };
                    if next_cwd != self.terminal_cwd {
                        self.terminal_cwd = next_cwd;
                    }
                }
            } else if self.active_shell_remote_prefix.is_none() {
                if let Some(live_cwd) = shell.query_live_cwd() {
                    if !live_cwd.is_empty() && live_cwd != self.terminal_cwd {
                        self.terminal_cwd = live_cwd;
                    }
                }
            }
        }
        changed
    }

    fn is_path_in_favorites(&self, target_path: &str) -> bool {
        let norm = normalize_fav_target(target_path);
        if norm.is_empty() {
            return false;
        }
        self.app_state
            .sidebar_sections
            .iter()
            .find(|s| s.title == "Favorites")
            .is_some_and(|sec| {
                sec.items
                    .iter()
                    .any(|item| normalize_fav_target(&item.vfs_url) == norm)
            })
    }

    fn unpin_folder_from_favorites(&mut self, id_hint: &str, target_path: &str) {
        let norm = normalize_fav_target(target_path);
        let mut removed_ids = Vec::new();
        let mut removed_label = String::new();
        if let Some(sec) = self
            .app_state
            .sidebar_sections
            .iter_mut()
            .find(|s| s.title == "Favorites")
        {
            sec.items.retain(|i| {
                let i_norm = normalize_fav_target(&i.vfs_url);
                let by_id = !id_hint.is_empty() && i.id == id_hint;
                let by_path = !norm.is_empty() && i_norm == norm;
                let matched = by_id || by_path;
                if matched {
                    if removed_label.is_empty() {
                        removed_label = i.label.clone();
                    }
                    removed_ids.push((i.id.clone(), i.vfs_url.clone()));
                }
                !matched
            });
        }
        if !id_hint.is_empty() {
            self.prefs
                .remove_favorite_by_id_or_path(id_hint, target_path);
        }
        for (rem_id, rem_url) in removed_ids {
            self.prefs.remove_favorite_by_id_or_path(&rem_id, &rem_url);
        }
        if !norm.is_empty() {
            let file_url = if norm.contains("://") {
                norm.clone()
            } else {
                format!("file://{norm}")
            };
            self.prefs.remove_favorite_by_id_or_path("", &file_url);
            self.prefs.remove_favorite_by_id_or_path("", &norm);
        }
        self.prefs.favorite_order = self
            .app_state
            .sidebar_sections
            .iter()
            .find(|s| s.title == "Favorites")
            .map(|sec| sec.items.iter().map(|i| i.id.clone()).collect())
            .unwrap_or_default();
        let _ = self.prefs.save_default();
        let disp = if removed_label.is_empty() {
            vfs_display_title(&norm)
        } else {
            removed_label
        };
        let toast = format!("Removed \"{disp}\" from Favorites");
        self.logs.push(format!("[FAVORITES] {toast}"));
        self.show_toast("info", toast);
    }

    fn sync_selected_sidebar_to_active_dir(&mut self) {
        let active_dir = self.current_pane_path(self.is_right_effective());
        let norm_active = normalize_fav_target(&active_dir);
        if norm_active.is_empty() {
            self.selected_sidebar_id = None;
            return;
        }
        if let Some(cur_id) = &self.selected_sidebar_id {
            let still_matches = self.app_state.sidebar_sections.iter().any(|sec| {
                sec.items.iter().any(|it| {
                    let top_ok =
                        it.id == *cur_id && normalize_fav_target(&it.vfs_url) == norm_active;
                    let child_ok = it.children.iter().any(|ch| {
                        let ch_norm = normalize_fav_target(&ch.vfs_url);
                        ch.id == *cur_id && ch_norm == norm_active
                    });
                    top_ok || child_ok
                })
            });
            if still_matches {
                return;
            }
        }
        for sec_title in ["Favorites", "Connections", "Devices"] {
            if let Some(sec) = self
                .app_state
                .sidebar_sections
                .iter()
                .find(|s| s.title == sec_title)
            {
                for item in &sec.items {
                    if normalize_fav_target(&item.vfs_url) == norm_active {
                        self.selected_sidebar_id = Some(item.id.clone());
                        return;
                    }
                    for child in &item.children {
                        if normalize_fav_target(&child.vfs_url) == norm_active {
                            self.selected_sidebar_id = Some(child.id.clone());
                            return;
                        }
                    }
                }
            }
        }
        self.selected_sidebar_id = None;
    }

    fn pin_folder_to_favorites(&mut self, folder_path: &str, label: &str) {
        let norm = normalize_fav_target(folder_path);
        let vfs_url = if folder_path.contains("://") {
            folder_path.to_string()
        } else {
            format!("file://{folder_path}")
        };
        self.prefs
            .removed_favorites
            .retain(|r| r != &vfs_url && r != folder_path && r != &norm);
        let existing_id = self
            .prefs
            .custom_favorites
            .iter()
            .find(|f| normalize_fav_target(&f.vfs_url) == norm)
            .map(|f| f.id.clone());
        let fav_id = existing_id.unwrap_or_else(|| {
            let mut n = self.prefs.custom_favorites.len() + 1;
            loop {
                let cand = format!("fav-pin-{n}");
                let taken = self.prefs.custom_favorites.iter().any(|f| f.id == cand);
                if !taken {
                    break cand;
                }
                n += 1;
            }
        });
        let color_lbl = self
            .sidebar_colors
            .get(&fav_id)
            .copied()
            .unwrap_or(ColorLabel::None);
        let fav_entry = FavoriteFolderEntry {
            id: fav_id.clone(),
            label: label.to_string(),
            vfs_url: vfs_url.clone(),
            icon_kind: "folder".to_string(),
            color_label: color_lbl,
        };
        self.prefs.upsert_favorite(fav_entry);
        let _ = self.prefs.save_default();

        let fav_sec = self
            .app_state
            .sidebar_sections
            .iter_mut()
            .find(|s| s.title == "Favorites");
        if let Some(sec) = fav_sec {
            let exists = sec
                .items
                .iter()
                .any(|i| normalize_fav_target(&i.vfs_url) == norm);
            if !exists {
                sec.items.push(SidebarItem {
                    id: fav_id,
                    label: label.to_string(),
                    vfs_url,
                    icon_kind: "folder".to_string(),
                    capacity_badge: None,
                    can_eject: true,
                    children: vec![],
                    expanded: false,
                });
            }
        }
        let msg = format!("[FAVORITES] Pinned {label} ({folder_path})");
        self.logs.push(msg);
        let toast = format!("Added \"{label}\" to Favorites");
        self.show_toast("success", toast);
    }

    fn reorder_favorites_by_item_ids(&mut self, from_id: &str, to_id: &str) -> bool {
        if from_id.is_empty() || to_id.is_empty() || from_id == to_id {
            return false;
        }
        let Some(sec) = self
            .app_state
            .sidebar_sections
            .iter_mut()
            .find(|s| s.title == "Favorites")
        else {
            return false;
        };
        let Some(from_pos) = sec.items.iter().position(|i| i.id == from_id) else {
            return false;
        };
        let Some(to_pos) = sec.items.iter().position(|i| i.id == to_id) else {
            return false;
        };
        let item = sec.items.remove(from_pos);
        sec.items.insert(to_pos, item);
        self.prefs.favorite_order = sec.items.iter().map(|i| i.id.clone()).collect();
        let _ = self.prefs.save_default();
        true
    }

    fn move_favorite_step(&mut self, item_id: &str, delta: isize) -> bool {
        let Some(sec) = self
            .app_state
            .sidebar_sections
            .iter_mut()
            .find(|s| s.title == "Favorites")
        else {
            return false;
        };
        let Some(pos) = sec.items.iter().position(|i| i.id == item_id) else {
            return false;
        };
        let next_isize = (pos as isize + delta).clamp(0, sec.items.len() as isize - 1);
        let next_pos = next_isize as usize;
        if next_pos == pos {
            return false;
        }
        let item = sec.items.remove(pos);
        sec.items.insert(next_pos, item);
        self.prefs.favorite_order = sec.items.iter().map(|i| i.id.clone()).collect();
        let _ = self.prefs.save_default();
        true
    }

    fn spawn_bg_transfer_batch(
        &mut self,
        pairs: Vec<(String, String, u64)>,
        is_move: bool,
        label: String,
        dst_dir: String,
        toast_msg: String,
    ) {
        if pairs.is_empty() {
            return;
        }
        let total_bytes: u64 = pairs.iter().map(|(_, _, sz)| (*sz).max(4096)).sum();
        let task_id = format!("tx-{}", self.transfers.len() + 1);
        let task = TransferTask {
            id: task_id.clone(),
            priority: TransferPriority::Foreground,
            source_path: label.clone(),
            destination_path: dst_dir,
            total_bytes,
            transferred_bytes: 0,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
            max_retries: 3,
            status: TransferStatus::InProgress,
            error_message: None,
        };
        self.transfers.insert(0, task);
        if self.transfers.len() > 24 {
            self.transfers.truncate(24);
        }
        self.bg_tasks_in_flight += 1;
        self.active_task_label = label.clone();
        self.active_task_progress = 5;
        let start_toast = format!("Started: {label}");
        self.show_toast("copy", start_toast);

        let ctrl = TransferControlHandle::new();
        self.transfer_controls.insert(task_id.clone(), ctrl.clone());
        let limiter = Arc::clone(&self.shared_limiter);
        let tx = self.worker_tx.clone();
        let remotes = self.remote_backends.clone();
        std::thread::spawn(move || {
            let local_vfs = LocalFsBackend::new();
            let mut transferred_so_far = 0u64;
            let mut ok_count = 0usize;
            let mut last_err: Option<String> = None;

            for (src_path, dst_path, item_sz) in pairs {
                if ctrl.is_cancelled() {
                    last_err = Some("Cancelled by user".to_string());
                    break;
                }
                let step_sz = item_sz.max(4096);
                let _ = tx.send(BgWorkerEvent::Progress {
                    task_id: task_id.clone(),
                    transferred: transferred_so_far,
                    total: total_bytes,
                    label: label.clone(),
                });
                let res = perform_bg_single_transfer(
                    &local_vfs,
                    &remotes,
                    &src_path,
                    &dst_path,
                    is_move,
                    (
                        &tx,
                        &task_id,
                        &label,
                        transferred_so_far,
                        total_bytes,
                        &ctrl,
                        &limiter,
                    ),
                );
                match res {
                    Ok(()) => {
                        transferred_so_far = transferred_so_far.saturating_add(step_sz);
                        ok_count += 1;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        if ctrl.is_cancelled() {
                            break;
                        }
                    }
                }
            }

            let _ = tx.send(BgWorkerEvent::Finished {
                task_id,
                ok_count,
                err_msg: last_err,
                toast_msg,
            });
        });
    }

    fn toggle_pause_transfer(&mut self, task_id: &str) {
        let Some(ctrl) = self.transfer_controls.get(task_id) else {
            return;
        };
        if ctrl.is_paused() {
            ctrl.resume();
            if let Some(t) = self.transfers.iter_mut().find(|t| t.id == task_id) {
                t.status = TransferStatus::InProgress;
            }
            self.logs.push(format!("[TRANSFER] Resumed task {task_id}"));
        } else {
            ctrl.pause();
            if let Some(t) = self.transfers.iter_mut().find(|t| t.id == task_id) {
                t.status = TransferStatus::Paused;
            }
            self.logs.push(format!("[TRANSFER] Paused task {task_id}"));
        }
    }

    fn cancel_transfer(&mut self, task_id: &str) {
        if let Some(ctrl) = self.transfer_controls.get(task_id) {
            ctrl.cancel();
        }
        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == task_id) {
            t.status = TransferStatus::Cancelled;
            t.error_message = Some("Cancelled by user".to_string());
        }
        self.logs
            .push(format!("[TRANSFER] Cancelled task {task_id}"));
    }

    fn show_toast(&mut self, kind: &str, message: impl Into<String>) {
        let msg = message.into();
        let is_err = kind == "error";
        if is_err {
            let log_line = format!("[ERROR] {msg}");
            self.logs.push(log_line);
        }
        self.status_banner_is_error = is_err;
        self.status_banner_text = msg;
        self.status_banner_ticks = if is_err { 110 } else { 65 };
        play_system_sound(kind);
    }

    fn ensure_archive_mounted(&mut self, arch_path: &str) {
        if self.archive_backends.contains_key(arch_path) {
            return;
        }
        match ArchiveBackend::open_from_disk(arch_path) {
            Ok(backend) => {
                let msg = format!("[ARCHIVE] Mounted virtual archive {arch_path}");
                self.logs.push(msg);
                self.archive_backends.insert(arch_path.to_string(), backend);
            }
            Err(err) => {
                let msg = format!("Failed to inspect archive: {err}");
                self.show_toast("error", msg);
            }
        }
    }

    fn request_bg_thumbnail(&self, path: &str) {
        if self.thumbnail_cache.contains_key(path) {
            return;
        }
        if self.pending_thumbs.borrow_mut().insert(path.to_string()) {
            self.thumb_queue.borrow_mut().push_back(path.to_string());
        }
    }

    fn drain_thumb_queue(&mut self) -> bool {
        let mut decoded_any = false;
        for _ in 0..3 {
            let Some(path) = self.thumb_queue.borrow_mut().pop_front() else {
                break;
            };
            self.pending_thumbs.borrow_mut().remove(&path);
            if self.thumbnail_cache.contains_key(&path) {
                continue;
            }
            if self.thumbnail_cache.len() > 2048 {
                self.thumbnail_cache.clear();
            }
            let p = Path::new(&path);
            let is_app = p.is_dir() && path.to_ascii_lowercase().ends_with(".app");
            let img = if is_app {
                load_macos_app_bundle_icon(&path)
            } else {
                Image::load_from_path(p).ok()
            };
            self.thumbnail_cache.insert(path, img);
            decoded_any = true;
        }
        decoded_any
    }

    fn request_bg_dir_size(&self, dir_path: &str) {
        if split_archive_uri(dir_path).is_some() {
            return;
        }
        if self.dir_size_cache.borrow().contains_key(dir_path) {
            return;
        }
        if !self
            .pending_dir_sizes
            .borrow_mut()
            .insert(dir_path.to_string())
        {
            return;
        }
        let tx = self.dir_size_tx.clone();
        let dir_owned = dir_path.to_string();
        if let Some((prefix, sub)) = split_remote_uri(dir_path) {
            if let Some(remote) = self.remote_backends.get(&prefix).cloned() {
                std::thread::spawn(move || {
                    if let Ok(sz) = remote.calculate_dir_size(&sub) {
                        let _ = tx.send((dir_owned, sz));
                    }
                });
            }
            return;
        }
        std::thread::spawn(move || {
            let vfs = LocalFsBackend::new();
            if let Ok(sz) = vfs.calculate_dir_size(&dir_owned) {
                let _ = tx.send((dir_owned, sz));
            }
        });
    }

    fn poll_bg_dir_sizes(&mut self) -> bool {
        let mut updated = false;
        while let Ok((dir_path, sz)) = self.dir_size_rx.try_recv() {
            self.pending_dir_sizes.borrow_mut().remove(&dir_path);
            self.dir_size_cache
                .borrow_mut()
                .insert(dir_path.clone(), sz);
            for entry in &mut self.left_entries {
                if entry.meta.path == dir_path {
                    entry.meta.size = Some(sz);
                    updated = true;
                }
            }
            for entry in &mut self.right_entries {
                if entry.meta.path == dir_path {
                    entry.meta.size = Some(sz);
                    updated = true;
                }
            }
            if let Some(parent) = vfs_parent_dir(&dir_path) {
                if let Some(cached) = self.dir_cache.borrow_mut().get_mut(&parent) {
                    for m in cached {
                        if m.path == dir_path {
                            m.size = Some(sz);
                            updated = true;
                        }
                    }
                }
            }
        }
        if updated {
            self.refresh_headers_and_inspector();
        }
        updated
    }

    fn invalidate_dir_cache(&self) {
        self.dir_cache.borrow_mut().clear();
        self.dir_size_cache.borrow_mut().clear();
        *self.left_search_key.borrow_mut() = (String::new(), String::new(), false);
        *self.right_search_key.borrow_mut() = (String::new(), String::new(), false);
    }

    fn effective_view_options_for_dir(&self, dir_path: &str) -> FolderViewOptions {
        self.prefs.effective_view_options(dir_path)
    }

    fn vfs_metadata(&self, path: &str) -> Option<FileMetadata> {
        if let Some(parent) = vfs_parent_dir(path) {
            if let Some(entries) = self.dir_cache.borrow().get(&parent) {
                if let Some(found) = entries.iter().find(|m| m.path == path) {
                    return Some(found.clone());
                }
            }
        }
        if let Some(rest) = path.strip_prefix("archive://") {
            let (arch_path, inner) = rest.split_once('#').unwrap_or((rest, "/"));
            let arch = self.archive_backends.get(arch_path)?;
            let mut meta = arch.metadata(inner).ok()?;
            meta.path = format!("archive://{arch_path}#{}", meta.path);
            return Some(meta);
        }
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self.remote_backends.get(&prefix)?;
            let mut meta = remote.metadata(&remote_sub).ok()?;
            meta.path = format!("{prefix}{}", meta.path);
            return Some(meta);
        }
        self.local_vfs.metadata(path).ok()
    }

    fn unique_keep_both_path(&self, dst_dir: &str, name: &str) -> String {
        let (stem, ext_sfx) = split_name_stem_ext(name);
        let mut idx = 2u32;
        loop {
            let cand_name = format!("{stem} {idx}{ext_sfx}");
            let cand_path = join_vfs_path(dst_dir, &cand_name);
            if self.vfs_metadata(&cand_path).is_none() {
                return cand_path;
            }
            idx += 1;
        }
    }

    fn active_conflict(&self) -> Option<(FileMetadata, FileMetadata, bool)> {
        let batch = self.pending_batch.as_ref()?;
        let (src, ex) = batch.current_collision.as_ref()?;
        Some((src.clone(), ex.clone(), batch.is_move))
    }

    fn start_tracked_task(
        &mut self,
        label: &str,
        dest: &str,
        total_bytes: u64,
        toast_msg: String,
        sound_kind: &str,
    ) {
        let total = total_bytes.max(100);
        let task = TransferTask {
            id: format!("tx-{}", self.transfers.len() + 1),
            priority: TransferPriority::Foreground,
            source_path: label.to_string(),
            destination_path: dest.to_string(),
            total_bytes: total,
            transferred_bytes: total / 5,
            chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
            max_retries: 3,
            status: TransferStatus::InProgress,
            error_message: None,
        };
        self.transfers.insert(0, task);
        if self.transfers.len() > 24 {
            self.transfers.truncate(24);
        }
        self.active_task_label = label.to_string();
        self.active_task_progress = 20;
        self.active_task_ticks = 8;
        self.show_toast(sound_kind, toast_msg);
    }

    fn selected_metadata_list(&self, is_right: bool) -> Vec<FileMetadata> {
        let (entries, sel_set, sel_idx) = if is_right {
            (
                &self.right_entries,
                &self.right_selected_indices,
                self.right_selected_idx,
            )
        } else {
            (
                &self.left_entries,
                &self.left_selected_indices,
                self.left_selected_idx,
            )
        };
        let mut indices: Vec<usize> = sel_set
            .iter()
            .copied()
            .filter(|&i| i < entries.len())
            .collect();
        if indices.is_empty() {
            if let Some(i) = sel_idx.filter(|&i| i < entries.len()) {
                indices.push(i);
            }
        }
        indices.sort_unstable();
        indices
            .into_iter()
            .map(|i| entries[i].meta.clone())
            .collect()
    }

    fn apply_row_selection(&mut self, is_right: bool, idx: usize, is_shift: bool, is_cmd: bool) {
        let len = if is_right {
            self.right_entries.len()
        } else {
            self.left_entries.len()
        };
        if idx >= len {
            return;
        }
        if is_right {
            self.app_state.active_pane = ActivePane::Right;
            let anchor = self
                .right_anchor_idx
                .or(self.right_selected_idx)
                .unwrap_or(idx)
                .min(len.saturating_sub(1));
            if is_shift {
                let lo = anchor.min(idx);
                let hi = anchor.max(idx);
                self.right_selected_indices = (lo..=hi).collect();
                self.right_selected_idx = Some(idx);
                self.right_anchor_idx = Some(anchor);
            } else if is_cmd {
                if !self.right_selected_indices.remove(&idx) {
                    self.right_selected_indices.insert(idx);
                }
                if self.right_selected_indices.is_empty() {
                    self.right_selected_indices.insert(idx);
                }
                self.right_selected_idx = Some(idx);
                self.right_anchor_idx = Some(idx);
            } else {
                self.right_selected_indices.clear();
                self.right_selected_indices.insert(idx);
                self.right_selected_idx = Some(idx);
                self.right_anchor_idx = Some(idx);
            }
        } else {
            self.app_state.active_pane = ActivePane::Left;
            let anchor = self
                .left_anchor_idx
                .or(self.left_selected_idx)
                .unwrap_or(idx)
                .min(len.saturating_sub(1));
            if is_shift {
                let lo = anchor.min(idx);
                let hi = anchor.max(idx);
                self.left_selected_indices = (lo..=hi).collect();
                self.left_selected_idx = Some(idx);
                self.left_anchor_idx = Some(anchor);
            } else if is_cmd {
                if !self.left_selected_indices.remove(&idx) {
                    self.left_selected_indices.insert(idx);
                }
                if self.left_selected_indices.is_empty() {
                    self.left_selected_indices.insert(idx);
                }
                self.left_selected_idx = Some(idx);
                self.left_anchor_idx = Some(idx);
            } else {
                self.left_selected_indices.clear();
                self.left_selected_indices.insert(idx);
                self.left_selected_idx = Some(idx);
                self.left_anchor_idx = Some(idx);
            }
        }
    }

    fn select_single_path_or_first(&mut self, is_right: bool, target_path: Option<&str>) {
        self.last_row_click = None;
        self.pending_rename_click = None;
        let len = if is_right {
            self.right_entries.len()
        } else {
            self.left_entries.len()
        };
        if len == 0 {
            if is_right {
                self.right_selected_indices.clear();
                self.right_selected_idx = None;
                self.right_anchor_idx = None;
            } else {
                self.left_selected_indices.clear();
                self.left_selected_idx = None;
                self.left_anchor_idx = None;
            }
            return;
        }
        let found_idx = target_path.and_then(|tp| {
            let entries = if is_right {
                &self.right_entries
            } else {
                &self.left_entries
            };
            entries.iter().position(|r| r.meta.path == tp)
        });
        let fallback_idx = if is_right {
            self.right_selected_idx
                .unwrap_or(0)
                .min(len.saturating_sub(1))
        } else {
            self.left_selected_idx
                .unwrap_or(0)
                .min(len.saturating_sub(1))
        };
        let idx = found_idx.unwrap_or(fallback_idx);
        if is_right {
            self.right_selected_indices.clear();
            self.right_selected_indices.insert(idx);
            self.right_selected_idx = Some(idx);
            self.right_anchor_idx = Some(idx);
        } else {
            self.left_selected_indices.clear();
            self.left_selected_indices.insert(idx);
            self.left_selected_idx = Some(idx);
            self.left_anchor_idx = Some(idx);
        }
        self.refresh_headers_and_inspector();
    }

    fn is_right_effective(&self) -> bool {
        self.app_state.dual_pane_enabled && self.app_state.active_pane == ActivePane::Right
    }

    fn backend_for_path(&self, path: &str) -> &dyn FileSystemBackend {
        if let Some((arch_path, _)) = split_archive_uri(path) {
            if let Some(arch) = self.archive_backends.get(&arch_path) {
                return arch;
            }
        }
        if let Some((prefix, _)) = split_remote_uri(path) {
            if let Some(remote) = self.remote_backends.get(&prefix) {
                return remote;
            }
        }
        &self.local_vfs
    }

    fn ensure_remote_connected(&mut self, prefix: &str) {
        if self.remote_backends.contains_key(prefix) {
            return;
        }
        let found_profile = self
            .saved_connections
            .connections
            .iter()
            .find(|p| profile_to_vfs_url(p).starts_with(prefix))
            .cloned();
        if let Some(profile) = found_profile {
            let secret = self.credential_store.get_secret(&profile.id).ok().flatten();
            match RemoteVfsBackend::connect_live(profile, secret) {
                Ok(remote) => {
                    let msg = format!("[CONNECT] Session opened for {prefix}");
                    self.logs.push(msg);
                    self.remote_backends.insert(prefix.to_string(), remote);
                }
                Err(err) => {
                    let msg = format!("[ERROR] Failed to connect to {prefix}: {err}");
                    self.logs.push(msg);
                }
            }
        }
    }

    fn vfs_create_dir(&self, path: &str) -> Result<(), String> {
        self.invalidate_dir_cache();
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote.create_dir(&remote_sub).map_err(|e| e.to_string());
        }
        self.local_vfs.create_dir(path).map_err(|e| e.to_string())
    }

    fn vfs_remove(&self, path: &str, recursive: bool) -> Result<(), String> {
        self.invalidate_dir_cache();
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote
                .remove(&remote_sub, recursive)
                .map_err(|e| e.to_string());
        }
        self.local_vfs
            .remove(path, recursive)
            .map_err(|e| e.to_string())
    }

    fn vfs_rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.invalidate_dir_cache();
        if let (Some((p1, sub1)), Some((p2, sub2))) = (split_remote_uri(from), split_remote_uri(to))
        {
            if p1 == p2 {
                let remote = self
                    .remote_backends
                    .get(&p1)
                    .ok_or_else(|| "Remote VFS not connected".to_string())?;
                return remote.rename(&sub1, &sub2).map_err(|e| e.to_string());
            }
        }
        self.local_vfs.rename(from, to).map_err(|e| e.to_string())
    }

    fn vfs_read_bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        if let Some((arch_path, inner)) = split_archive_uri(path) {
            if let Some(arch) = self.archive_backends.get(&arch_path) {
                return arch.read_bytes(&inner).map_err(|e| e.to_string());
            }
            let arch = ArchiveBackend::open_from_disk(&arch_path).map_err(|e| e.to_string())?;
            return arch.read_bytes(&inner).map_err(|e| e.to_string());
        }
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote.read_bytes(&remote_sub).map_err(|e| e.to_string());
        }
        self.local_vfs.read_bytes(path).map_err(|e| e.to_string())
    }

    fn vfs_write_bytes(&self, path: &str, data: &[u8]) -> Result<(), String> {
        if split_archive_uri(path).is_some() {
            return Err("Archive virtual filesystem is read-only".to_string());
        }
        self.invalidate_dir_cache();
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote
                .write_bytes(&remote_sub, data)
                .map_err(|e| e.to_string());
        }
        self.local_vfs
            .write_bytes(path, data)
            .map_err(|e| e.to_string())
    }

    fn vfs_set_permissions(&self, path: &str, mode: u32) -> Result<(), String> {
        if split_archive_uri(path).is_some() {
            return Err("Archive virtual filesystem is read-only".to_string());
        }
        self.invalidate_dir_cache();
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote
                .set_permissions(&remote_sub, mode)
                .map_err(|e| e.to_string());
        }
        self.local_vfs
            .set_permissions(path, mode)
            .map_err(|e| e.to_string())
    }

    fn vfs_create_symlink(&self, target: &str, link_path: &str) -> Result<(), String> {
        if split_archive_uri(target).is_some() || split_archive_uri(link_path).is_some() {
            return Err("Cannot create symlinks inside archives".to_string());
        }
        self.invalidate_dir_cache();
        if let Some((prefix, link_sub)) = split_remote_uri(link_path) {
            let target_sub = split_remote_uri(target)
                .filter(|(tp, _)| tp == &prefix)
                .map(|(_, s)| s)
                .unwrap_or_else(|| target.to_string());
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote
                .create_symlink(&target_sub, &link_sub)
                .map_err(|e| e.to_string());
        }
        self.local_vfs
            .create_symlink(target, link_path)
            .map_err(|e| e.to_string())
    }

    fn duplicate_entry(&self, meta: &FileMetadata) -> Result<String, String> {
        if split_archive_uri(&meta.path).is_some() {
            return Err("Archive virtual filesystem is read-only".to_string());
        }
        if split_remote_uri(&meta.path).is_some() {
            let parent = vfs_parent_dir(&meta.path)
                .ok_or_else(|| "Cannot determine remote parent directory".to_string())?;
            let (stem, ext_sfx) = split_name_stem_ext(&meta.name);
            let copy_name = format!("{stem} copy{ext_sfx}");
            let first_cand = join_vfs_path(&parent, &copy_name);
            let dst = if self.vfs_metadata(&first_cand).is_none() {
                first_cand
            } else {
                self.unique_keep_both_path(&parent, &copy_name)
            };
            self.vfs_copy_entry(&meta.path, &dst)?;
            return Ok(dst);
        }
        self.invalidate_dir_cache();
        duplicate_path_on_disk(&self.local_vfs, meta)
    }

    fn vfs_copy_entry(&self, src_path: &str, dst_path: &str) -> Result<(), String> {
        if split_archive_uri(dst_path).is_some() {
            return Err("Archive virtual filesystem is read-only".to_string());
        }
        self.invalidate_dir_cache();
        let src_slash = format!("{}/", src_path.trim_end_matches('/'));
        if dst_path == src_path || dst_path.starts_with(&src_slash) {
            return Err("Cannot copy or move a folder into itself".to_string());
        }
        let is_dir = if split_archive_uri(src_path).is_some() {
            self.vfs_metadata(src_path).is_some_and(|m| m.is_dir)
        } else if let Some((p, sub)) = split_remote_uri(src_path) {
            self.remote_backends
                .get(&p)
                .and_then(|r| r.metadata(&sub).ok())
                .is_some_and(|m| m.is_dir)
        } else {
            Path::new(src_path).is_dir()
        };
        if is_dir {
            if split_remote_uri(src_path).is_none()
                && split_archive_uri(src_path).is_none()
                && split_remote_uri(dst_path).is_none()
                && Path::new(src_path).is_dir()
                && copy_local_dir_preserving_bundle(src_path, dst_path).is_ok()
            {
                return Ok(());
            }
            self.vfs_create_dir(dst_path)?;
            let children = self.list_sorted_dir(
                src_path,
                true,
                "",
                SortColumn::Name,
                SortDirection::Ascending,
            );
            for child in children {
                let child_dst = join_vfs_path(dst_path, &child.name);
                self.vfs_copy_entry(&child.path, &child_dst)?;
            }
            return Ok(());
        }
        if let (Some((p1, sub1)), Some((p2, sub2))) =
            (split_remote_uri(src_path), split_remote_uri(dst_path))
        {
            if p1 == p2 {
                let remote = self
                    .remote_backends
                    .get(&p1)
                    .ok_or_else(|| "Remote VFS not connected".to_string())?;
                return remote.copy_file(&sub1, &sub2).map_err(|e| e.to_string());
            }
        }
        if split_archive_uri(src_path).is_some()
            || split_remote_uri(src_path).is_some()
            || split_remote_uri(dst_path).is_some()
        {
            let bytes = self.vfs_read_bytes(src_path)?;
            return self.vfs_write_bytes(dst_path, &bytes);
        }
        self.local_vfs
            .copy_file(src_path, dst_path)
            .map_err(|e| e.to_string())
    }

    fn vfs_move_entry(&self, src_path: &str, dst_path: &str) -> Result<(), String> {
        self.invalidate_dir_cache();
        let src_slash = format!("{}/", src_path.trim_end_matches('/'));
        if dst_path == src_path || dst_path.starts_with(&src_slash) {
            return Err("Cannot move a folder into itself".to_string());
        }
        if self.vfs_rename(src_path, dst_path).is_ok() {
            if split_remote_uri(dst_path).is_none() {
                sanitize_macos_app_bundle(Path::new(dst_path), cfg!(target_os = "macos"));
            }
            return Ok(());
        }
        self.vfs_copy_entry(src_path, dst_path)?;
        self.vfs_remove(src_path, true)?;
        Ok(())
    }

    fn current_pane_path(&self, is_right: bool) -> String {
        let tab = if is_right {
            self.app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
        } else {
            self.app_state.left_tabs.get(self.app_state.left_active_tab)
        };
        match tab {
            Some(t) => t.current_path.clone(),
            None => "/".to_string(),
        }
    }

    fn navigate_to_path(&mut self, is_right: bool, new_path: String) {
        let cur = self.current_pane_path(is_right);
        if cur != new_path {
            if is_right {
                let idx = self.app_state.right_active_tab;
                if let Some(tab) = self.app_state.right_tabs.get_mut(idx) {
                    tab.back_stack.push(cur);
                    tab.forward_stack.clear();
                }
            } else {
                let idx = self.app_state.left_active_tab;
                if let Some(tab) = self.app_state.left_tabs.get_mut(idx) {
                    tab.back_stack.push(cur);
                    tab.forward_stack.clear();
                }
            }
        }
        self.set_current_pane_path(is_right, new_path);
    }

    fn go_back(&mut self) {
        let is_right = self.is_right_effective();
        let cur = self.current_pane_path(is_right);
        let prev_opt = if is_right {
            let idx = self.app_state.right_active_tab;
            self.app_state
                .right_tabs
                .get_mut(idx)
                .and_then(|t| t.back_stack.pop())
        } else {
            let idx = self.app_state.left_active_tab;
            self.app_state
                .left_tabs
                .get_mut(idx)
                .and_then(|t| t.back_stack.pop())
        };
        if let Some(prev) = prev_opt {
            if is_right {
                let idx = self.app_state.right_active_tab;
                if let Some(tab) = self.app_state.right_tabs.get_mut(idx) {
                    tab.forward_stack.push(cur);
                }
            } else {
                let idx = self.app_state.left_active_tab;
                if let Some(tab) = self.app_state.left_tabs.get_mut(idx) {
                    tab.forward_stack.push(cur);
                }
            }
            self.set_current_pane_path(is_right, prev);
        }
    }

    fn go_forward(&mut self) {
        let is_right = self.is_right_effective();
        let cur = self.current_pane_path(is_right);
        let next_opt = if is_right {
            let idx = self.app_state.right_active_tab;
            self.app_state
                .right_tabs
                .get_mut(idx)
                .and_then(|t| t.forward_stack.pop())
        } else {
            let idx = self.app_state.left_active_tab;
            self.app_state
                .left_tabs
                .get_mut(idx)
                .and_then(|t| t.forward_stack.pop())
        };
        if let Some(next) = next_opt {
            if is_right {
                let idx = self.app_state.right_active_tab;
                if let Some(tab) = self.app_state.right_tabs.get_mut(idx) {
                    tab.back_stack.push(cur);
                }
            } else {
                let idx = self.app_state.left_active_tab;
                if let Some(tab) = self.app_state.left_tabs.get_mut(idx) {
                    tab.back_stack.push(cur);
                }
            }
            self.set_current_pane_path(is_right, next);
        }
    }

    fn set_current_pane_path(&mut self, is_right: bool, new_path: String) {
        if let Some((arch_path, _)) = split_archive_uri(&new_path) {
            self.ensure_archive_mounted(&arch_path);
        } else if let Some((prefix, _)) = split_remote_uri(&new_path) {
            self.ensure_remote_connected(&prefix);
        }
        let title = self.format_tab_title(&new_path);
        let vfs_url = if new_path.contains("://") {
            new_path.clone()
        } else {
            format!("file://{new_path}")
        };

        if is_right {
            let idx = self.app_state.right_active_tab;
            if let Some(tab) = self.app_state.right_tabs.get_mut(idx) {
                if !tab.inline_filter.is_empty() {
                    tab.inline_filter.clear();
                }
                tab.current_path = new_path.clone();
                tab.vfs_url = vfs_url;
                tab.title = title.clone();
                tab.expanded_folders.clear();
            }
            self.right_selected_idx = Some(0);
            self.right_selected_indices.clear();
            self.right_selected_indices.insert(0);
            self.right_anchor_idx = Some(0);
            self.right_virtual_top_row = 0;
        } else {
            let idx = self.app_state.left_active_tab;
            if let Some(tab) = self.app_state.left_tabs.get_mut(idx) {
                if !tab.inline_filter.is_empty() {
                    tab.inline_filter.clear();
                }
                tab.current_path = new_path.clone();
                tab.vfs_url = vfs_url;
                tab.title = title.clone();
                tab.expanded_folders.clear();
            }
            self.left_selected_idx = Some(0);
            self.left_selected_indices.clear();
            self.left_selected_indices.insert(0);
            self.left_anchor_idx = Some(0);
            self.left_virtual_top_row = 0;
        }
        self.cancel_pane_search(is_right);
        self.app_state.toolbar.search_query.clear();
        self.app_state.toolbar.active_folder_title = title;
        self.reload_pane_entries(is_right);
        self.sync_selected_sidebar_to_active_dir();
    }

    fn show_entry_in_enclosing_folder(&mut self, is_right: bool, target_path: &str) {
        let enclosing_dir =
            vfs_parent_dir(target_path).unwrap_or_else(|| self.current_pane_path(is_right));
        self.cancel_pane_search(is_right);
        self.app_state.toolbar.search_query.clear();
        let cur = self.current_pane_path(is_right);
        if cur != enclosing_dir {
            self.navigate_to_path(is_right, enclosing_dir);
        } else {
            self.set_current_pane_path(is_right, enclosing_dir);
        }
        self.select_single_path_or_first(is_right, Some(target_path));
        self.sync_terminal_cwd();
        self.refresh_headers_and_inspector();
    }

    fn list_raw_dir_cached(&self, dir_path: &str) -> Vec<FileMetadata> {
        if let Some(cached) = self.dir_cache.borrow().get(dir_path) {
            return cached.clone();
        }
        let raw_items = if let Some(rest) = dir_path.strip_prefix("archive://") {
            let (arch_path, inner) = rest.split_once('#').unwrap_or((rest, "/"));
            if let Some(arch) = self.archive_backends.get(arch_path) {
                let mut entries = arch.list_directory(inner).unwrap_or_default();
                for entry in &mut entries {
                    entry.path = format!("archive://{arch_path}#{}", entry.path);
                }
                entries
            } else {
                Vec::new()
            }
        } else if let Some((prefix, remote_sub)) = split_remote_uri(dir_path) {
            if let Some(remote) = self.remote_backends.get(&prefix) {
                let mut entries = remote.list_directory(&remote_sub).unwrap_or_default();
                for entry in &mut entries {
                    entry.path = format!("{prefix}{}", entry.path);
                }
                entries
            } else {
                Vec::new()
            }
        } else {
            self.local_vfs.list_directory(dir_path).unwrap_or_default()
        };
        let mut cache = self.dir_cache.borrow_mut();
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(dir_path.to_string(), raw_items.clone());
        drop(cache);
        if let Some(fp) = compute_local_dir_fingerprint(dir_path) {
            self.dir_fingerprints
                .borrow_mut()
                .insert(dir_path.to_string(), fp);
        }
        raw_items
    }

    fn list_sorted_dir(
        &self,
        dir_path: &str,
        show_hidden: bool,
        query: &str,
        sort_col: SortColumn,
        sort_dir: SortDirection,
    ) -> Vec<FileMetadata> {
        let mut vo = self.effective_view_options_for_dir(dir_path);
        if show_hidden {
            vo.show_hidden_files = true;
        }
        self.list_sorted_dir_for_view(dir_path, query, sort_col, sort_dir, &vo)
    }

    fn list_sorted_dir_for_view(
        &self,
        dir_path: &str,
        query: &str,
        sort_col: SortColumn,
        sort_dir: SortDirection,
        vo: &FolderViewOptions,
    ) -> Vec<FileMetadata> {
        let show_hidden = self.app_state.toolbar.show_hidden_files || vo.show_hidden_files;
        let raw_items = self.list_raw_dir_cached(dir_path);
        let q_low = query.trim().to_lowercase();
        let mut filtered: Vec<FileMetadata> = raw_items
            .into_iter()
            .filter(|m| show_hidden || !m.is_hidden)
            .filter(|m| matches_search_query_lower(m, &q_low))
            .collect();

        if vo.calculate_all_sizes {
            for item in &mut filtered {
                if item.is_dir && item.size.is_none() {
                    let cached_sz = self.dir_size_cache.borrow().get(&item.path).copied();
                    if let Some(sz) = cached_sz {
                        item.size = Some(sz);
                    } else if split_remote_uri(&item.path).is_none()
                        && split_archive_uri(&item.path).is_none()
                    {
                        self.request_bg_dir_size(&item.path);
                    }
                }
            }
        }

        sort_metadata_slice_with_options(
            &mut filtered,
            sort_col,
            sort_dir,
            vo.show_folders_on_top,
            vo.folders_always_alpha,
            vo.treat_packages_as_folders,
        );
        filtered
    }

    fn collect_flattened_rows(
        &self,
        dir_path: &str,
        depth: i32,
        expanded_set: &HashSet<String>,
        opts: (&str, SortColumn, SortDirection),
        out: &mut Vec<FlattenedRowEntry>,
    ) {
        let vo = self.effective_view_options_for_dir(dir_path);
        let query = if depth == 0 { opts.0 } else { "" };
        let items = self.list_sorted_dir_for_view(dir_path, query, opts.1, opts.2, &vo);
        for meta in items {
            let is_browsable = is_browsable_folder_with_opts(&meta, vo.treat_packages_as_folders);
            let is_exp = is_browsable && expanded_set.contains(&meta.path);
            let child_path = meta.path.clone();
            out.push(FlattenedRowEntry {
                meta,
                depth,
                is_expanded: is_exp,
            });
            if is_exp && depth < 6 {
                self.collect_flattened_rows(&child_path, depth + 1, expanded_set, opts, out);
            }
        }
    }

    fn cancel_pane_search(&mut self, is_right: bool) {
        if is_right {
            self.right_search_gen.fetch_add(1, Ordering::Relaxed);
            self.right_search_running = false;
            *self.right_search_key.borrow_mut() = (String::new(), String::new(), false);
            self.right_search_results.clear();
        } else {
            self.left_search_gen.fetch_add(1, Ordering::Relaxed);
            self.left_search_running = false;
            *self.left_search_key.borrow_mut() = (String::new(), String::new(), false);
            self.left_search_results.clear();
        }
    }

    fn spawn_recursive_search(
        &mut self,
        is_right: bool,
        root_path: &str,
        query: &str,
        show_hidden: bool,
    ) {
        let q_low = query.trim().to_lowercase();
        let initial_matches: Vec<FileMetadata> = self
            .list_raw_dir_cached(root_path)
            .into_iter()
            .filter(|m| show_hidden || !m.is_hidden)
            .filter(|m| matches_search_query_lower(m, &q_low))
            .collect();

        let key = (root_path.to_string(), q_low.clone(), show_hidden);
        let gen_arc = if is_right {
            *self.right_search_key.borrow_mut() = key;
            self.right_search_results = initial_matches;
            self.right_search_running = true;
            Arc::clone(&self.right_search_gen)
        } else {
            *self.left_search_key.borrow_mut() = key;
            self.left_search_results = initial_matches;
            self.left_search_running = true;
            Arc::clone(&self.left_search_gen)
        };
        let my_gen = gen_arc.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let tx = self.search_tx.clone();
        let color_labels = self.prefs.file_color_labels.clone();

        if let Some((prefix, remote_sub)) = split_remote_uri(root_path) {
            let remote_opt = self
                .remote_backends
                .get(&prefix)
                .or_else(|| self.remote_backends.get(prefix.trim_end_matches('/')))
                .cloned();
            if let Some(remote) = remote_opt {
                let q_owned = q_low;
                std::thread::spawn(move || {
                    if gen_arc.load(Ordering::Relaxed) != my_gen {
                        return;
                    }
                    let found = remote
                        .search_recursive(&remote_sub, &q_owned, show_hidden, 2500)
                        .unwrap_or_default();
                    if gen_arc.load(Ordering::Relaxed) != my_gen {
                        return;
                    }
                    let mut items = Vec::with_capacity(found.len());
                    for mut entry in found {
                        entry.path = format!("{prefix}{}", entry.path);
                        if let Some(&lbl) = color_labels.get(&entry.path) {
                            entry.color_label = lbl;
                        }
                        items.push(entry);
                    }
                    if !items.is_empty() {
                        let _ = tx.send(BgSearchEvent::Batch {
                            is_right,
                            gen_id: my_gen,
                            items,
                        });
                    }
                    let _ = tx.send(BgSearchEvent::Finished {
                        is_right,
                        gen_id: my_gen,
                    });
                });
            } else if is_right {
                self.right_search_running = false;
            } else {
                self.left_search_running = false;
            }
            return;
        }

        let root_owned = root_path.to_string();
        std::thread::spawn(move || {
            run_local_recursive_bfs_search(LocalBfsSearchParams {
                is_right,
                my_gen,
                gen_arc: &gen_arc,
                root_dir: &root_owned,
                q_low: &q_low,
                show_hidden,
                color_labels: &color_labels,
                tx: &tx,
            });
        });
    }

    fn poll_search_events(&mut self) -> bool {
        let mut updated_left = false;
        let mut updated_right = false;
        while let Ok(ev) = self.search_rx.try_recv() {
            match ev {
                BgSearchEvent::Batch {
                    is_right,
                    gen_id,
                    items,
                } => {
                    let cur_gen = if is_right {
                        self.right_search_gen.load(Ordering::Relaxed)
                    } else {
                        self.left_search_gen.load(Ordering::Relaxed)
                    };
                    if gen_id != cur_gen {
                        continue;
                    }
                    let results = if is_right {
                        &mut self.right_search_results
                    } else {
                        &mut self.left_search_results
                    };
                    let mut existing: HashSet<String> =
                        results.iter().map(|m| m.path.clone()).collect();
                    let mut added = false;
                    for item in items {
                        if results.len() >= 2500 {
                            break;
                        }
                        if existing.insert(item.path.clone()) {
                            results.push(item);
                            added = true;
                        }
                    }
                    if added {
                        if is_right {
                            updated_right = true;
                        } else {
                            updated_left = true;
                        }
                    }
                }
                BgSearchEvent::Finished { is_right, gen_id } => {
                    let cur_gen = if is_right {
                        self.right_search_gen.load(Ordering::Relaxed)
                    } else {
                        self.left_search_gen.load(Ordering::Relaxed)
                    };
                    if gen_id == cur_gen {
                        if is_right {
                            self.right_search_running = false;
                            updated_right = true;
                        } else {
                            self.left_search_running = false;
                            updated_left = true;
                        }
                    }
                }
            }
        }
        if updated_left {
            self.rebuild_search_pane_entries(false);
        }
        if updated_right {
            self.rebuild_search_pane_entries(true);
        }
        if updated_left || updated_right {
            self.refresh_headers_and_inspector();
            return true;
        }
        false
    }

    fn rebuild_search_pane_entries(&mut self, is_right: bool) {
        let (sort_col, sort_dir) = if is_right {
            self.app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
                .map(|t| (t.sort_column, t.sort_direction))
                .unwrap_or((SortColumn::Name, SortDirection::Ascending))
        } else {
            self.app_state
                .left_tabs
                .get(self.app_state.left_active_tab)
                .map(|t| (t.sort_column, t.sort_direction))
                .unwrap_or((SortColumn::Name, SortDirection::Ascending))
        };

        let sel_paths: HashSet<String> = if is_right {
            self.right_selected_indices
                .iter()
                .filter_map(|&i| self.right_entries.get(i).map(|r| r.meta.path.clone()))
                .collect()
        } else {
            self.left_selected_indices
                .iter()
                .filter_map(|&i| self.left_entries.get(i).map(|r| r.meta.path.clone()))
                .collect()
        };

        let pane_path = self.current_pane_path(is_right);
        let vo = self.effective_view_options_for_dir(&pane_path);
        let mut sorted = if is_right {
            self.right_search_results.clone()
        } else {
            self.left_search_results.clone()
        };
        sort_metadata_slice_with_options(
            &mut sorted,
            sort_col,
            sort_dir,
            vo.show_folders_on_top,
            vo.folders_always_alpha,
            vo.treat_packages_as_folders,
        );
        let flattened: Vec<FlattenedRowEntry> = sorted
            .into_iter()
            .map(|meta| FlattenedRowEntry {
                meta,
                depth: 0,
                is_expanded: false,
            })
            .collect();

        if is_right {
            self.right_entries = flattened;
            let len = self.right_entries.len();
            let mut restored = HashSet::new();
            for (i, r) in self.right_entries.iter().enumerate() {
                if sel_paths.contains(&r.meta.path) {
                    restored.insert(i);
                }
            }
            if len == 0 {
                self.right_selected_indices.clear();
                self.right_selected_idx = None;
                self.right_anchor_idx = None;
            } else if !restored.is_empty() {
                let first = restored.iter().copied().min();
                self.right_selected_indices = restored;
                self.right_selected_idx = first;
            } else {
                self.right_selected_indices.clear();
                self.right_selected_indices.insert(0);
                self.right_selected_idx = Some(0);
                self.right_anchor_idx = Some(0);
            }
        } else {
            self.left_entries = flattened;
            let len = self.left_entries.len();
            let mut restored = HashSet::new();
            for (i, r) in self.left_entries.iter().enumerate() {
                if sel_paths.contains(&r.meta.path) {
                    restored.insert(i);
                }
            }
            if len == 0 {
                self.left_selected_indices.clear();
                self.left_selected_idx = None;
                self.left_anchor_idx = None;
            } else if !restored.is_empty() {
                let first = restored.iter().copied().min();
                self.left_selected_indices = restored;
                self.left_selected_idx = first;
            } else {
                self.left_selected_indices.clear();
                self.left_selected_indices.insert(0);
                self.left_selected_idx = Some(0);
                self.left_anchor_idx = Some(0);
            }
        }
    }

    fn reload_pane_entries(&mut self, is_right: bool) {
        let path = self.current_pane_path(is_right);
        if let Some((arch_path, _)) = split_archive_uri(&path) {
            self.ensure_archive_mounted(&arch_path);
        } else if let Some((prefix, _)) = split_remote_uri(&path) {
            self.ensure_remote_connected(&prefix);
        }
        let vo = self.effective_view_options_for_dir(&path);
        let show_hidden = self.app_state.toolbar.show_hidden_files || vo.show_hidden_files;

        let active_tab = if is_right {
            self.app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
        } else {
            self.app_state.left_tabs.get(self.app_state.left_active_tab)
        };
        let (sort_col, sort_dir, inline_filter, expanded, view_mode) = match active_tab {
            Some(t) => (
                t.sort_column,
                t.sort_direction,
                t.inline_filter.clone(),
                t.expanded_folders.clone(),
                t.view_mode,
            ),
            None => (
                SortColumn::Name,
                SortDirection::Ascending,
                String::new(),
                HashSet::new(),
                PaneViewMode::List,
            ),
        };

        let trimmed_q = inline_filter.trim().to_lowercase();
        let mut flattened = Vec::new();
        if !trimmed_q.is_empty() {
            let desired_key = (path.clone(), trimmed_q.clone(), show_hidden);
            let same_key = if is_right {
                *self.right_search_key.borrow() == desired_key
            } else {
                *self.left_search_key.borrow() == desired_key
            };
            if !same_key {
                self.spawn_recursive_search(is_right, &path, &trimmed_q, show_hidden);
            }
            let mut results = if is_right {
                self.right_search_results.clone()
            } else {
                self.left_search_results.clone()
            };
            sort_metadata_slice_with_options(
                &mut results,
                sort_col,
                sort_dir,
                vo.show_folders_on_top,
                vo.folders_always_alpha,
                vo.treat_packages_as_folders,
            );
            for meta in results {
                flattened.push(FlattenedRowEntry {
                    meta,
                    depth: 0,
                    is_expanded: false,
                });
            }
        } else {
            self.cancel_pane_search(is_right);
            let empty_expanded = HashSet::new();
            let active_expanded = if view_mode == PaneViewMode::Tree {
                &expanded
            } else {
                &empty_expanded
            };
            let opts = ("", sort_col, sort_dir);
            self.collect_flattened_rows(&path, 0, active_expanded, opts, &mut flattened);
        }

        let parent_items = if view_mode == PaneViewMode::Columns {
            vfs_parent_dir(&path)
                .filter(|p| p != &path)
                .map(|parent_path| {
                    self.list_sorted_dir(
                        &parent_path,
                        show_hidden,
                        "",
                        SortColumn::Name,
                        SortDirection::Ascending,
                    )
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        if is_right {
            self.right_entries = flattened;
            self.right_parent_entries = parent_items;
            let len = self.right_entries.len();
            self.right_selected_indices.retain(|&i| i < len);
            let out_of_bounds = self.right_selected_idx.is_none_or(|sel| sel >= len);
            if len == 0 {
                self.right_selected_idx = None;
                self.right_anchor_idx = None;
            } else if out_of_bounds {
                self.right_selected_idx = Some(0);
                self.right_anchor_idx = Some(0);
                self.right_selected_indices.insert(0);
            } else if self.right_selected_indices.is_empty() {
                if let Some(sel) = self.right_selected_idx {
                    self.right_selected_indices.insert(sel);
                }
            }
        } else {
            self.left_entries = flattened;
            self.left_parent_entries = parent_items;
            let len = self.left_entries.len();
            self.left_selected_indices.retain(|&i| i < len);
            let out_of_bounds = self.left_selected_idx.is_none_or(|sel| sel >= len);
            if len == 0 {
                self.left_selected_idx = None;
                self.left_anchor_idx = None;
            } else if out_of_bounds {
                self.left_selected_idx = Some(0);
                self.left_anchor_idx = Some(0);
                self.left_selected_indices.insert(0);
            } else if self.left_selected_indices.is_empty() {
                if let Some(sel) = self.left_selected_idx {
                    self.left_selected_indices.insert(sel);
                }
            }
        }
        self.sync_terminal_cwd();
        self.refresh_headers_and_inspector();
    }

    fn selected_metadata(&self) -> Option<&FileMetadata> {
        if self.is_right_effective() {
            let idx = self.right_selected_idx?;
            self.right_entries.get(idx).map(|r| &r.meta)
        } else {
            let idx = self.left_selected_idx?;
            self.left_entries.get(idx).map(|r| &r.meta)
        }
    }

    fn refresh_headers_and_inspector(&mut self) {
        let left_path = self.current_pane_path(false);
        let right_path = self.current_pane_path(true);

        let root_vol_name = crate::resolve_dynamic_root_label();
        let left_vol =
            if split_remote_uri(&left_path).is_some() || split_archive_uri(&left_path).is_some() {
                None
            } else {
                self.local_vfs
                    .volume_stats(&left_path)
                    .or_else(|_| self.local_vfs.volume_stats("/"))
                    .ok()
            };
        let r_rem =
            split_remote_uri(&right_path).is_some() || split_archive_uri(&right_path).is_some();
        let right_vol = if !self.app_state.dual_pane_enabled || r_rem {
            None
        } else {
            self.local_vfs
                .volume_stats(&right_path)
                .or_else(|_| self.local_vfs.volume_stats("/"))
                .ok()
        };

        let left_sel: Vec<&FileMetadata> = self
            .left_selected_indices
            .iter()
            .filter_map(|&i| self.left_entries.get(i).map(|r| &r.meta))
            .collect();
        let left_hdr_sel: &[&FileMetadata] = if left_sel.len() == 1 && left_sel[0].size.is_none() {
            &[]
        } else {
            &left_sel
        };
        self.app_state.left_header = crate::PathAndStatusHeader::build(
            Some(root_vol_name),
            &left_path,
            self.left_entries.len(),
            left_hdr_sel,
            left_vol,
        );

        let right_sel: Vec<&FileMetadata> = self
            .right_selected_indices
            .iter()
            .filter_map(|&i| self.right_entries.get(i).map(|r| &r.meta))
            .collect();
        let right_hdr_sel: &[&FileMetadata] = if right_sel.len() == 1 && right_sel[0].size.is_none()
        {
            &[]
        } else {
            &right_sel
        };
        self.app_state.right_header = crate::PathAndStatusHeader::build(
            Some(root_vol_name),
            &right_path,
            self.right_entries.len(),
            right_hdr_sel,
            right_vol,
        );

        let is_right_active = self.is_right_effective();
        let active_path = if is_right_active {
            &right_path
        } else {
            &left_path
        };
        let active_filter = if is_right_active {
            self.app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
                .map(|t| t.inline_filter.trim().to_string())
                .unwrap_or_default()
        } else {
            self.app_state
                .left_tabs
                .get(self.app_state.left_active_tab)
                .map(|t| t.inline_filter.trim().to_string())
                .unwrap_or_default()
        };
        let active_title = if !active_filter.is_empty() {
            format!("Search: \"{active_filter}\"")
        } else if split_remote_uri(active_path).is_some() {
            self.format_tab_title(active_path)
        } else {
            vfs_display_title(active_path)
        };
        let active_mode = if is_right_active {
            self.app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
                .map(|t| t.view_mode)
                .unwrap_or(PaneViewMode::Icons)
        } else {
            self.app_state
                .left_tabs
                .get(self.app_state.left_active_tab)
                .map(|t| t.view_mode)
                .unwrap_or(PaneViewMode::List)
        };
        self.app_state.toolbar.active_folder_title = active_title;
        self.app_state.toolbar.search_query = active_filter;
        self.app_state.toolbar.active_view_mode = active_mode;

        let active_sel_count = if is_right_active {
            right_sel.len()
        } else {
            left_sel.len()
        };

        if let Some(meta) = self.selected_metadata().cloned() {
            let need_sample = self.app_state.inspector_open
                && !meta.is_dir
                && split_remote_uri(&meta.path).is_none();
            let sample = if need_sample {
                let vfs = self.backend_for_path(&meta.path);
                vfs.read_range(&meta.path, 0, 512).ok()
            } else {
                None
            };
            let modified = meta.formatted_iso_datetime();
            let mut vm = FileInspectorViewModel::from_metadata(&meta, &modified, sample.as_deref());
            if active_sel_count > 1 {
                let total_sz: u64 = if is_right_active {
                    right_sel.iter().filter_map(|m| m.size).sum()
                } else {
                    left_sel.iter().filter_map(|m| m.size).sum()
                };
                let sz_str = manifold_vfs::format_bytes_comma(total_sz);
                vm.title = format!("{active_sel_count} Items Selected");
                vm.subtitle = format!("Multiple Selection - {sz_str}");
                vm.general.size = sz_str;
            }
            self.app_state.inspector_vm = Some(vm);
        } else {
            self.app_state.inspector_vm = None;
        }
    }

    fn stage_remote_file_for_edit(&mut self, meta: &FileMetadata) -> Option<String> {
        let (prefix, remote_sub) = split_remote_uri(&meta.path)?;
        self.ensure_remote_connected(&prefix);
        let remote = self
            .remote_backends
            .get(&prefix)
            .or_else(|| self.remote_backends.get(prefix.trim_end_matches('/')))?
            .clone();
        let sess_id = remote.profile().id.clone();
        let staged = self
            .remote_edit_staging
            .stage_for_external_edit(&sess_id, &remote_sub, &remote)
            .ok()?;
        let local_str = staged.to_string_lossy().to_string();
        let msg = format!("[STAGING] Staged {} -> {local_str}", meta.path);
        self.logs.push(msg);
        Some(local_str)
    }

    fn open_entry_at(&mut self, is_right: bool, row_idx: Option<usize>) {
        let idx_opt = row_idx.or(if is_right {
            self.right_selected_idx
        } else {
            self.left_selected_idx
        });
        let Some(idx) = idx_opt else {
            return;
        };
        let entry_opt = if is_right {
            self.right_entries.get(idx).map(|r| r.meta.clone())
        } else {
            self.left_entries.get(idx).map(|r| r.meta.clone())
        };
        let Some(meta) = entry_opt else {
            return;
        };

        let pane_path = self.current_pane_path(is_right);
        let vo = self.effective_view_options_for_dir(&pane_path);
        if is_browsable_folder_with_opts(&meta, vo.treat_packages_as_folders) {
            self.last_nav_instant = Some(Instant::now());
            let msg = format!("[OPEN] Directory -> {}", meta.path);
            self.logs.push(msg);
            self.navigate_to_path(is_right, meta.path);
        } else if let Some((arch_path, inner)) = split_archive_uri(&meta.path) {
            self.ensure_archive_mounted(&arch_path);
            let res = self
                .archive_backends
                .get(&arch_path)
                .ok_or_else(|| "Archive not mounted".to_string())
                .and_then(|a| a.read_bytes(&inner).map_err(|e| e.to_string()));
            match res {
                Ok(bytes) => {
                    if let Ok(dir) = manifold_vfs::secure_runtime_subdir("archive-preview") {
                        let safe_name = meta.name.replace(['/', '\\', '\0'], "_");
                        let dst = dir.join(safe_name);
                        if std::fs::write(&dst, bytes).is_ok() {
                            let s = dst.to_string_lossy().to_string();
                            open_with_system_handler(&s);
                        }
                    }
                }
                Err(err) => self.show_toast("error", format!("Archive extract failed: {err}")),
            }
        } else if split_remote_uri(&meta.path).is_some() {
            if let Some(staged_path) = self.stage_remote_file_for_edit(&meta) {
                let msg = format!("[OPEN] Remote file staged -> {staged_path}");
                self.logs.push(msg);
                open_with_system_handler(&staged_path);
            }
        } else {
            let msg = format!("[OPEN] File -> {}", meta.path);
            self.logs.push(msg);
            open_with_system_handler(&meta.path);
        }
    }
}

const fn play_system_sound(_kind: &str) {}

fn sync_window_from_runtime(window: &ManifoldWindow, rt: &mut GuiRuntime) {
    apply_preferences_to_window(window, &rt.prefs);
    let folder_title = rt.app_state.toolbar.active_folder_title.clone();
    let show_hidden = rt.app_state.toolbar.show_hidden_files;
    let dual_mode = rt.app_state.dual_pane_enabled;
    let is_right_active = rt.is_right_effective();
    let can_back = if is_right_active {
        rt.app_state
            .right_tabs
            .get(rt.app_state.right_active_tab)
            .is_some_and(|t| !t.back_stack.is_empty())
    } else {
        rt.app_state
            .left_tabs
            .get(rt.app_state.left_active_tab)
            .is_some_and(|t| !t.back_stack.is_empty())
    };
    let can_fwd = if is_right_active {
        rt.app_state
            .right_tabs
            .get(rt.app_state.right_active_tab)
            .is_some_and(|t| !t.forward_stack.is_empty())
    } else {
        rt.app_state
            .left_tabs
            .get(rt.app_state.left_active_tab)
            .is_some_and(|t| !t.forward_stack.is_empty())
    };

    let layout_int = match rt.app_state.panes_layout {
        PanesLayout::Single => 0,
        PanesLayout::SplitHorizontal => 1,
        PanesLayout::SplitVertical => 2,
    };

    let is_task_active = rt.active_task_ticks > 0 || rt.bg_tasks_in_flight > 0;
    window.set_active_folder_title(folder_title.into());
    window.set_show_hidden_files(show_hidden);
    if window.get_search_query().as_str() != rt.app_state.toolbar.search_query {
        window.set_search_query(rt.app_state.toolbar.search_query.clone().into());
    }
    window.set_dual_pane_mode(dual_mode);
    window.set_panes_layout(layout_int);
    window.set_split_ratio(rt.app_state.split_ratio);
    window.set_right_pane_active(is_right_active);
    window.set_can_go_back(can_back);
    window.set_can_go_forward(can_fwd);
    window.set_has_clipboard_item(!rt.clipboard_paths.is_empty());
    window.set_task_running(is_task_active);
    window.set_spinner_frame(rt.spinner_frame);
    window.set_active_task_label(rt.active_task_label.clone().into());
    window.set_active_task_progress(rt.active_task_progress);
    let banner = if rt.status_banner_ticks > 0 {
        rt.status_banner_text.as_str()
    } else {
        ""
    };
    window.set_status_banner_text(banner.into());
    window.set_status_banner_is_error(rt.status_banner_is_error && rt.status_banner_ticks > 0);
    sync_batch_rename_favorites_to_window(window, rt);

    // Embedded Bottom Terminal state sync
    sync_terminal_to_window(window, rt);

    let active_dir = rt.current_pane_path(is_right_active);
    window.set_current_folder_is_favorite(rt.is_path_in_favorites(&active_dir));

    // Sidebar model
    let mut sb_entries = Vec::new();
    let mut sb_targets = Vec::new();
    let mut fav_min_idx: i32 = -1;
    let mut fav_max_idx: i32 = -1;

    for section in &rt.app_state.sidebar_sections {
        sb_entries.push(SidebarEntrySlint {
            id: SharedString::from(&section.title),
            section: SharedString::from(&section.title),
            icon_kind: SharedString::default(),
            is_header: true,
            label: SharedString::from(&section.title),
            badge: SharedString::default(),
            can_eject: false,
            indent_level: 0,
            is_selected: false,
            has_color_tag: false,
            color_tag_brush: Color::from_argb_u8(0, 0, 0, 0).into(),
        });
        sb_targets.push((section.title.clone(), String::new()));

        for item in &section.items {
            let cur_sb_idx = sb_entries.len() as i32;
            if section.title == "Favorites" {
                if fav_min_idx < 0 {
                    fav_min_idx = cur_sb_idx;
                }
                fav_max_idx = cur_sb_idx;
            }
            let target_path = vfs_url_to_target(&item.vfs_url);
            let badge_str = item.capacity_badge.as_deref().unwrap_or("");
            let is_sel = match &rt.selected_sidebar_id {
                Some(sel_id) => item.id == *sel_id,
                None => !target_path.is_empty() && target_path == active_dir,
            };
            let color_lbl = rt
                .sidebar_colors
                .get(&item.id)
                .copied()
                .unwrap_or(ColorLabel::None);
            let c_brush = color_label_to_slint_color(color_lbl);
            sb_entries.push(SidebarEntrySlint {
                id: SharedString::from(&item.id),
                section: SharedString::from(&section.title),
                icon_kind: SharedString::from(&item.icon_kind),
                is_header: false,
                label: SharedString::from(&item.label),
                badge: SharedString::from(badge_str),
                can_eject: item.can_eject,
                indent_level: 0,
                is_selected: is_sel,
                has_color_tag: color_lbl != ColorLabel::None,
                color_tag_brush: c_brush.into(),
            });
            sb_targets.push((item.id.clone(), target_path));

            for child in &item.children {
                let child_target = vfs_url_to_target(&child.vfs_url);
                let c_badge = child.capacity_badge.as_deref().unwrap_or("");
                let c_sel = match &rt.selected_sidebar_id {
                    Some(sel_id) => child.id == *sel_id,
                    None => !child_target.is_empty() && child_target == active_dir,
                };
                let ch_color = rt
                    .sidebar_colors
                    .get(&child.id)
                    .copied()
                    .unwrap_or(ColorLabel::None);
                let ch_brush = color_label_to_slint_color(ch_color);
                sb_entries.push(SidebarEntrySlint {
                    id: SharedString::from(&child.id),
                    section: SharedString::from(&section.title),
                    icon_kind: SharedString::from(&child.icon_kind),
                    is_header: false,
                    label: SharedString::from(&child.label),
                    badge: SharedString::from(c_badge),
                    can_eject: child.can_eject,
                    indent_level: 1,
                    is_selected: c_sel,
                    has_color_tag: ch_color != ColorLabel::None,
                    color_tag_brush: ch_brush.into(),
                });
                sb_targets.push((child.id.clone(), child_target));
            }
        }
    }
    rt.sidebar_targets = sb_targets;
    window.set_fav_min_sidebar_idx(fav_min_idx.max(0));
    window.set_fav_max_sidebar_idx(fav_max_idx.max(0));
    window.set_sidebar_items(ModelRc::new(VecModel::from(sb_entries)));

    // Left & Right headers, interactive breadcrumbs, and tabs
    let root_label = crate::resolve_dynamic_root_label();
    let l_cur_path = rt.current_pane_path(false);
    let r_cur_path = rt.current_pane_path(true);

    let l_crumbs = rt.app_state.left_header.formatted_breadcrumbs.clone();
    let l_sub = rt.app_state.left_header.status_subtitle.clone();
    let (l_hdr_icon, l_segs) = build_pane_header_breadcrumbs(root_label, &l_cur_path);
    window.set_left_header_icon(l_hdr_icon.into());
    window.set_left_breadcrumbs(l_crumbs.into());
    window.set_left_breadcrumb_segments(ModelRc::new(VecModel::from(l_segs)));
    if !window.get_left_path_edit_mode() {
        window.set_left_path_input(l_cur_path.clone().into());
    }
    window.set_left_status_subtitle(l_sub.into());
    let l_mode = rt
        .app_state
        .left_tabs
        .get(rt.app_state.left_active_tab)
        .map(|t| view_mode_to_int(t.view_mode))
        .unwrap_or(0);
    window.set_left_view_mode(l_mode);
    let (l_sort_col, l_sort_asc) = rt
        .app_state
        .left_tabs
        .get(rt.app_state.left_active_tab)
        .map(|t| {
            let asc = t.sort_direction == SortDirection::Ascending;
            (sort_col_to_str(t.sort_column), asc)
        })
        .unwrap_or(("name", true));
    window.set_left_sort_column(l_sort_col.into());
    window.set_left_sort_ascending(l_sort_asc);
    let l_filter = rt
        .app_state
        .left_tabs
        .get(rt.app_state.left_active_tab)
        .map(|t| t.inline_filter.trim().to_string())
        .unwrap_or_default();
    window.set_left_filter_query(l_filter.clone().into());
    window.set_left_search_running(rt.left_search_running && !l_filter.is_empty());

    let l_tabs: Vec<TabEntrySlint> = rt
        .app_state
        .left_tabs
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let q = t.inline_filter.trim();
            let is_search = !q.is_empty();
            let is_remote = split_remote_uri(&t.current_path).is_some();
            let tab_icon = tab_icon_for_path(&t.current_path, is_search, is_remote);
            let tab_title = if is_search {
                format!("Search: \"{q}\"")
            } else {
                rt.format_tab_title(&t.current_path)
            };
            TabEntrySlint {
                title: SharedString::from(tab_title),
                tooltip: SharedString::from(&t.current_path),
                icon_kind: SharedString::from(tab_icon),
                is_active: idx == rt.app_state.left_active_tab,
                is_search,
                is_remote,
            }
        })
        .collect();
    window.set_left_tabs(ModelRc::new(VecModel::from(l_tabs)));

    let r_crumbs = rt.app_state.right_header.formatted_breadcrumbs.clone();
    let r_sub = rt.app_state.right_header.status_subtitle.clone();
    let (r_hdr_icon, r_segs) = build_pane_header_breadcrumbs(root_label, &r_cur_path);
    window.set_right_header_icon(r_hdr_icon.into());
    window.set_right_breadcrumbs(r_crumbs.into());
    window.set_right_breadcrumb_segments(ModelRc::new(VecModel::from(r_segs)));
    if !window.get_right_path_edit_mode() {
        window.set_right_path_input(r_cur_path.clone().into());
    }
    window.set_right_status_subtitle(r_sub.into());
    let r_mode = rt
        .app_state
        .right_tabs
        .get(rt.app_state.right_active_tab)
        .map(|t| view_mode_to_int(t.view_mode))
        .unwrap_or(1);
    window.set_right_view_mode(r_mode);
    let (r_sort_col, r_sort_asc) = rt
        .app_state
        .right_tabs
        .get(rt.app_state.right_active_tab)
        .map(|t| {
            let asc = t.sort_direction == SortDirection::Ascending;
            (sort_col_to_str(t.sort_column), asc)
        })
        .unwrap_or(("name", true));
    window.set_right_sort_column(r_sort_col.into());
    window.set_right_sort_ascending(r_sort_asc);
    let r_filter = rt
        .app_state
        .right_tabs
        .get(rt.app_state.right_active_tab)
        .map(|t| t.inline_filter.trim().to_string())
        .unwrap_or_default();
    window.set_right_filter_query(r_filter.clone().into());
    window.set_right_search_running(rt.right_search_running && !r_filter.is_empty());

    let r_tabs: Vec<TabEntrySlint> = rt
        .app_state
        .right_tabs
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let q = t.inline_filter.trim();
            let is_search = !q.is_empty();
            let is_remote = split_remote_uri(&t.current_path).is_some();
            let tab_icon = tab_icon_for_path(&t.current_path, is_search, is_remote);
            let tab_title = if is_search {
                format!("Search: \"{q}\"")
            } else {
                rt.format_tab_title(&t.current_path)
            };
            TabEntrySlint {
                title: SharedString::from(tab_title),
                tooltip: SharedString::from(&t.current_path),
                icon_kind: SharedString::from(tab_icon),
                is_active: idx == rt.app_state.right_active_tab,
                is_search,
                is_remote,
            }
        })
        .collect();
    window.set_right_tabs(ModelRc::new(VecModel::from(r_tabs)));

    // Left & Right effective FolderViewOptions + active View Options popover state
    let l_vo = rt.effective_view_options_for_dir(&l_cur_path);
    let r_vo = rt.effective_view_options_for_dir(&r_cur_path);
    window.set_left_col_size(l_vo.col_size);
    window.set_left_col_kind(l_vo.col_kind);
    window.set_left_col_modified(l_vo.col_date_modified);
    window.set_left_col_created(l_vo.col_date_created);
    window.set_left_col_added(l_vo.col_date_added);
    window.set_left_col_owner(l_vo.col_owner);
    window.set_left_col_group(l_vo.col_group);
    window.set_left_col_permissions(l_vo.col_permissions);
    window.set_left_col_tags(l_vo.col_tags);
    window.set_left_icon_size(l_vo.icon_size as i32);
    window.set_left_bold_folders(l_vo.bold_folders);
    window.set_left_alt_bg(l_vo.use_alternating_row_bg);

    window.set_right_col_size(r_vo.col_size);
    window.set_right_col_kind(r_vo.col_kind);
    window.set_right_col_modified(r_vo.col_date_modified);
    window.set_right_col_created(r_vo.col_date_created);
    window.set_right_col_added(r_vo.col_date_added);
    window.set_right_col_owner(r_vo.col_owner);
    window.set_right_col_group(r_vo.col_group);
    window.set_right_col_permissions(r_vo.col_permissions);
    window.set_right_col_tags(r_vo.col_tags);
    window.set_right_icon_size(r_vo.icon_size as i32);
    window.set_right_bold_folders(r_vo.bold_folders);
    window.set_right_alt_bg(r_vo.use_alternating_row_bg);

    let active_vo = if is_right_active { &r_vo } else { &l_vo };
    let active_sort_enum = if is_right_active {
        rt.app_state
            .right_tabs
            .get(rt.app_state.right_active_tab)
            .map(|t| t.sort_column)
            .unwrap_or(active_vo.sort_column)
    } else {
        rt.app_state
            .left_tabs
            .get(rt.app_state.left_active_tab)
            .map(|t| t.sort_column)
            .unwrap_or(active_vo.sort_column)
    };
    let scope_str = scope_to_str(active_vo.scope);
    let sort_str = sort_col_to_str(active_sort_enum);
    window.set_vo_scope(scope_str.into());
    window.set_vo_sort_by(sort_str.into());
    window.set_vo_icon_size(active_vo.icon_size as i32);
    window.set_vo_col_size(active_vo.col_size);
    window.set_vo_col_kind(active_vo.col_kind);
    window.set_vo_col_modified(active_vo.col_date_modified);
    window.set_vo_col_added(active_vo.col_date_added);
    window.set_vo_col_created(active_vo.col_date_created);
    window.set_vo_col_owner(active_vo.col_owner);
    window.set_vo_col_group(active_vo.col_group);
    window.set_vo_col_permissions(active_vo.col_permissions);
    window.set_vo_col_tags(active_vo.col_tags);
    window.set_vo_bold_folders(active_vo.bold_folders);
    window.set_vo_show_hidden(show_hidden || active_vo.show_hidden_files);
    window.set_vo_show_icon_preview(active_vo.show_icon_preview);
    window.set_vo_treat_packages_as_folders(active_vo.treat_packages_as_folders);
    window.set_vo_show_folders_on_top(active_vo.show_folders_on_top);
    window.set_vo_folders_always_alpha(active_vo.folders_always_alpha);
    window.set_vo_use_relative_dates(active_vo.use_relative_dates);
    window.set_vo_calculate_all_sizes(active_vo.calculate_all_sizes);
    window.set_vo_use_alternating_bg(active_vo.use_alternating_row_bg);
    window.set_show_hidden_files(show_hidden || active_vo.show_hidden_files);

    // Rows, 3-Column Polaroid Icon Triples, and Miller Parent Columns
    let mut missing_thumbs = Vec::new();
    let l_total = rt.left_entries.len();
    let l_max_top = l_total.saturating_sub(VIRTUAL_WINDOW_CAP);
    rt.left_virtual_top_row = rt.left_virtual_top_row.min(l_max_top);
    window.set_left_total_rows(l_total as i32);
    window.set_left_virtual_top_row(rt.left_virtual_top_row as i32);

    let l_base_opt = (!l_filter.is_empty()).then_some(l_cur_path.as_str());
    let l_rows = build_slint_rows_from_flattened(
        &rt.left_entries,
        &rt.left_selected_indices,
        &rt.thumbnail_cache,
        &mut missing_thumbs,
        &l_vo,
        (rt.left_virtual_top_row, l_base_opt),
    );
    let l_triples = if l_mode == 1 {
        build_icon_triples(&l_rows, rt.left_virtual_top_row)
    } else {
        Vec::new()
    };
    let l_parent_rows = if l_mode == 2 {
        let l_parent_sel = rt
            .left_parent_entries
            .iter()
            .position(|m| m.path == l_cur_path);
        build_slint_rows_from_meta(
            &rt.left_parent_entries,
            l_parent_sel,
            &rt.thumbnail_cache,
            &mut missing_thumbs,
            &l_vo,
        )
    } else {
        Vec::new()
    };
    window.set_left_rows(ModelRc::new(VecModel::from(l_rows)));
    window.set_left_icon_triples(ModelRc::new(VecModel::from(l_triples)));
    window.set_left_parent_rows(ModelRc::new(VecModel::from(l_parent_rows)));

    if dual_mode {
        let r_total = rt.right_entries.len();
        let r_max_top = r_total.saturating_sub(VIRTUAL_WINDOW_CAP);
        rt.right_virtual_top_row = rt.right_virtual_top_row.min(r_max_top);
        window.set_right_total_rows(r_total as i32);
        window.set_right_virtual_top_row(rt.right_virtual_top_row as i32);

        let r_base_opt = (!r_filter.is_empty()).then_some(r_cur_path.as_str());
        let r_rows = build_slint_rows_from_flattened(
            &rt.right_entries,
            &rt.right_selected_indices,
            &rt.thumbnail_cache,
            &mut missing_thumbs,
            &r_vo,
            (rt.right_virtual_top_row, r_base_opt),
        );
        let r_triples = if r_mode == 1 {
            build_icon_triples(&r_rows, rt.right_virtual_top_row)
        } else {
            Vec::new()
        };
        let r_parent_rows = if r_mode == 2 {
            let r_parent_sel = rt
                .right_parent_entries
                .iter()
                .position(|m| m.path == r_cur_path);
            build_slint_rows_from_meta(
                &rt.right_parent_entries,
                r_parent_sel,
                &rt.thumbnail_cache,
                &mut missing_thumbs,
                &r_vo,
            )
        } else {
            Vec::new()
        };
        window.set_right_rows(ModelRc::new(VecModel::from(r_rows)));
        window.set_right_icon_triples(ModelRc::new(VecModel::from(r_triples)));
        window.set_right_parent_rows(ModelRc::new(VecModel::from(r_parent_rows)));
    }
    for p in missing_thumbs {
        rt.request_bg_thumbnail(&p);
    }

    // Active File Conflict Resolution Modal state sync
    if let Some((src_meta, ex_meta, is_move)) = rt.active_conflict() {
        let src_dt = src_meta.formatted_conflict_datetime();
        let ex_dt = ex_meta.formatted_conflict_datetime();
        let modal = ConflictModalDescriptor::from_collision(&src_meta, &ex_meta, &src_dt, &ex_dt);
        let sub = if is_move {
            modal.subheadline
        } else {
            "Do you want to replace it with the one you\u{2019}re copying?".to_string()
        };
        let new_sz = if is_browsable_folder(&src_meta) {
            "Folder".to_string()
        } else {
            src_meta.formatted_size()
        };
        let ex_sz = if is_browsable_folder(&ex_meta) {
            "Folder".to_string()
        } else {
            ex_meta.formatted_size()
        };
        let new_dt = modal.new_item.formatted_modified;
        let old_dt = modal.existing_item.formatted_modified;
        window.set_conflict_headline(modal.headline.into());
        window.set_conflict_subheadline(sub.into());
        window.set_conflict_new_date(new_dt.into());
        window.set_conflict_new_size(new_sz.into());
        window.set_conflict_existing_date(old_dt.into());
        window.set_conflict_existing_size(ex_sz.into());
        window.set_show_conflict_modal(true);
    } else {
        window.set_show_conflict_modal(false);
    }

    // Inspector & Preview Minion
    sync_inspector_to_window(window, rt);

    // Transfers & Logs
    let tr_rows: Vec<TransferRowSlint> = rt
        .transfers
        .iter()
        .map(|t| {
            let can_control = rt.transfer_controls.contains_key(&t.id)
                && matches!(
                    t.status,
                    TransferStatus::InProgress | TransferStatus::Paused | TransferStatus::Queued
                );
            TransferRowSlint {
                id: SharedString::from(&t.id),
                priority_label: match t.priority {
                    TransferPriority::Foreground => SharedString::from("Foreground"),
                    TransferPriority::Background => SharedString::from("Background"),
                },
                source_name: SharedString::from(&t.source_path),
                dest_path: SharedString::from(&t.destination_path),
                status_text: SharedString::from(format!("{:?}", t.status)),
                progress_pct: (t.progress_fraction() * 100.0) as i32,
                is_paused: t.status == TransferStatus::Paused,
                can_control,
            }
        })
        .collect();
    window.set_transfer_rows(ModelRc::new(VecModel::from(tr_rows)));

    if window.get_show_sync_modal() {
        sync_sync_modal_to_window(window, rt);
    }
    rt.session_dirty = true;
}

fn update_rows_selection_in_place(
    rows: &ModelRc<FileRowSlint>,
    selected_indices: &HashSet<usize>,
    virtual_top: usize,
) {
    let count = rows.row_count();
    for idx in 0..count {
        if let Some(mut row) = rows.row_data(idx) {
            let should_sel = selected_indices.contains(&(virtual_top + idx));
            if row.is_selected != should_sel {
                row.is_selected = should_sel;
                rows.set_row_data(idx, row);
            }
        }
    }
}

fn update_triples_selection_in_place(
    triples: &ModelRc<IconGridTripleSlint>,
    selected_indices: &HashSet<usize>,
) {
    let count = triples.row_count();
    for idx in 0..count {
        if let Some(mut triple) = triples.row_data(idx) {
            let sel0 = triple.has_0 && selected_indices.contains(&(triple.idx_0 as usize));
            let sel1 = triple.has_1 && selected_indices.contains(&(triple.idx_1 as usize));
            let sel2 = triple.has_2 && selected_indices.contains(&(triple.idx_2 as usize));
            if triple.item_0.is_selected != sel0
                || triple.item_1.is_selected != sel1
                || triple.item_2.is_selected != sel2
            {
                triple.item_0.is_selected = sel0;
                triple.item_1.is_selected = sel1;
                triple.item_2.is_selected = sel2;
                triples.set_row_data(idx, triple);
            }
        }
    }
}

fn sync_selection_and_inspector(window: &ManifoldWindow, rt: &mut GuiRuntime) {
    let folder_title = rt.app_state.toolbar.active_folder_title.clone();
    let is_right_active = rt.is_right_effective();
    let l_sub = rt.app_state.left_header.status_subtitle.clone();
    let r_sub = rt.app_state.right_header.status_subtitle.clone();
    window.set_active_folder_title(folder_title.into());
    window.set_right_pane_active(is_right_active);
    window.set_left_status_subtitle(l_sub.into());
    window.set_right_status_subtitle(r_sub.into());

    let l_rows = window.get_left_rows();
    update_rows_selection_in_place(&l_rows, &rt.left_selected_indices, rt.left_virtual_top_row);
    let l_triples = window.get_left_icon_triples();
    update_triples_selection_in_place(&l_triples, &rt.left_selected_indices);
    let r_rows = window.get_right_rows();
    update_rows_selection_in_place(
        &r_rows,
        &rt.right_selected_indices,
        rt.right_virtual_top_row,
    );
    let r_triples = window.get_right_icon_triples();
    update_triples_selection_in_place(&r_triples, &rt.right_selected_indices);

    sync_inspector_to_window(window, rt);
}

fn sync_inspector_to_window(window: &ManifoldWindow, rt: &mut GuiRuntime) {
    window.set_show_inspector(rt.app_state.inspector_open);
    if let Some(vm) = &rt.app_state.inspector_vm {
        let title_s = vm.title.clone();
        let sub_s = vm.subtitle.clone();
        let path_s = vm.general.path.clone();
        let size_s = vm.general.size.clone();
        let mod_s = vm.general.modified.clone();
        let cre_s = vm.general.created.clone();
        let add_s = vm.general.added.clone();
        let raw_ext = vm.general.extension.trim().trim_start_matches('.');
        let ext_s = if raw_ext.is_empty() {
            "--".to_string()
        } else {
            format!(".{raw_ext}")
        };
        let hide_ext = vm.general.hide_extension;
        window.set_inspector_title(title_s.into());
        window.set_inspector_subtitle(sub_s.into());
        window.set_inspector_path(path_s.into());
        window.set_inspector_size(size_s.into());
        window.set_inspector_modified(mod_s.into());
        window.set_inspector_created(cre_s.into());
        window.set_inspector_added(add_s.into());
        window.set_inspector_extension(ext_s.into());
        window.set_inspector_hide_extension(hide_ext);

        let m = vm.permissions.matrix;
        window.set_perm_ur(m.owner.read);
        window.set_perm_uw(m.owner.write);
        window.set_perm_ux(m.owner.execute);
        window.set_perm_gr(m.group.read);
        window.set_perm_gw(m.group.write);
        window.set_perm_gx(m.group.execute);
        window.set_perm_or(m.others.read);
        window.set_perm_ow(m.others.write);
        window.set_perm_ox(m.others.execute);
        let sym = vm.permissions.octal_symbolic.clone();
        let oct = vm.permissions.octal_code.clone();
        let own_s = vm.permissions.owner.clone();
        let grp_s = vm.permissions.group.clone();
        window.set_inspector_perm_symbolic(sym.into());
        window.set_inspector_perm_octal(oct.into());
        window.set_inspector_owner(own_s.into());
        window.set_inspector_group(grp_s.into());
        window.set_inspector_locked(vm.permissions.locked);
    } else {
        window.set_inspector_title("No Selection".into());
        window.set_inspector_subtitle("--".into());
        window.set_inspector_path("--".into());
        window.set_inspector_size("--".into());
        window.set_inspector_modified("--".into());
        window.set_inspector_created("--".into());
        window.set_inspector_added("--".into());
        window.set_inspector_extension("--".into());
        window.set_inspector_is_dir(true);
        window.set_inspector_is_image(false);
        window.set_inspector_has_image(false);
        window.set_inspector_has_text_preview(false);
        window.set_inspector_image_dimensions(SharedString::default());
        window.set_inspector_media_category(SharedString::default());
        window.set_inspector_media_line1(SharedString::default());
        window.set_inspector_media_line2(SharedString::default());
        window.set_inspector_media_line3(SharedString::default());
    }

    if let Some(sel) = rt.selected_metadata().cloned() {
        let color_idx = color_label_to_index(sel.color_label);
        let is_img = is_image_extension(sel.extension.as_deref());
        let is_app = is_macos_app_bundle(&sel);
        let is_browsable = is_browsable_folder(&sel);
        window.set_inspector_color_index(color_idx);
        window.set_inspector_is_dir(is_browsable);
        window.set_inspector_is_image(is_img);

        let l_cols = rt
            .app_state
            .left_tabs
            .get(rt.app_state.left_active_tab)
            .is_some_and(|t| t.view_mode == PaneViewMode::Columns);
        let r_cols = rt.app_state.dual_pane_enabled
            && rt
                .app_state
                .right_tabs
                .get(rt.app_state.right_active_tab)
                .is_some_and(|t| t.view_mode == PaneViewMode::Columns);
        let need_rich_preview =
            rt.app_state.inspector_open || window.get_show_preview_minion() || l_cols || r_cols;
        if !need_rich_preview {
            return;
        }

        let loaded_img = if is_app && split_remote_uri(&sel.path).is_none() {
            rt.thumbnail_cache
                .entry(sel.path.clone())
                .or_insert_with(|| load_macos_app_bundle_icon(&sel.path))
                .clone()
        } else if is_img && !sel.is_dir && split_remote_uri(&sel.path).is_none() {
            rt.thumbnail_cache
                .entry(sel.path.clone())
                .or_insert_with(|| Image::load_from_path(Path::new(&sel.path)).ok())
                .clone()
        } else {
            None
        };
        let img_dims = if let Some(img) = loaded_img {
            let sz = img.size();
            let dims = format!("{}×{} px", sz.width, sz.height);
            window.set_inspector_preview_image(img);
            window.set_inspector_has_image(true);
            window.set_inspector_image_dimensions(dims.into());
            Some((sz.width, sz.height))
        } else {
            window.set_inspector_has_image(false);
            window.set_inspector_image_dimensions(SharedString::default());
            None
        };

        let (m_cat, m_l1, m_l2, m_l3) = extract_media_metadata(rt, &sel, img_dims);
        window.set_inspector_media_category(m_cat.into());
        window.set_inspector_media_line1(m_l1.into());
        window.set_inspector_media_line2(m_l2.into());
        window.set_inspector_media_line3(m_l3.into());

        let (preview_text, has_text_prev) = build_inspector_preview(rt, &sel);
        window.set_inspector_preview_text(preview_text.into());
        window.set_inspector_has_text_preview(has_text_prev);
    }
}

fn format_duration_secs(secs: u64) -> String {
    let hrs = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hrs > 0 {
        format!("{hrs:02}:{mins:02}:{rem:02}")
    } else {
        format!("{mins:02}:{rem:02}")
    }
}

fn extract_media_metadata(
    rt: &GuiRuntime,
    meta: &FileMetadata,
    img_dims: Option<(u32, u32)>,
) -> (String, String, String, String) {
    if meta.is_dir {
        return (String::new(), String::new(), String::new(), String::new());
    }
    let ext_low = meta.extension.as_deref().unwrap_or("").to_ascii_lowercase();
    let ext_up = ext_low.to_ascii_uppercase();
    let sz_str = meta.formatted_size();

    if is_image_extension(Some(&ext_low)) {
        let (w, h) = img_dims.unwrap_or((0, 0));
        let (line1, line2) = if w > 0 && h > 0 {
            let mp = (f64::from(w) * f64::from(h)) / 1_000_000.0;
            (
                format!("Resolution: {w} × {h} px"),
                format!("Format: {ext_up} • {mp:.2} MP"),
            )
        } else {
            (
                format!("Format: {ext_up} Image"),
                format!("File Size: {sz_str}"),
            )
        };
        let color_info = match ext_low.as_str() {
            "png" | "webp" | "ico" => "Color: RGBA (8-bit, Alpha)",
            "svg" => "Color: Vector SVG / XML",
            "gif" => "Color: Indexed 256-Color",
            _ => "Color: sRGB (24-bit TrueColor)",
        };
        return (
            "Photo / Image".to_string(),
            line1,
            line2,
            color_info.to_string(),
        );
    }

    if matches!(
        ext_low.as_str(),
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "opus" | "wma" | "aiff"
    ) {
        let vfs = rt.backend_for_path(&meta.path);
        let head = vfs.read_range(&meta.path, 0, 2048).unwrap_or_default();
        let bytes_len = meta.size.unwrap_or(0);
        if ext_low == "wav" && head.len() >= 36 && &head[0..4] == b"RIFF" {
            let ch = u16::from_le_bytes([head[22], head[23]]);
            let sr = u32::from_le_bytes([head[24], head[25], head[26], head[27]]);
            let br = u32::from_le_bytes([head[28], head[29], head[30], head[31]]);
            let bps = u16::from_le_bytes([head[34], head[35]]);
            let dur_s = if br > 0 { bytes_len / u64::from(br) } else { 0 };
            let dur_str = format_duration_secs(dur_s);
            let ch_lbl = if ch == 1 { "Mono" } else { "Stereo" };
            let khz = f64::from(sr) / 1000.0;
            let kbps = (u64::from(br) * 8) / 1000;
            return (
                "Audio / Music".to_string(),
                format!("Format: PCM WAV ({bps}-bit)"),
                format!("Audio: {khz:.1} kHz • {ch_lbl} ({ch} ch)"),
                format!("Duration: {dur_str} • {kbps} kbps"),
            );
        }
        if ext_low == "flac" && head.len() >= 26 && &head[0..4] == b"fLaC" {
            let b18 = u32::from(head[18]);
            let b19 = u32::from(head[19]);
            let b20 = u32::from(head[20]);
            let sr = (b18 << 12) | (b19 << 4) | (b20 >> 4);
            let ch = ((head[20] >> 1) & 0x07) + 1;
            let bps = (((head[20] & 0x01) << 4) | (head[21] >> 4)) + 1;
            let khz = if sr > 0 { f64::from(sr) / 1000.0 } else { 44.1 };
            let est_dur = bytes_len / 110_000;
            let dur_str = format_duration_secs(est_dur.max(1));
            return (
                "Audio / Music".to_string(),
                format!("Format: FLAC Lossless ({bps}-bit)"),
                format!("Audio: {khz:.1} kHz • {ch} ch"),
                format!("Est. Duration: {dur_str}"),
            );
        }
        let est_kbps: u64 = match ext_low.as_str() {
            "m4a" | "aac" | "ogg" | "opus" => 192,
            _ => 256,
        };
        let dur_s = (bytes_len * 8) / (est_kbps * 1000).max(1);
        let dur_str = format_duration_secs(dur_s.max(1));
        let has_id3 = head.starts_with(b"ID3");
        let tag_lbl = if has_id3 {
            "ID3v2 Tags"
        } else {
            "Audio Stream"
        };
        return (
            "Audio / Music".to_string(),
            format!("Format: {ext_up} • {tag_lbl}"),
            "Audio: 44.1 kHz • Stereo (2 ch)".to_string(),
            format!("Duration: ~{dur_str} • ~{est_kbps} kbps"),
        );
    }

    if matches!(
        ext_low.as_str(),
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "mpeg" | "mpg" | "ts" | "flv"
    ) {
        let bytes_len = meta.size.unwrap_or(0);
        let est_mbps = 4u64;
        let dur_s = (bytes_len * 8) / (est_mbps * 1_000_000).max(1);
        let dur_str = format_duration_secs(dur_s.max(1));
        let codec_lbl = match ext_low.as_str() {
            "mp4" | "m4v" | "mov" => "H.264 / HEVC + AAC",
            "webm" => "VP9 / AV1 + Opus",
            "mkv" => "Matroska + Multi-Audio",
            _ => "Digital Video Stream",
        };
        return (
            "Video / Media".to_string(),
            format!("Container: {ext_up} Video"),
            format!("Stream: {codec_lbl}"),
            format!("Est. Duration: {dur_str} (~{est_mbps}.0 Mbps)"),
        );
    }

    (String::new(), String::new(), String::new(), String::new())
}

fn is_text_or_structured_extension(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "json"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "html"
            | "htm"
            | "css"
            | "scss"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "rs"
            | "py"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "java"
            | "kt"
            | "swift"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "sql"
            | "csv"
            | "tsv"
            | "ini"
            | "conf"
            | "cfg"
            | "env"
            | "log"
            | "slint"
            | "dockerfile"
            | "makefile"
            | "gitignore"
            | "plist"
            | "svg"
    )
}

fn tab_icon_for_path(path: &str, is_search: bool, is_remote: bool) -> &'static str {
    if is_search {
        return "search-circle";
    }
    if split_archive_uri(path).is_some() {
        return "archive";
    }
    if is_remote {
        return "globe";
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let trimmed = path.trim_end_matches('/');
    if !home.is_empty() && trimmed == home.trim_end_matches('/') {
        return "home";
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match name {
        "Movies" | "Videos" => "movies",
        "Music" | "Audio" => "music",
        _ => "folder",
    }
}

fn build_pane_header_breadcrumbs(
    root_label: &str,
    path: &str,
) -> (&'static str, Vec<BreadcrumbSegmentSlint>) {
    if let Some((arch_path, inner)) = split_archive_uri(path) {
        let mut raw: Vec<(String, String)> = Vec::new();
        if let Some(parent_dir) = Path::new(&arch_path).parent().and_then(|p| p.to_str()) {
            let parent_lbl = Path::new(parent_dir)
                .file_name()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(root_label);
            raw.push((parent_lbl.to_string(), parent_dir.to_string()));
        }
        let arch_name = Path::new(&arch_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Archive");
        raw.push((arch_name.to_string(), format!("archive://{arch_path}#/")));
        let mut accum = String::new();
        for part in inner.split('/').filter(|s| !s.is_empty()) {
            accum = format!("{accum}/{part}");
            let target = format!("archive://{arch_path}#{accum}");
            raw.push((part.to_string(), target));
        }
        let len = raw.len();
        let segs = raw
            .into_iter()
            .enumerate()
            .map(|(idx, (lbl, target))| BreadcrumbSegmentSlint {
                label: SharedString::from(lbl),
                target_path: SharedString::from(target),
                is_last: idx + 1 == len,
            })
            .collect();
        return ("archive", segs);
    }
    if split_remote_uri(path).is_some() {
        let raw = crate::PathAndStatusHeader::clickable_segments(root_label, path);
        let len = raw.len();
        let segs = raw
            .into_iter()
            .enumerate()
            .map(|(idx, (lbl, target))| BreadcrumbSegmentSlint {
                label: SharedString::from(lbl),
                target_path: SharedString::from(target),
                is_last: idx + 1 == len,
            })
            .collect();
        return ("globe", segs);
    }

    let home = std::env::var("HOME")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    let clean_path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    let in_home = !home.is_empty()
        && home != "/"
        && (clean_path == home || clean_path.starts_with(&format!("{home}/")));

    if in_home {
        let home_name = Path::new(&home)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("Home");
        let mut raw: Vec<(String, String)> = vec![(home_name.to_string(), home.clone())];
        if let Some(rel) = clean_path.strip_prefix(&home) {
            let mut accum = home;
            for part in rel.split('/').filter(|s| !s.is_empty()) {
                accum = format!("{accum}/{part}");
                raw.push((part.to_string(), accum.clone()));
            }
        }
        let len = raw.len();
        let segs = raw
            .into_iter()
            .enumerate()
            .map(|(idx, (lbl, target))| BreadcrumbSegmentSlint {
                label: SharedString::from(lbl),
                target_path: SharedString::from(target),
                is_last: idx + 1 == len,
            })
            .collect();
        return ("home", segs);
    }

    let raw = crate::PathAndStatusHeader::clickable_segments(root_label, path);
    let len = raw.len();
    let segs = raw
        .into_iter()
        .enumerate()
        .map(|(idx, (lbl, target))| BreadcrumbSegmentSlint {
            label: SharedString::from(lbl),
            target_path: SharedString::from(target),
            is_last: idx + 1 == len,
        })
        .collect();
    ("drive", segs)
}

fn is_likely_utf8_text(bytes: &[u8], ext_low: &str) -> bool {
    if bytes.is_empty() {
        return is_text_or_structured_extension(ext_low);
    }
    if bytes.contains(&0) {
        return false;
    }
    std::str::from_utf8(bytes).is_ok()
}

fn build_inspector_preview(rt: &GuiRuntime, sel: &FileMetadata) -> (String, bool) {
    if is_macos_app_bundle(sel) {
        let summary = format!(
            "macOS Application Bundle\nSize: {}\nPath: {}",
            sel.formatted_size(),
            sel.path
        );
        return (summary, false);
    }
    if sel.is_dir {
        return (format!("Folder\n{}", sel.path), false);
    }
    let ext_low = sel.extension.as_deref().unwrap_or("").to_ascii_lowercase();
    let is_local = split_remote_uri(&sel.path).is_none() && Path::new(&sel.path).is_file();

    if is_local && ext_low == "docx" {
        let out_opt = Command::new("unzip")
            .args(["-p", &sel.path, "word/document.xml"])
            .output()
            .ok()
            .filter(|o| o.status.success() && !o.stdout.is_empty());
        if let Some(out) = out_opt {
            let xml = String::from_utf8_lossy(&out.stdout);
            let plain = strip_xml_tags(&xml);
            if !plain.trim().is_empty() {
                let sz = sel.formatted_size();
                let dt = sel.formatted_iso_datetime();
                let sep = "────────────────────────────────────────";
                let doc = format!(
                    "DOCX Document Preview — {}\nSize: {sz}  |  Modified: {dt}\n{sep}\n{plain}",
                    sel.name
                );
                return (doc, true);
            }
        }
    }

    let vfs = rt.backend_for_path(&sel.path);
    if let Ok(bytes) = vfs.read_range(&sel.path, 0, 4096) {
        let snippet_opt = is_likely_utf8_text(&bytes, &ext_low)
            .then(|| std::str::from_utf8(&bytes).ok())
            .flatten()
            .map(|t| t.lines().take(36).collect::<Vec<_>>().join("\n"))
            .filter(|s| !s.trim().is_empty());
        if let Some(snippet) = snippet_opt {
            return (snippet, true);
        }
    }
    let summary = format!(
        "{}\nSize: {}\nPermissions: {} ({})",
        sel.kind,
        sel.formatted_size(),
        sel.permissions_matrix().to_symbolic_string(),
        sel.permissions_matrix().to_octal_string()
    );
    (summary, false)
}

fn strip_xml_tags(xml: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for ch in xml.chars() {
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
        } else if ch == '>' {
            in_tag = false;
            if tag_buf == "/w:p" || tag_buf == "w:br" || tag_buf == "w:cr" {
                out.push('\n');
            } else if tag_buf == "w:tab" {
                out.push('\t');
            }
        } else if in_tag {
            if tag_buf.len() < 16 {
                tag_buf.push(ch);
            }
        } else {
            out.push(ch);
        }
        if out.len() >= 1800 {
            break;
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(28)
        .collect::<Vec<_>>()
        .join("\n")
}

fn vfs_url_to_target(vfs_url: &str) -> String {
    if vfs_url == "connect://dialog" {
        return vfs_url.to_string();
    }
    if let Some(stripped) = vfs_url.strip_prefix("file://") {
        if stripped.is_empty() {
            "/".to_string()
        } else {
            stripped.to_string()
        }
    } else {
        vfs_url.to_string()
    }
}

fn relative_parent_subpath(item_path: &str, base_dir: &str) -> Option<String> {
    let base_clean = if base_dir == "/" {
        "/".to_string()
    } else {
        format!("{}/", base_dir.trim_end_matches('/'))
    };
    let rel = item_path.strip_prefix(&base_clean)?;
    let (parent_sub, _) = rel.rsplit_once('/')?;
    let trimmed = parent_sub.trim_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Formats a [`SystemTime`](std::time::SystemTime) for List/Tree View columns, optionally using
/// `"Today, HH:mm"` and `"Yesterday, HH:mm"` when `use_relative` is enabled.
pub fn format_datetime_for_view(ts: Option<std::time::SystemTime>, use_relative: bool) -> String {
    let Some(t) = ts else {
        return "--".to_string();
    };
    let (y, m, d, hh, mm) = manifold_vfs::system_time_to_civil(t);
    if use_relative {
        let (ny, nm, nd, _, _) = manifold_vfs::current_local_civil_datetime();
        if (y, m, d) == (ny, nm, nd) {
            return format!("Today, {hh:02}:{mm:02}");
        }
        let item_days = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|dur| (dur.as_secs() as i64).div_euclid(86_400))
            .unwrap_or(-1);
        let now_days = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|dur| (dur.as_secs() as i64).div_euclid(86_400))
            .unwrap_or(-99);
        if item_days + 1 == now_days {
            return format!("Yesterday, {hh:02}:{mm:02}");
        }
    }
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

fn build_slint_rows_from_flattened(
    entries: &[FlattenedRowEntry],
    selected_indices: &HashSet<usize>,
    thumb_cache: &HashMap<String, Option<Image>>,
    missing_thumbs: &mut Vec<String>,
    vo: &FolderViewOptions,
    window_ctx: (usize, Option<&str>),
) -> Vec<FileRowSlint> {
    let (virtual_top, search_base_dir) = window_ctx;
    entries
        .iter()
        .enumerate()
        .skip(virtual_top)
        .take(VIRTUAL_WINDOW_CAP)
        .map(|(idx, entry)| {
            let state = (
                entry.depth,
                entry.is_expanded,
                selected_indices.contains(&idx),
            );
            let mut row = meta_to_slint_row(&entry.meta, state, thumb_cache, missing_thumbs, vo);
            if let Some(base) = search_base_dir {
                if let Some(sub) = relative_parent_subpath(&entry.meta.path, base) {
                    let disp = format!("{}  —  {sub}", entry.meta.display_name());
                    row.name = SharedString::from(disp);
                }
            }
            row
        })
        .collect()
}

fn build_slint_rows_from_meta(
    entries: &[FileMetadata],
    selected_idx: Option<usize>,
    thumb_cache: &HashMap<String, Option<Image>>,
    missing_thumbs: &mut Vec<String>,
    vo: &FolderViewOptions,
) -> Vec<FileRowSlint> {
    entries
        .iter()
        .enumerate()
        .take(VIRTUAL_WINDOW_CAP)
        .map(|(idx, meta)| {
            let is_sel = selected_idx == Some(idx);
            meta_to_slint_row(meta, (0, false, is_sel), thumb_cache, missing_thumbs, vo)
        })
        .collect()
}

fn extract_png_from_icns(icns_bytes: &[u8]) -> Option<&[u8]> {
    if icns_bytes.len() < 16 || &icns_bytes[0..4] != b"icns" {
        return None;
    }
    let mut pos = 8usize;
    let mut best_png: Option<&[u8]> = None;
    while pos + 8 <= icns_bytes.len() {
        let chunk_len = u32::from_be_bytes([
            icns_bytes[pos + 4],
            icns_bytes[pos + 5],
            icns_bytes[pos + 6],
            icns_bytes[pos + 7],
        ]) as usize;
        if chunk_len < 8 || pos + chunk_len > icns_bytes.len() {
            break;
        }
        let payload = &icns_bytes[pos + 8..pos + chunk_len];
        if payload.starts_with(b"\x89PNG\r\n\x1a\n") {
            best_png = Some(payload);
            if payload.len() >= 1024 {
                return best_png;
            }
        }
        pos += chunk_len;
    }
    best_png
}

fn load_macos_app_bundle_icon(app_path: &str) -> Option<Image> {
    let res_dir = Path::new(app_path).join("Contents").join("Resources");
    let rd = std::fs::read_dir(&res_dir).ok()?;
    let mut icns_candidates = Vec::new();
    for entry in rd.flatten().take(32) {
        let p = entry.path();
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext == "png" {
            if let Ok(img) = Image::load_from_path(&p) {
                return Some(img);
            }
        } else if ext == "icns" {
            icns_candidates.push(p);
        }
    }
    icns_candidates.sort_by_key(|p| {
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        !n.eq_ignore_ascii_case("AppIcon.icns")
    });
    for icns_path in icns_candidates {
        let Ok(bytes) = std::fs::read(&icns_path) else {
            continue;
        };
        let Some(png_bytes) = extract_png_from_icns(&bytes) else {
            continue;
        };
        let Ok(cache_dir) = manifold_vfs::secure_runtime_subdir("app-icons") else {
            continue;
        };
        let safe_name: String = app_path
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let png_path = cache_dir.join(format!("{safe_name}.png"));
        if std::fs::write(&png_path, png_bytes).is_err() {
            continue;
        }
        if let Ok(img) = Image::load_from_path(&png_path) {
            return Some(img);
        }
    }
    None
}

fn meta_to_slint_row(
    meta: &FileMetadata,
    row_state: (i32, bool, bool),
    thumb_cache: &HashMap<String, Option<Image>>,
    missing_thumbs: &mut Vec<String>,
    vo: &FolderViewOptions,
) -> FileRowSlint {
    let (depth, is_expanded, is_selected) = row_state;
    let load_thumbs = vo.show_icon_preview;
    let hex = meta.color_label.hex_color().unwrap_or("");
    let tag_name = color_label_to_name(meta.color_label);
    let brush = color_label_to_slint_color(meta.color_label);
    let tint = color_label_to_tint_color(meta.color_label);
    let ext_ref = meta.extension.as_deref();
    let is_app = is_macos_app_bundle(meta);
    let is_browsable = is_browsable_folder_with_opts(meta, vo.treat_packages_as_folders);
    let is_img = is_image_extension(ext_ref);
    let is_exec = is_executable_file(meta);
    let is_local =
        split_remote_uri(&meta.path).is_none() && split_archive_uri(&meta.path).is_none();
    let can_thumb = load_thumbs
        && is_img
        && !meta.is_dir
        && is_local
        && meta.size.unwrap_or(0) <= 16 * 1024 * 1024;
    let thumb_opt = if (load_thumbs && is_app && is_local) || can_thumb {
        match thumb_cache.get(&meta.path) {
            Some(cached) => cached.clone(),
            None => {
                if missing_thumbs.len() < 96 {
                    missing_thumbs.push(meta.path.clone());
                }
                None
            }
        }
    } else {
        None
    };
    let has_thumbnail = thumb_opt.is_some();
    let thumbnail = thumb_opt.unwrap_or_default();
    let badge_str = if is_app {
        "APP".to_string()
    } else {
        format_ext_badge(ext_ref)
    };
    let kind_str = if is_app { "Application" } else { &meta.kind };
    let rel_dates = vo.use_relative_dates;
    let modified_str = format_datetime_for_view(meta.modified, rel_dates);
    let created_str = format_datetime_for_view(meta.created.or(meta.modified), rel_dates);
    let added_str =
        format_datetime_for_view(meta.added.or(meta.created).or(meta.modified), rel_dates);
    let owner_str = meta
        .owner
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("--");
    let group_str = meta
        .group
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("--");
    let p_mat = meta.permissions_matrix();
    let perms_str = format!(
        "{} ({})",
        p_mat.to_symbolic_string(),
        p_mat.to_octal_string()
    );
    FileRowSlint {
        name: SharedString::from(meta.display_name()),
        path: SharedString::from(&meta.path),
        ext_badge: SharedString::from(badge_str),
        size_text: SharedString::from(meta.formatted_size()),
        modified_text: SharedString::from(modified_str),
        created_text: SharedString::from(created_str),
        added_text: SharedString::from(added_str),
        owner_text: SharedString::from(owner_str),
        group_text: SharedString::from(group_str),
        permissions_text: SharedString::from(perms_str),
        kind_text: SharedString::from(kind_str),
        color_tag_hex: SharedString::from(hex),
        color_tag_name: SharedString::from(tag_name),
        color_tag_brush: brush.into(),
        color_tag_tint: tint.into(),
        has_color_tag: meta.color_label != ColorLabel::None,
        is_dir: is_browsable,
        is_image: is_img,
        is_executable: is_exec,
        is_expanded,
        is_selected,
        has_thumbnail,
        thumbnail,
        depth,
    }
}

fn empty_slint_row() -> FileRowSlint {
    FileRowSlint {
        name: SharedString::default(),
        path: SharedString::default(),
        ext_badge: SharedString::default(),
        size_text: SharedString::default(),
        modified_text: SharedString::default(),
        created_text: SharedString::default(),
        added_text: SharedString::default(),
        owner_text: SharedString::default(),
        group_text: SharedString::default(),
        permissions_text: SharedString::default(),
        kind_text: SharedString::default(),
        color_tag_hex: SharedString::default(),
        color_tag_name: SharedString::default(),
        color_tag_brush: Color::from_argb_u8(0, 0, 0, 0).into(),
        color_tag_tint: Color::from_argb_u8(0, 0, 0, 0).into(),
        has_color_tag: false,
        is_dir: false,
        is_image: false,
        is_executable: false,
        is_expanded: false,
        is_selected: false,
        has_thumbnail: false,
        thumbnail: Image::default(),
        depth: 0,
    }
}

fn build_icon_triples(rows: &[FileRowSlint], virtual_top: usize) -> Vec<IconGridTripleSlint> {
    let mut triples = Vec::new();
    let limit = rows.len().min(VIRTUAL_WINDOW_CAP);
    let mut idx = 0usize;
    while idx < limit {
        let item_0 = rows[idx].clone();
        let has_1 = idx + 1 < limit;
        let item_1 = if has_1 {
            rows[idx + 1].clone()
        } else {
            empty_slint_row()
        };
        let has_2 = idx + 2 < limit;
        let item_2 = if has_2 {
            rows[idx + 2].clone()
        } else {
            empty_slint_row()
        };
        let g0 = virtual_top + idx;
        triples.push(IconGridTripleSlint {
            has_0: true,
            idx_0: g0 as i32,
            item_0,
            has_1,
            idx_1: (g0 + 1) as i32,
            item_1,
            has_2,
            idx_2: (g0 + 2) as i32,
            item_2,
        });
        idx += 3;
    }
    triples
}

/// Launches the native cross-platform Slint desktop GUI window for Manifold.
pub fn run_desktop_gui(initial_state: ManifoldAppState) -> Result<(), String> {
    let window = ManifoldWindow::new().map_err(|e| e.to_string())?;
    window.set_app_version_label(current_app_version_label().into());
    let rt = Rc::new(RefCell::new(GuiRuntime::new(initial_state)));

    rt.borrow_mut().restore_saved_session(&window);
    sync_window_from_runtime(&window, &mut rt.borrow_mut());

    // Activate Left or Right pane
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_activate_pane(move |is_right| {
            let mut guard = rt.borrow_mut();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.app_state.active_pane = if use_right {
                ActivePane::Right
            } else {
                ActivePane::Left
            };
            guard.sync_terminal_cwd();
            guard.sync_selected_sidebar_to_active_dir();
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Toggle Dual-Pane vs Single-Pane mode & explicit layout selector (Single / SplitH / SplitV)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_dual_pane(move || {
            let mut guard = rt.borrow_mut();
            let next = !guard.app_state.dual_pane_enabled;
            guard.app_state.dual_pane_enabled = next;
            guard.app_state.panes_layout = if next {
                PanesLayout::SplitHorizontal
            } else {
                PanesLayout::Single
            };
            if !next {
                guard.app_state.active_pane = ActivePane::Left;
            }
            guard.sync_terminal_cwd();
            guard.sync_selected_sidebar_to_active_dir();
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_panes_layout(move |layout_idx| {
            let mut guard = rt.borrow_mut();
            let layout = match layout_idx {
                1 => PanesLayout::SplitHorizontal,
                2 => PanesLayout::SplitVertical,
                _ => PanesLayout::Single,
            };
            guard.app_state.panes_layout = layout;
            guard.app_state.dual_pane_enabled = layout != PanesLayout::Single;
            if !guard.app_state.dual_pane_enabled {
                guard.app_state.active_pane = ActivePane::Left;
            }
            guard.sync_terminal_cwd();
            guard.sync_selected_sidebar_to_active_dir();
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_virtual_scroll_row(move |is_right, top_row| {
            let mut guard = rt.borrow_mut();
            let clamped = top_row.max(0) as usize;
            let changed = if is_right {
                let prev = guard.right_virtual_top_row;
                guard.right_virtual_top_row = clamped;
                prev != clamped
            } else {
                let prev = guard.left_virtual_top_row;
                guard.left_virtual_top_row = clamped;
                prev != clamped
            };
            if changed {
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Draggable Dual-Pane Splitter Handle (Section 2.2)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_split_ratio(move |ratio| {
            let mut guard = rt.borrow_mut();
            let next_ratio = ratio.clamp(0.20, 0.80);
            guard.app_state.split_ratio = next_ratio;
            if let Some(w) = win_weak.upgrade() {
                w.set_split_ratio(next_ratio);
            }
        });
    }

    // Interactive Breadcrumb Node Navigation & Sibling Popover Dropdown (Section 1.2)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_navigate_to_breadcrumb(move |is_right, target_path| {
            let mut guard = rt.borrow_mut();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.app_state.active_pane = if use_right {
                ActivePane::Right
            } else {
                ActivePane::Left
            };
            let dest = target_path.to_string();
            if !dest.is_empty() {
                guard.navigate_to_path(use_right, dest);
            }
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_sibling_dropdown(move |is_right, seg_path, x_pos, y_pos| {
            let guard = rt.borrow();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let seg_str = seg_path.to_string();
            let parent_dir = vfs_parent_dir(&seg_str).unwrap_or_else(|| "/".to_string());
            let show_hidden = guard.app_state.toolbar.show_hidden_files;
            let raw_dirs = guard.list_sorted_dir(
                &parent_dir,
                show_hidden,
                "",
                SortColumn::Name,
                SortDirection::Ascending,
            );
            let siblings: Vec<BreadcrumbSegmentSlint> = raw_dirs
                .into_iter()
                .filter(|m| m.is_dir)
                .take(12)
                .map(|m| BreadcrumbSegmentSlint {
                    label: SharedString::from(m.name),
                    target_path: SharedString::from(&m.path),
                    is_last: m.path == seg_str,
                })
                .collect();
            if let Some(w) = win_weak.upgrade() {
                w.set_sibling_dropdown_is_right(use_right);
                w.set_sibling_dropdown_x(x_pos);
                w.set_sibling_dropdown_y(y_pos);
                w.set_sibling_dropdown_items(ModelRc::new(VecModel::from(siblings)));
                w.set_show_sibling_dropdown(true);
            }
        });
    }

    // Dual-Mode Path Bar Edit Mode Commit (`Enter`) & Tab Auto-Completion (Section 1.2)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_commit_path_input(move |is_right, raw_input| {
            let mut guard = rt.borrow_mut();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let expanded = expand_tilde_path(raw_input.trim());
            let is_remote = split_remote_uri(&expanded).is_some();
            let is_archive =
                split_archive_uri(&expanded).is_some_and(|(a, _)| Path::new(&a).is_file());
            let target = if is_remote || is_archive || Path::new(&expanded).is_dir() {
                Some(expanded)
            } else if Path::new(&expanded).exists() {
                Path::new(&expanded)
                    .parent()
                    .and_then(|p| p.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            };
            if let Some(valid_dir) = target {
                let msg = format!("[PATH-BAR] Navigated -> {valid_dir}");
                guard.logs.push(msg);
                guard.navigate_to_path(use_right, valid_dir);
            }
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_autocomplete_path_input(move |is_right, raw_input| {
            let guard = rt.borrow();
            let expanded = expand_tilde_path(raw_input.trim());
            let completed_opt = complete_directory_prefix(&guard.local_vfs, &expanded);
            if let (Some(completed), Some(w)) = (completed_opt, win_weak.upgrade()) {
                if is_right {
                    w.set_right_path_input(completed.into());
                } else {
                    w.set_left_path_input(completed.into());
                }
            }
        });
    }

    // Back / Forward History
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_go_back(move || {
            let mut guard = rt.borrow_mut();
            guard.go_back();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_go_forward(move || {
            let mut guard = rt.borrow_mut();
            guard.go_forward();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Sidebar item navigation & connection eject/disconnect
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_sidebar_item(move |idx| {
            let mut guard = rt.borrow_mut();
            let Some((item_id, target)) = guard.sidebar_targets.get(idx as usize).cloned() else {
                return;
            };
            if target == "connect://dialog" {
                if let Some(w) = win_weak.upgrade() {
                    w.set_show_connect_modal(true);
                }
                return;
            }
            if !target.is_empty() {
                guard.selected_sidebar_id = Some(item_id);
                let is_right = guard.is_right_effective();
                guard.logs.push(format!("[NAV] Sidebar -> {target}"));
                guard.navigate_to_path(is_right, target);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_eject_sidebar_item(move |idx| {
            let mut guard = rt.borrow_mut();
            let entry_opt = guard.sidebar_targets.get(idx as usize).cloned();
            if let Some((item_id, target_path)) = entry_opt {
                if let Some((prefix, _)) = split_remote_uri(&target_path) {
                    guard.remote_backends.remove(&prefix);
                } else if item_id.starts_with("conn-mnt-") {
                    try_unmount_network_share(&target_path);
                }
                for sec in &mut guard.app_state.sidebar_sections {
                    sec.items.retain(|i| i.id != item_id);
                }
                if guard.saved_connections.remove_profile(&item_id) {
                    let _ = guard.saved_connections.save_default();
                }
                if guard
                    .prefs
                    .remove_favorite_by_id_or_path(&item_id, &target_path)
                {
                    let _ = guard.prefs.save_default();
                }
                guard.sync_selected_sidebar_to_active_dir();
                let msg = format!("[DISCONNECT] Disconnected {item_id}");
                guard.logs.push(msg);
                let toast = format!("Removed {item_id}");
                guard.show_toast("info", toast);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Sidebar Right-Click Context Menu (Devices, Connections, Favorites)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_sidebar_context_menu(move |idx, x, y| {
            let mut guard = rt.borrow_mut();
            let u_idx = idx as usize;
            let Some((item_id, target_path)) = guard.sidebar_targets.get(u_idx).cloned() else {
                return;
            };
            if target_path.is_empty() {
                return;
            }
            guard.sidebar_context_idx = u_idx;
            guard.selected_sidebar_id = Some(item_id.clone());
            let mut found_sec = "Devices".to_string();
            let mut found_label = item_id.clone();
            let mut found_icon = "drive".to_string();
            let mut found_eject = false;
            for sec in &guard.app_state.sidebar_sections {
                for item in &sec.items {
                    if item.id == item_id {
                        found_sec = sec.title.clone();
                        found_label = item.label.clone();
                        found_icon = item.icon_kind.clone();
                        found_eject = item.can_eject;
                    }
                    for child in &item.children {
                        if child.id == item_id {
                            found_sec = sec.title.clone();
                            found_label = child.label.clone();
                            found_icon = child.icon_kind.clone();
                            found_eject = child.can_eject;
                        }
                    }
                }
            }
            let is_fav = found_sec == "Favorites" || guard.is_path_in_favorites(&target_path);
            let color_lbl = guard
                .sidebar_colors
                .get(&item_id)
                .copied()
                .unwrap_or(ColorLabel::None);
            let color_idx = color_label_to_index(color_lbl);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
                w.set_sidebar_context_idx(idx);
                w.set_sidebar_context_x(x);
                w.set_sidebar_context_y(y);
                w.set_sidebar_context_label(found_label.into());
                w.set_sidebar_context_section(found_sec.into());
                w.set_sidebar_context_icon(found_icon.into());
                w.set_sidebar_context_can_eject(found_eject);
                w.set_sidebar_context_is_favorite(is_fav);
                w.set_sidebar_context_color_index(color_idx);
                w.set_show_sidebar_context_menu(true);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_execute_sidebar_context_action(move |action| {
            let mut guard = rt.borrow_mut();
            let u_idx = guard.sidebar_context_idx;
            let Some((item_id, target_path)) = guard.sidebar_targets.get(u_idx).cloned() else {
                return;
            };
            if target_path.is_empty() {
                return;
            }
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let sec_name = w.get_sidebar_context_section().to_string();
            let item_label = w.get_sidebar_context_label().to_string();

            match action.as_str() {
                "open" => {
                    let is_right = guard.is_right_effective();
                    guard.navigate_to_path(is_right, target_path);
                }
                "open_new_tab" => {
                    let is_right = guard.is_right_effective();
                    let title = guard.format_tab_title(&target_path);
                    let vfs_url = if target_path.contains("://") {
                        target_path.clone()
                    } else {
                        format!("file://{target_path}")
                    };
                    let new_tab = crate::PaneTabModel {
                        id: format!("tab-sb-{}", guard.logs.len()),
                        title: title.clone(),
                        icon_kind: "folder".to_string(),
                        vfs_url,
                        current_path: target_path,
                        view_mode: PaneViewMode::List,
                        sort_column: SortColumn::Name,
                        sort_direction: SortDirection::Ascending,
                        inline_filter: String::new(),
                        expanded_folders: HashSet::new(),
                        selected_paths: HashSet::new(),
                        back_stack: Vec::new(),
                        forward_stack: Vec::new(),
                    };
                    if is_right {
                        guard.app_state.right_tabs.push(new_tab);
                        let new_idx = guard.app_state.right_tabs.len() - 1;
                        guard.app_state.right_active_tab = new_idx;
                    } else {
                        guard.app_state.left_tabs.push(new_tab);
                        let new_idx = guard.app_state.left_tabs.len() - 1;
                        guard.app_state.left_active_tab = new_idx;
                    }
                    guard.reload_pane_entries(is_right);
                    let toast = format!("Opened \"{title}\" in new tab");
                    guard.show_toast("info", toast);
                }
                "open_other_pane" => {
                    if !guard.app_state.dual_pane_enabled {
                        guard.app_state.dual_pane_enabled = true;
                        guard.app_state.panes_layout = PanesLayout::SplitHorizontal;
                    }
                    guard.app_state.active_pane = ActivePane::Right;
                    let title = guard.format_tab_title(&target_path);
                    guard.navigate_to_path(true, target_path);
                    let toast = format!("Opened \"{title}\" in Dual Pane");
                    guard.show_toast("info", toast);
                }
                "edit" => {
                    let prof_opt = guard
                        .saved_connections
                        .connections
                        .iter()
                        .find(|p| p.id == item_id)
                        .cloned();
                    if let Some(prof) = prof_opt {
                        populate_connect_modal_from_profile(&w, &guard, &prof);
                        w.set_show_connect_modal(true);
                    } else if sec_name == "Connections" && split_remote_uri(&target_path).is_some()
                    {
                        let mut fallback =
                            ConnectionProfile::new_sftp_default(&item_id, &item_label);
                        if let Some((prefix, rel)) = split_remote_uri(&target_path) {
                            let stripped = prefix.split("://").nth(1).unwrap_or(&item_label);
                            if let Some((u, h)) = stripped.split_once('@') {
                                fallback.username = u.to_string();
                                fallback.host = Some(h.to_string());
                            } else {
                                fallback.host = Some(stripped.to_string());
                            }
                            fallback.base_path = rel;
                        }
                        populate_connect_modal_from_profile(&w, &guard, &fallback);
                        w.set_show_connect_modal(true);
                    } else {
                        let title = format!("Edit Label for \"{item_label}\"");
                        w.set_input_modal_mode("rename_sidebar".into());
                        w.set_input_modal_title(title.into());
                        w.set_input_modal_value(item_label.into());
                        w.set_show_input_modal(true);
                    }
                }
                "get_info" => {
                    let is_right = guard.is_right_effective();
                    guard.navigate_to_path(is_right, target_path.clone());
                    let vfs = guard.backend_for_path(&target_path);
                    if let Ok(meta) = vfs.metadata(&target_path) {
                        let mod_str = meta.formatted_iso_datetime();
                        let vm = FileInspectorViewModel::from_metadata(&meta, &mod_str, None);
                        guard.app_state.inspector_vm = Some(vm);
                    }
                    guard.app_state.inspector_open = true;
                    w.set_inspector_tab_index(0);
                    let toast = format!("Inspecting \"{item_label}\"");
                    guard.show_toast("info", toast);
                }
                "terminal" => {
                    let is_right = guard.is_right_effective();
                    guard.navigate_to_path(is_right, target_path.clone());
                    guard.open_terminal_at_path(&target_path);
                    w.set_show_terminal_drawer(true);
                    w.invoke_focus_terminal_input();
                    let toast = format!("Opened Bottom Terminal at {item_label}");
                    guard.logs.push(format!("[TERM] {toast}"));
                    guard.show_toast("info", toast);
                }
                "refresh" => {
                    if let Some((prefix, _)) = split_remote_uri(&target_path) {
                        guard.remote_backends.remove(&prefix);
                        guard.ensure_remote_connected(&prefix);
                    }
                    let is_right = guard.is_right_effective();
                    guard.navigate_to_path(is_right, target_path.clone());
                    let label = format!("Refreshed {item_label}");
                    guard.start_tracked_task(&label, &target_path, 4096, label.clone(), "info");
                }
                "copy_path" => {
                    copy_string_to_system_clipboard(&target_path);
                    let toast = format!("Copied path: {target_path}");
                    guard.logs.push(format!("[PATH] {toast}"));
                    guard.show_toast("copy", toast);
                }
                "add_favorite" => {
                    guard.pin_folder_to_favorites(&target_path, &item_label);
                }
                "remove_favorite" => {
                    let id_arg = if sec_name == "Favorites" {
                        item_id.as_str()
                    } else {
                        ""
                    };
                    guard.unpin_folder_from_favorites(id_arg, &target_path);
                    guard.sync_selected_sidebar_to_active_dir();
                }
                "duplicate_connection" => {
                    let prof_opt = guard
                        .saved_connections
                        .connections
                        .iter()
                        .find(|p| p.id == item_id)
                        .cloned();
                    if let Some(mut prof) = prof_opt {
                        let count = guard.saved_connections.connections.len() + 1;
                        prof.id = format!("{}-copy-{count}", prof.id);
                        prof.name = format!("{} Copy", prof.name);
                        let vfs_url = profile_to_vfs_url(&prof);
                        if prof.color_label != ColorLabel::None {
                            guard
                                .sidebar_colors
                                .insert(prof.id.clone(), prof.color_label);
                        }
                        guard.saved_connections.upsert_profile(prof.clone());
                        let _ = guard.saved_connections.save_default();
                        let conn_sec = guard
                            .app_state
                            .sidebar_sections
                            .iter_mut()
                            .find(|s| s.title == "Connections");
                        if let Some(sec) = conn_sec {
                            sec.items.push(SidebarItem {
                                id: prof.id.clone(),
                                label: prof.name.clone(),
                                vfs_url,
                                icon_kind: "globe".to_string(),
                                capacity_badge: None,
                                can_eject: true,
                                children: vec![],
                                expanded: false,
                            });
                        }
                        let toast = format!("Duplicated connection \"{}\"", prof.name);
                        guard.show_toast("success", toast);
                    }
                }
                "eject" => {
                    if let Some((prefix, _)) = split_remote_uri(&target_path) {
                        guard.remote_backends.remove(&prefix);
                    } else if item_id.starts_with("conn-mnt-") {
                        try_unmount_network_share(&target_path);
                    }
                    for sec in &mut guard.app_state.sidebar_sections {
                        sec.items.retain(|i| i.id != item_id);
                    }
                    if guard.saved_connections.remove_profile(&item_id) {
                        let _ = guard.saved_connections.save_default();
                    }
                    if guard
                        .prefs
                        .remove_favorite_by_id_or_path(&item_id, &target_path)
                    {
                        let _ = guard.prefs.save_default();
                    }
                    guard.sync_selected_sidebar_to_active_dir();
                    let toast = format!("Removed \"{item_label}\"");
                    guard.logs.push(format!("[EJECT] {toast}"));
                    guard.show_toast("info", toast);
                }
                "move_fav_up" if guard.move_favorite_step(&item_id, -1) => {
                    let toast = format!("Moved \"{item_label}\" up in Favorites");
                    guard.show_toast("info", toast);
                }
                "move_fav_down" if guard.move_favorite_step(&item_id, 1) => {
                    let toast = format!("Moved \"{item_label}\" down in Favorites");
                    guard.show_toast("info", toast);
                }
                _ => {}
            }
            sync_window_from_runtime(&w, &mut guard);
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_reorder_sidebar_favorite(move |from_idx, to_idx| {
            if from_idx < 0 || to_idx < 0 || from_idx == to_idx {
                return;
            }
            let mut guard = rt.borrow_mut();
            let f_u = from_idx as usize;
            let t_u = to_idx as usize;
            let from_id = guard
                .sidebar_targets
                .get(f_u)
                .map(|(id, _)| id.clone())
                .unwrap_or_default();
            let to_id = guard
                .sidebar_targets
                .get(t_u)
                .map(|(id, _)| id.clone())
                .unwrap_or_default();
            if guard.reorder_favorites_by_item_ids(&from_id, &to_id) {
                let toast = "Reordered Favorites".to_string();
                guard.show_toast("info", toast);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_sidebar_item_color(move |c_idx| {
            let mut guard = rt.borrow_mut();
            let u_idx = guard.sidebar_context_idx;
            let Some((item_id, target_path)) = guard.sidebar_targets.get(u_idx).cloned() else {
                return;
            };
            let color = index_to_color_label(c_idx);
            if color == ColorLabel::None {
                guard.sidebar_colors.remove(&item_id);
            } else {
                guard.sidebar_colors.insert(item_id.clone(), color);
            }
            guard.prefs.set_sidebar_color(&item_id, color);
            if !target_path.is_empty() && split_remote_uri(&target_path).is_none() {
                let _ = guard.local_vfs.set_color_label(&target_path, color);
                guard.prefs.set_file_color_label(&target_path, color);
                guard.invalidate_dir_cache();
            }
            let _ = guard.prefs.save_default();
            if let Some(prof) = guard
                .saved_connections
                .connections
                .iter_mut()
                .find(|p| p.id == item_id)
            {
                prof.color_label = color;
                let _ = guard.saved_connections.save_default();
            }
            let toast = format!("Updated color tag for {item_id}");
            guard.show_toast("info", toast);
            if let Some(w) = win_weak.upgrade() {
                w.set_sidebar_context_color_index(c_idx);
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_add_connection_to_favorites(move || {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let mut guard = rt.borrow_mut();
            let proto_str = w.get_connect_protocol().to_string();
            let server = w.get_connect_server().trim().to_string();
            let user = w.get_connect_username().trim().to_string();
            let remote_path = w.get_connect_remote_path().trim().to_string();
            let custom_name = w.get_connect_name().trim().to_string();
            let color = index_to_color_label(w.get_connect_color_index());
            let host = if server.is_empty() {
                "localhost".to_string()
            } else {
                server
            };
            let label = if !custom_name.is_empty() {
                custom_name
            } else if user.is_empty() {
                host.clone()
            } else {
                format!("{user}@{host}")
            };
            let proto_low = proto_str.to_lowercase();
            let clean_path = remote_path.trim_start_matches('/');
            let vfs_url = if user.is_empty() {
                format!("{proto_low}://{host}/{clean_path}")
            } else {
                format!("{proto_low}://{user}@{host}/{clean_path}")
            };
            let fav_id = format!("fav-conn-{}", guard.logs.len() + 1);
            if color != ColorLabel::None {
                guard.sidebar_colors.insert(fav_id.clone(), color);
                guard.prefs.set_sidebar_color(&fav_id, color);
            }
            guard.pin_folder_to_favorites(&vfs_url, &label);
            sync_window_from_runtime(&w, &mut guard);
        });
    }

    // Pin or unpin current or selected directory in Sidebar Favorites (with persistence)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_add_current_to_favorites(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let sel_dir = guard
                .selected_metadata()
                .filter(|m| m.is_dir)
                .map(|m| (m.path.clone(), m.name.clone()));
            let (target_path, label) = sel_dir.unwrap_or_else(|| {
                let p = guard.current_pane_path(is_right);
                let l = guard.format_tab_title(&p);
                (p, l)
            });
            if guard.is_path_in_favorites(&target_path) {
                guard.unpin_folder_from_favorites("", &target_path);
            } else {
                guard.pin_folder_to_favorites(&target_path, &label);
            }
            guard.sync_selected_sidebar_to_active_dir();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_add_current_folder_to_favorites(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let target_path = guard.current_pane_path(is_right);
            let label = guard.format_tab_title(&target_path);
            if guard.is_path_in_favorites(&target_path) {
                guard.unpin_folder_from_favorites("", &target_path);
            } else {
                guard.pin_folder_to_favorites(&target_path, &label);
            }
            guard.sync_selected_sidebar_to_active_dir();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Tab selection
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_tab(move |is_right, tab_idx| {
            let mut guard = rt.borrow_mut();
            let idx = tab_idx as usize;
            if is_right && idx < guard.app_state.right_tabs.len() {
                guard.app_state.right_active_tab = idx;
                guard.right_selected_idx = Some(0);
                guard.right_anchor_idx = Some(0);
                guard.right_selected_indices.clear();
                guard.right_selected_indices.insert(0);
            } else if !is_right && idx < guard.app_state.left_tabs.len() {
                guard.app_state.left_active_tab = idx;
                guard.left_selected_idx = Some(0);
                guard.left_anchor_idx = Some(0);
                guard.left_selected_indices.clear();
                guard.left_selected_indices.insert(0);
            }
            guard.reload_pane_entries(is_right);
            guard.sync_selected_sidebar_to_active_dir();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Add tab
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_add_tab(move |is_right| {
            let mut guard = rt.borrow_mut();
            let path = guard.current_pane_path(is_right);
            let title = guard.format_tab_title(&path);
            let vfs_url = if path.contains("://") {
                path.clone()
            } else {
                format!("file://{path}")
            };
            let new_tab = crate::PaneTabModel {
                id: format!("tab-{}", guard.logs.len()),
                title,
                icon_kind: "folder".to_string(),
                vfs_url,
                current_path: path,
                view_mode: if is_right {
                    PaneViewMode::Icons
                } else {
                    PaneViewMode::List
                },
                sort_column: SortColumn::Name,
                sort_direction: SortDirection::Ascending,
                inline_filter: String::new(),
                expanded_folders: HashSet::new(),
                selected_paths: HashSet::new(),
                back_stack: Vec::new(),
                forward_stack: Vec::new(),
            };
            if is_right {
                guard.app_state.right_tabs.push(new_tab);
                let new_idx = guard.app_state.right_tabs.len() - 1;
                guard.app_state.right_active_tab = new_idx;
            } else {
                guard.app_state.left_tabs.push(new_tab);
                let new_idx = guard.app_state.left_tabs.len() - 1;
                guard.app_state.left_active_tab = new_idx;
            }
            guard.reload_pane_entries(is_right);
            guard.sync_selected_sidebar_to_active_dir();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Close tab
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_close_tab(move |is_right, tab_idx| {
            let mut guard = rt.borrow_mut();
            let idx = tab_idx as usize;
            let state = &mut guard.app_state;
            if is_right && state.right_tabs.len() > 1 && idx < state.right_tabs.len() {
                state.right_tabs.remove(idx);
                let max_idx = state.right_tabs.len() - 1;
                if state.right_active_tab > max_idx {
                    state.right_active_tab = max_idx;
                }
            } else if !is_right && state.left_tabs.len() > 1 && idx < state.left_tabs.len() {
                state.left_tabs.remove(idx);
                let max_idx = state.left_tabs.len() - 1;
                if state.left_active_tab > max_idx {
                    state.left_active_tab = max_idx;
                }
            }
            guard.reload_pane_entries(is_right);
            guard.sync_selected_sidebar_to_active_dir();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Rearrange Folder Tabs via Drag & Drop (within a pane or across Dual Panes)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_move_tab(move |from_right, from_idx, to_right, to_idx| {
            let mut guard = rt.borrow_mut();
            let f_idx = from_idx.max(0) as usize;
            let t_idx = to_idx.max(0) as usize;
            if from_right == to_right {
                let tabs = if from_right {
                    &mut guard.app_state.right_tabs
                } else {
                    &mut guard.app_state.left_tabs
                };
                if f_idx >= tabs.len() {
                    return;
                }
                let clamped_to = t_idx.min(tabs.len() - 1);
                if f_idx == clamped_to {
                    return;
                }
                let item = tabs.remove(f_idx);
                tabs.insert(clamped_to, item);
                if from_right {
                    guard.app_state.right_active_tab = clamped_to;
                } else {
                    guard.app_state.left_active_tab = clamped_to;
                }
                guard.reload_pane_entries(from_right);
            } else {
                let can_remove = if from_right {
                    guard.app_state.right_tabs.len() > 1 && f_idx < guard.app_state.right_tabs.len()
                } else {
                    guard.app_state.left_tabs.len() > 1 && f_idx < guard.app_state.left_tabs.len()
                };
                if !can_remove {
                    return;
                }
                let item = if from_right {
                    let removed = guard.app_state.right_tabs.remove(f_idx);
                    let max_r = guard.app_state.right_tabs.len() - 1;
                    if guard.app_state.right_active_tab > max_r {
                        guard.app_state.right_active_tab = max_r;
                    }
                    removed
                } else {
                    let removed = guard.app_state.left_tabs.remove(f_idx);
                    let max_l = guard.app_state.left_tabs.len() - 1;
                    if guard.app_state.left_active_tab > max_l {
                        guard.app_state.left_active_tab = max_l;
                    }
                    removed
                };
                if to_right {
                    let ins = t_idx.min(guard.app_state.right_tabs.len());
                    guard.app_state.right_tabs.insert(ins, item);
                    guard.app_state.right_active_tab = ins;
                    guard.app_state.active_pane = ActivePane::Right;
                } else {
                    let ins = t_idx.min(guard.app_state.left_tabs.len());
                    guard.app_state.left_tabs.insert(ins, item);
                    guard.app_state.left_active_tab = ins;
                    guard.app_state.active_pane = ActivePane::Left;
                }
                guard.reload_pane_entries(false);
                guard.reload_pane_entries(true);
            }
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Select file row (with Shift/Cmd multi-selection and single-click-on-selected rename)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_file_row(move |is_right, row_idx| {
            let mut guard = rt.borrow_mut();
            guard.native_os_drag_handed_off = false;
            guard.pending_rename_click = None;
            guard.hover_candidate = None;
            guard.last_hovered_cell = None;
            let idx = row_idx as usize;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.last_row_click = Some((use_right, idx, Instant::now()));
            guard.apply_row_selection(use_right, idx, false, false);
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_hover_preview(false);
                reset_slint_modifiers(&w);
                w.set_show_tab_preview(true);
                sync_selection_and_inspector(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_file_row_mod(move |is_right, row_idx, is_shift, is_cmd| {
            let (clean_shift, clean_cmd) = if let Some(w) = win_weak.upgrade() {
                sanitize_click_modifiers(&w, is_shift, is_cmd)
            } else {
                (is_shift, is_cmd)
            };
            let mut guard = rt.borrow_mut();
            guard.native_os_drag_handed_off = false;
            guard.hover_candidate = None;
            guard.last_hovered_cell = None;
            let idx = row_idx as usize;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let was_sole_selected = if use_right {
                guard.right_selected_indices.len() == 1
                    && guard.right_selected_indices.contains(&idx)
            } else {
                guard.left_selected_indices.len() == 1 && guard.left_selected_indices.contains(&idx)
            };
            let not_fast_double_click = guard.last_row_click.is_none_or(|(r, i, t)| {
                let dt = t.elapsed();
                r != use_right || i != idx || dt >= Duration::from_millis(380)
            });
            let now = Instant::now();
            if !clean_shift && !clean_cmd && was_sole_selected && not_fast_double_click {
                guard.pending_rename_click = Some((use_right, idx, now));
            } else {
                guard.pending_rename_click = None;
            }
            guard.last_row_click = if !clean_shift && !clean_cmd {
                Some((use_right, idx, now))
            } else {
                None
            };
            guard.apply_row_selection(use_right, idx, clean_shift, clean_cmd);
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_hover_preview(false);
                w.set_show_tab_preview(true);
                sync_selection_and_inspector(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_shift_drag_select(move |is_right, target_idx| {
            let mut guard = rt.borrow_mut();
            guard.pending_rename_click = None;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let len = if use_right {
                guard.right_entries.len()
            } else {
                guard.left_entries.len()
            };
            if len == 0 {
                return;
            }
            let clamped = (target_idx.max(0) as usize).min(len - 1);
            guard.apply_row_selection(use_right, clamped, true, false);
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_selection_and_inspector(&w, &mut guard);
            }
        });
    }

    // Explicit open file row callback
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_file_row(move |is_right, row_idx| {
            let mut guard = rt.borrow_mut();
            guard.pending_rename_click = None;
            guard.last_row_click = None;
            guard.hover_candidate = None;
            guard.last_hovered_cell = None;
            let in_cooldown = guard
                .last_nav_instant
                .is_some_and(|t| t.elapsed() < Duration::from_millis(280));
            if in_cooldown {
                return;
            }
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let opt_idx = (row_idx >= 0).then_some(row_idx as usize);
            guard.open_entry_at(use_right, opt_idx);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_hover_preview(false);
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Mouseover File Row / Cell callbacks (hover preview disabled per user preference)
    window.on_hover_file_row(|_is_right, _row_idx, _abs_x, _abs_y| {});
    window.on_hover_leave_file_row(|_is_right, _row_idx| {});

    // Inline Folder Disclosure Triangle ('›' / '⌄')
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_expand_folder(move |is_right, row_idx| {
            let mut guard = rt.borrow_mut();
            let idx = row_idx as usize;
            let path_opt = if is_right {
                guard
                    .right_entries
                    .get(idx)
                    .filter(|r| is_browsable_folder(&r.meta))
                    .map(|r| r.meta.path.clone())
            } else {
                guard
                    .left_entries
                    .get(idx)
                    .filter(|r| is_browsable_folder(&r.meta))
                    .map(|r| r.meta.path.clone())
            };
            if let Some(folder_path) = path_opt {
                let tab_opt = if is_right {
                    let t_idx = guard.app_state.right_active_tab;
                    guard.app_state.right_tabs.get_mut(t_idx)
                } else {
                    let t_idx = guard.app_state.left_active_tab;
                    guard.app_state.left_tabs.get_mut(t_idx)
                };
                if let Some(tab) = tab_opt {
                    if !tab.expanded_folders.remove(&folder_path) {
                        tab.expanded_folders.insert(folder_path);
                    }
                }
                guard.reload_pane_entries(is_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Right-click Context Menu on any file or folder row (preserves multi-selection)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_context_menu_for_row(move |is_right, row_idx, x_pos, y_pos| {
            let mut guard = rt.borrow_mut();
            guard.pending_rename_click = None;
            guard.last_row_click = None;
            let idx = row_idx as usize;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let already_selected = if use_right {
                guard.right_selected_indices.contains(&idx)
            } else {
                guard.left_selected_indices.contains(&idx)
            };
            if use_right {
                guard.app_state.active_pane = ActivePane::Right;
                guard.right_selected_idx = Some(idx);
                if !already_selected {
                    guard.right_selected_indices.clear();
                    guard.right_selected_indices.insert(idx);
                    guard.right_anchor_idx = Some(idx);
                }
            } else {
                guard.app_state.active_pane = ActivePane::Left;
                guard.left_selected_idx = Some(idx);
                if !already_selected {
                    guard.left_selected_indices.clear();
                    guard.left_selected_indices.insert(idx);
                    guard.left_anchor_idx = Some(idx);
                }
            }
            guard.sync_terminal_cwd();
            guard.refresh_headers_and_inspector();
            let sel_list = guard.selected_metadata_list(use_right);
            let target_name = if sel_list.len() > 1 {
                format!("{} items selected", sel_list.len())
            } else {
                sel_list.first().map(|m| m.name.clone()).unwrap_or_default()
            };
            let target_is_dir = sel_list.len() == 1 && is_browsable_folder(&sel_list[0]);
            let target_is_app = sel_list.len() == 1 && is_macos_app_bundle(&sel_list[0]);
            let target_is_archive = sel_list.len() == 1 && is_extractable_archive(&sel_list[0]);
            let cur_pane_dir = guard.current_pane_path(use_right);
            let active_filter = if use_right {
                guard
                    .app_state
                    .right_tabs
                    .get(guard.app_state.right_active_tab)
                    .map(|t| t.inline_filter.as_str())
                    .unwrap_or("")
            } else {
                guard
                    .app_state
                    .left_tabs
                    .get(guard.app_state.left_active_tab)
                    .map(|t| t.inline_filter.as_str())
                    .unwrap_or("")
            };
            let can_show_enclosing =
                should_show_in_enclosing_folder(active_filter, &cur_pane_dir, &sel_list);
            let fav_check_path = sel_list
                .first()
                .filter(|m| is_browsable_folder(m))
                .map(|m| m.path.clone())
                .unwrap_or(cur_pane_dir);
            let target_is_fav = guard.is_path_in_favorites(&fav_check_path);
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
                let has_clip =
                    !guard.clipboard_paths.is_empty() || !read_files_from_os_clipboard().is_empty();
                w.set_has_clipboard_item(has_clip);
                sync_selection_and_inspector(&w, &mut guard);
                w.set_context_is_blank(false);
                w.set_context_selected_count(sel_list.len() as i32);
                w.set_context_target_is_dir(target_is_dir);
                w.set_context_target_is_archive(target_is_archive);
                w.set_context_target_is_app_bundle(target_is_app);
                w.set_context_can_show_enclosing(can_show_enclosing);
                w.set_context_target_is_favorite(target_is_fav);
                w.set_context_target_name(target_name.into());
                w.set_context_menu_x(x_pos);
                w.set_context_menu_y(y_pos);
                w.set_show_context_menu(true);
            }
        });
    }

    // Blank / Empty Directory Context Menu (New Folder, New File, Paste, Add to Favorites)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_blank_context_menu(move |is_right, x_pos, y_pos| {
            let mut guard = rt.borrow_mut();
            guard.pending_rename_click = None;
            guard.last_row_click = None;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.app_state.active_pane = if use_right {
                ActivePane::Right
            } else {
                ActivePane::Left
            };
            guard.sync_terminal_cwd();
            guard.refresh_headers_and_inspector();
            let cur_path = guard.current_pane_path(use_right);
            let folder_label = guard.format_tab_title(&cur_path);
            let target_is_fav = guard.is_path_in_favorites(&cur_path);
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
                let has_clip =
                    !guard.clipboard_paths.is_empty() || !read_files_from_os_clipboard().is_empty();
                w.set_has_clipboard_item(has_clip);
                sync_selection_and_inspector(&w, &mut guard);
                w.set_context_is_blank(true);
                w.set_context_selected_count(0);
                w.set_context_target_is_dir(true);
                w.set_context_target_is_archive(false);
                w.set_context_target_is_app_bundle(false);
                w.set_context_can_show_enclosing(false);
                w.set_context_target_is_favorite(target_is_fav);
                w.set_context_target_name(folder_label.into());
                w.set_context_menu_x(x_pos);
                w.set_context_menu_y(y_pos);
                w.set_show_context_menu(true);
            }
        });
    }

    // Execute Context Menu action
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_execute_context_action(move |action| {
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
            }
            let mut guard = rt.borrow_mut();
            guard.pending_rename_click = None;
            guard.last_row_click = None;
            let is_right = guard.is_right_effective();
            let sel_list = guard.selected_metadata_list(is_right);
            let sel_meta = sel_list.first().cloned();

            match action.as_str() {
                "open" => {
                    guard.open_entry_at(is_right, None);
                }
                "show_in_enclosing_folder" => {
                    if let Some(meta) = sel_meta {
                        guard.show_entry_in_enclosing_folder(is_right, &meta.path);
                        if let Some(w) = win_weak.upgrade() {
                            sync_window_from_runtime(&w, &mut guard);
                            let sel_idx = if is_right {
                                guard.right_selected_idx
                            } else {
                                guard.left_selected_idx
                            };
                            if let Some(idx) = sel_idx {
                                w.invoke_ensure_row_visible(is_right, idx as i32);
                            }
                        }
                        return;
                    }
                }
                "show_archive_contents" => {
                    if let Some(meta) = sel_meta {
                        if let Ok(arch_vfs) = ArchiveBackend::open_from_disk(&meta.path) {
                            let virt_uri = format!("archive://{}#/", meta.path);
                            guard.archive_backends.insert(meta.path.clone(), arch_vfs);
                            let log_msg = format!("[ARCHIVE-VIRTUAL] -> {virt_uri}");
                            guard.logs.push(log_msg);
                            guard.navigate_to_path(is_right, virt_uri);
                        } else {
                            let is_mac = cfg!(target_os = "macos");
                            match open_archive_contents_dir(&meta, is_mac) {
                                Ok(stage_dir) => {
                                    let log_msg = format!("[ARCHIVE-CONTENTS] -> {stage_dir}");
                                    guard.logs.push(log_msg);
                                    guard.invalidate_dir_cache();
                                    guard.navigate_to_path(is_right, stage_dir);
                                }
                                Err(err) => {
                                    let log_msg = format!("[ARCHIVE-CONTENTS_ERR] {err}");
                                    guard.logs.push(log_msg);
                                    let toast = format!("Archive error: {err}");
                                    guard.show_toast("error", toast);
                                }
                            }
                        }
                    }
                }
                "show_package_contents" => {
                    if let Some(meta) = sel_meta {
                        let base = meta.path.trim_end_matches('/');
                        let contents_dir = format!("{base}/Contents");
                        let target = if Path::new(&contents_dir).is_dir() {
                            contents_dir
                        } else {
                            meta.path
                        };
                        let log_msg = format!("[PACKAGE-CONTENTS] -> {target}");
                        guard.logs.push(log_msg);
                        guard.navigate_to_path(is_right, target);
                    }
                }
                "open_with" => {
                    if let (Some(meta), Some(w)) = (sel_meta, win_weak.upgrade()) {
                        let apps = discover_open_with_apps(&meta);
                        w.set_open_with_target_name(meta.name.into());
                        w.set_open_with_target_path(meta.path.into());
                        w.set_open_with_apps(ModelRc::new(VecModel::from(apps)));
                        w.set_open_with_selected_idx(0);
                        w.set_open_with_custom_cmd(SharedString::default());
                        w.set_show_open_with_modal(true);
                        return;
                    }
                }
                "open_new_tab" => {
                    let target_dir = match sel_meta {
                        Some(meta) if meta.is_dir => meta.path,
                        Some(meta) => vfs_parent_dir(&meta.path).unwrap_or_else(|| "/".to_string()),
                        None => guard.current_pane_path(is_right),
                    };
                    let title = guard.format_tab_title(&target_dir);
                    let vfs_url = if target_dir.contains("://") {
                        target_dir.clone()
                    } else {
                        format!("file://{target_dir}")
                    };
                    let new_tab = crate::PaneTabModel {
                        id: format!("tab-{}", guard.logs.len()),
                        title,
                        icon_kind: "folder".to_string(),
                        vfs_url,
                        current_path: target_dir,
                        view_mode: PaneViewMode::List,
                        sort_column: SortColumn::Name,
                        sort_direction: SortDirection::Ascending,
                        inline_filter: String::new(),
                        expanded_folders: HashSet::new(),
                        selected_paths: HashSet::new(),
                        back_stack: Vec::new(),
                        forward_stack: Vec::new(),
                    };
                    if is_right {
                        guard.app_state.right_tabs.push(new_tab);
                        guard.app_state.right_active_tab = guard.app_state.right_tabs.len() - 1;
                    } else {
                        guard.app_state.left_tabs.push(new_tab);
                        guard.app_state.left_active_tab = guard.app_state.left_tabs.len() - 1;
                    }
                    guard.reload_pane_entries(is_right);
                    guard.sync_selected_sidebar_to_active_dir();
                }
                "trash" => {
                    if let Some(w) = win_weak.upgrade() {
                        prompt_delete_confirmation(&w, &mut guard, false);
                        sync_window_from_runtime(&w, &mut guard);
                        return;
                    }
                }
                "delete_permanent" => {
                    if let Some(w) = win_weak.upgrade() {
                        prompt_delete_confirmation(&w, &mut guard, true);
                        sync_window_from_runtime(&w, &mut guard);
                        return;
                    }
                }
                "get_info" => {
                    guard.app_state.inspector_open = true;
                    guard.refresh_headers_and_inspector();
                    if let Some(w) = win_weak.upgrade() {
                        w.set_inspector_tab_index(0);
                    }
                }
                "quick_look" => {
                    if let Some(w) = win_weak.upgrade() {
                        w.set_preview_minion_zoom(1.0);
                        w.set_show_preview_minion(true);
                        sync_inspector_to_window(&w, &mut guard);
                        return;
                    }
                }
                "rename" => {
                    if let Some(w) = win_weak.upgrade() {
                        open_rename_or_batch_modal(&w, &mut guard);
                        return;
                    }
                }
                "new_folder_with_selection" if !sel_list.is_empty() => {
                    let cur_dir = guard.current_pane_path(is_right);
                    let existing: HashSet<String> = guard
                        .list_raw_dir_cached(&cur_dir)
                        .into_iter()
                        .map(|m| m.name)
                        .collect();
                    let base_name = "New Folder with Items";
                    let mut folder_name = base_name.to_string();
                    let mut seq = 2usize;
                    while existing.contains(&folder_name) {
                        folder_name = format!("{base_name} {seq}");
                        seq += 1;
                    }
                    let new_dir_path = join_vfs_path(&cur_dir, &folder_name);
                    if guard.vfs_create_dir(&new_dir_path).is_ok() {
                        let mut moved_count = 0usize;
                        for item in &sel_list {
                            if item.path == new_dir_path {
                                continue;
                            }
                            let dst = join_vfs_path(&new_dir_path, &item.name);
                            if guard.vfs_move_entry(&item.path, &dst).is_ok() {
                                moved_count += 1;
                            }
                        }
                        let msg =
                            format!("[NEW-FOLDER-SELECTION] {folder_name} ({moved_count} items)");
                        guard.logs.push(msg);
                        guard.invalidate_dir_cache();
                        guard.reload_pane_entries(false);
                        guard.reload_pane_entries(true);
                        guard.select_single_path_or_first(is_right, Some(&new_dir_path));
                        if let Some(w) = win_weak.upgrade() {
                            sync_window_from_runtime(&w, &mut guard);
                            let title = format!("Rename \"{folder_name}\"");
                            w.set_input_modal_mode("rename".into());
                            w.set_input_modal_title(title.into());
                            w.set_input_modal_value(folder_name.into());
                            w.set_show_input_modal(true);
                            w.invoke_focus_input_modal();
                            return;
                        }
                    }
                }
                "new_folder" => {
                    if let Some(w) = win_weak.upgrade() {
                        w.set_input_modal_mode("new_folder".into());
                        w.set_input_modal_title("New Folder".into());
                        w.set_input_modal_value("Untitled Folder".into());
                        w.set_show_input_modal(true);
                        w.invoke_focus_input_modal();
                        return;
                    }
                }
                "new_file" => {
                    if let Some(w) = win_weak.upgrade() {
                        w.set_input_modal_mode("new_file".into());
                        w.set_input_modal_title("New File".into());
                        w.set_input_modal_value("untitled.txt".into());
                        w.set_show_input_modal(true);
                        w.invoke_focus_input_modal();
                        return;
                    }
                }
                "add_favorite" => {
                    let is_blank = win_weak.upgrade().is_some_and(|w| w.get_context_is_blank());
                    let sel_dir = if is_blank {
                        None
                    } else {
                        sel_meta.filter(|m| m.is_dir).map(|m| (m.path, m.name))
                    };
                    let (target_path, label) = sel_dir.unwrap_or_else(|| {
                        let p = guard.current_pane_path(is_right);
                        let l = guard.format_tab_title(&p);
                        (p, l)
                    });
                    guard.pin_folder_to_favorites(&target_path, &label);
                    guard.sync_selected_sidebar_to_active_dir();
                }
                "remove_favorite" => {
                    let is_blank = win_weak.upgrade().is_some_and(|w| w.get_context_is_blank());
                    let sel_dir = if is_blank {
                        None
                    } else {
                        sel_meta.filter(|m| m.is_dir).map(|m| m.path)
                    };
                    let target_path = sel_dir.unwrap_or_else(|| guard.current_pane_path(is_right));
                    guard.unpin_folder_from_favorites("", &target_path);
                    guard.sync_selected_sidebar_to_active_dir();
                }
                "show_view_options" => {
                    if let Some(w) = win_weak.upgrade() {
                        w.set_view_options_scope_menu_open(false);
                        w.set_view_options_sort_menu_open(false);
                        w.set_view_options_icon_menu_open(false);
                        w.set_view_options_font_menu_open(false);
                        w.set_view_options_text_menu_open(false);
                        w.set_show_view_options_popover(true);
                        sync_window_from_runtime(&w, &mut guard);
                        return;
                    }
                }
                "compress" if !sel_list.is_empty() => {
                    if let Some(w) = win_weak.upgrade() {
                        let count = sel_list.len();
                        let title = if count == 1 {
                            format!("Compress \"{}\"", sel_list[0].name)
                        } else {
                            format!("Compress {count} Selected Items")
                        };
                        let summary = if count == 1 {
                            let name = &sel_list[0].name;
                            format!("1 item selected ({name})")
                        } else {
                            format!("{count} items will be archived together")
                        };
                        let default_stem = default_compress_stem(&sel_list);
                        w.set_input_modal_mode("compress".into());
                        w.set_input_modal_title(title.into());
                        w.set_compress_items_summary(summary.into());
                        w.set_compress_format("tar.gz".into());
                        w.set_input_modal_value(default_stem.into());
                        w.set_show_input_modal(true);
                        w.invoke_focus_input_modal();
                        return;
                    }
                }
                "extract" => {
                    if let Some(meta) = sel_meta {
                        match extract_archive_to_folder(&meta) {
                            Ok(dest_dir) => {
                                let label = format!("Extracted {}", meta.name);
                                let sz = meta.size.unwrap_or(4096);
                                let log_msg = format!("[EXTRACT] -> {dest_dir}");
                                guard.logs.push(log_msg);
                                let toast = label.clone();
                                guard.start_tracked_task(&label, &dest_dir, sz, toast, "success");
                                guard.invalidate_dir_cache();
                                guard.reload_pane_entries(is_right);
                                guard.select_single_path_or_first(is_right, Some(&dest_dir));
                            }
                            Err(err) => {
                                let log_msg = format!("[EXTRACT_ERR] {err}");
                                guard.logs.push(log_msg);
                                guard.show_toast("error", format!("Extract failed: {err}"));
                            }
                        }
                    }
                }
                "duplicate" => {
                    let mut dup_count = 0usize;
                    let mut total_bytes = 0u64;
                    let mut first_name = String::new();
                    let mut last_dup_path = String::new();
                    let mut last_err = None;
                    for meta in &sel_list {
                        match guard.duplicate_entry(meta) {
                            Ok(dup_path) => {
                                if first_name.is_empty() {
                                    first_name = meta.name.clone();
                                }
                                last_dup_path = dup_path.clone();
                                total_bytes += meta.size.unwrap_or(4096);
                                guard.logs.push(format!("[DUPLICATE] -> {dup_path}"));
                                dup_count += 1;
                            }
                            Err(err) => last_err = Some(err),
                        }
                    }
                    if dup_count > 0 {
                        let cur_dir = guard.current_pane_path(is_right);
                        let label = if dup_count == 1 {
                            format!("Duplicated {first_name}")
                        } else {
                            format!("Duplicated {dup_count} items")
                        };
                        let toast = label.clone();
                        guard.start_tracked_task(&label, &cur_dir, total_bytes, toast, "success");
                        guard.invalidate_dir_cache();
                        guard.reload_pane_entries(is_right);
                        guard.select_single_path_or_first(is_right, Some(&last_dup_path));
                    } else if let Some(err) = last_err {
                        guard.show_toast("error", format!("Duplicate failed: {err}"));
                    }
                }
                "make_alias" => {
                    if let Some(meta) = sel_meta {
                        let alias_path = format!("{} alias", meta.path);
                        match guard.vfs_create_symlink(&meta.path, &alias_path) {
                            Ok(()) => {
                                guard.logs.push(format!("[ALIAS] -> {alias_path}"));
                                let toast = format!("Created symlink for {}", meta.name);
                                guard.show_toast("success", toast);
                                guard.invalidate_dir_cache();
                                guard.reload_pane_entries(is_right);
                                guard.select_single_path_or_first(is_right, Some(&alias_path));
                            }
                            Err(err) => {
                                guard.logs.push(format!("[ALIAS_ERR] {err}"));
                                guard.show_toast("error", format!("Symlink failed: {err}"));
                            }
                        }
                    }
                }
                "reveal_in_finder" => {
                    let is_blank = win_weak.upgrade().is_some_and(|w| w.get_context_is_blank());
                    if !is_blank {
                        if let Some(meta) = sel_meta {
                            reveal_in_system_file_manager(&meta.path);
                        } else {
                            let cur_dir = guard.current_pane_path(is_right);
                            reveal_in_system_file_manager(&cur_dir);
                        }
                    } else {
                        let cur_dir = guard.current_pane_path(is_right);
                        reveal_in_system_file_manager(&cur_dir);
                    }
                }
                "cut" if !sel_list.is_empty() => {
                    let count = sel_list.len();
                    let first_name = sel_list[0].name.clone();
                    guard.clipboard_paths = sel_list.into_iter().map(|m| m.path).collect();
                    guard.clipboard_cut = true;
                    let toast = if count == 1 {
                        format!("Cut \"{first_name}\" to clipboard")
                    } else {
                        format!("Cut {count} items to clipboard")
                    };
                    guard.logs.push(format!("[CLIPBOARD] {toast}"));
                    guard.show_toast("cut", toast);
                }
                "copy" if !sel_list.is_empty() => {
                    let count = sel_list.len();
                    let first_name = sel_list[0].name.clone();
                    guard.clipboard_paths = sel_list.into_iter().map(|m| m.path).collect();
                    guard.clipboard_cut = false;
                    copy_files_to_os_clipboard(&guard.clipboard_paths);
                    let toast = if count == 1 {
                        format!("Copied \"{first_name}\" to clipboard")
                    } else {
                        format!("Copied {count} items to clipboard")
                    };
                    guard.logs.push(format!("[CLIPBOARD] {toast}"));
                    guard.show_toast("copy", toast);
                }
                "paste" => {
                    let dst_dir = guard.current_pane_path(is_right);
                    guard.paste_clipboard_into(&dst_dir);
                }
                "copy_path" => {
                    let is_blank = win_weak.upgrade().is_some_and(|w| w.get_context_is_blank());
                    if is_blank || sel_list.is_empty() {
                        let cur_dir = guard.current_pane_path(is_right);
                        copy_string_to_system_clipboard(&cur_dir);
                        let toast = format!("Copied path: {cur_dir}");
                        guard.logs.push(format!("[PATH] {toast}"));
                        guard.show_toast("copy", toast);
                    } else {
                        let joined = sel_list
                            .iter()
                            .map(|m| m.path.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                        copy_string_to_system_clipboard(&joined);
                        let toast = if sel_list.len() == 1 {
                            format!("Copied path: {}", sel_list[0].path)
                        } else {
                            format!("Copied {} paths to clipboard", sel_list.len())
                        };
                        guard.logs.push(format!("[PATH] {toast}"));
                        guard.show_toast("copy", toast);
                    }
                }
                _ => {}
            }

            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Toolbar New Folder, New File & Rename Modal triggers
    {
        let win_weak = window.as_weak();
        window.on_open_new_folder_modal(move || {
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
                w.set_input_modal_mode("new_folder".into());
                w.set_input_modal_title("New Folder".into());
                w.set_input_modal_value("Untitled Folder".into());
                w.set_show_input_modal(true);
                w.invoke_focus_input_modal();
            }
        });
    }
    {
        let win_weak = window.as_weak();
        window.on_open_new_file_modal(move || {
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
                w.set_input_modal_mode("new_file".into());
                w.set_input_modal_title("New File".into());
                w.set_input_modal_value("untitled.txt".into());
                w.set_show_input_modal(true);
                w.invoke_focus_input_modal();
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_rename_modal(move || {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                open_rename_or_batch_modal(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_confirm_input_modal(move |val| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            reset_slint_modifiers(&w);
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return;
            }
            let mut guard = rt.borrow_mut();
            guard.pending_rename_click = None;
            guard.last_row_click = None;
            let is_right = guard.is_right_effective();
            let mode = w.get_input_modal_mode().to_string();
            if mode == "new_folder" {
                let cur_dir = guard.current_pane_path(is_right);
                let new_dir = join_vfs_path(&cur_dir, trimmed);
                match guard.vfs_create_dir(&new_dir) {
                    Ok(()) => {
                        guard.logs.push(format!("[MKDIR] Created {new_dir}"));
                        let toast = format!("Created folder \"{trimmed}\"");
                        guard.show_toast("success", toast);
                        guard.reload_pane_entries(is_right);
                        guard.select_single_path_or_first(is_right, Some(&new_dir));
                    }
                    Err(err) => guard.show_toast("error", format!("Create folder failed: {err}")),
                }
            } else if mode == "new_file" {
                let cur_dir = guard.current_pane_path(is_right);
                let new_file = join_vfs_path(&cur_dir, trimmed);
                match guard.vfs_write_bytes(&new_file, b"") {
                    Ok(()) => {
                        guard.logs.push(format!("[TOUCH] Created {new_file}"));
                        let toast = format!("Created file \"{trimmed}\"");
                        guard.show_toast("success", toast);
                        guard.reload_pane_entries(is_right);
                        guard.select_single_path_or_first(is_right, Some(&new_file));
                    }
                    Err(err) => guard.show_toast("error", format!("Create file failed: {err}")),
                }
            } else if mode == "rename" {
                let Some(meta) = guard.selected_metadata().cloned() else {
                    return;
                };
                let parent = vfs_parent_dir(&meta.path).unwrap_or_else(|| "/".to_string());
                let dst = join_vfs_path(&parent, trimmed);
                match guard.vfs_rename(&meta.path, &dst) {
                    Ok(()) => {
                        let msg = format!("[RENAME] {} -> {trimmed}", meta.name);
                        guard.logs.push(msg);
                        let toast = format!("Renamed to \"{trimmed}\"");
                        guard.show_toast("success", toast);
                        guard.reload_pane_entries(false);
                        guard.reload_pane_entries(true);
                        guard.select_single_path_or_first(is_right, Some(&dst));
                    }
                    Err(err) => guard.show_toast("error", format!("Rename failed: {err}")),
                }
            } else if mode == "compress" {
                let fmt = w.get_compress_format().to_string();
                let sel_list = guard.selected_metadata_list(is_right);
                let total_bytes: u64 = sel_list.iter().map(|m| m.size.unwrap_or(4096)).sum();
                match create_archive_from_selection(&sel_list, trimmed, &fmt) {
                    Ok(archive_path) => {
                        let out_name = Path::new(&archive_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| archive_path.clone());
                        let label = format!("Compressed {out_name}");
                        let log_msg = format!("[COMPRESS] Created {archive_path}");
                        guard.logs.push(log_msg);
                        let sz = total_bytes.max(4096);
                        guard.start_tracked_task(&label, &archive_path, sz, label.clone(), "info");
                        guard.invalidate_dir_cache();
                        guard.reload_pane_entries(is_right);
                        guard.select_single_path_or_first(is_right, Some(&archive_path));
                    }
                    Err(err) => {
                        let log_msg = format!("[COMPRESS_ERR] {err}");
                        guard.logs.push(log_msg);
                        guard.show_toast("error", format!("Compress failed: {err}"));
                    }
                }
            } else if mode == "rename_sidebar" {
                let u_idx = guard.sidebar_context_idx;
                if let Some((item_id, _)) = guard.sidebar_targets.get(u_idx).cloned() {
                    for sec in &mut guard.app_state.sidebar_sections {
                        for item in &mut sec.items {
                            if item.id == item_id {
                                item.label = trimmed.to_string();
                            }
                            for child in &mut item.children {
                                if child.id == item_id {
                                    child.label = trimmed.to_string();
                                }
                            }
                        }
                    }
                    guard.prefs.set_sidebar_label(&item_id, trimmed);
                    for fav in &mut guard.prefs.custom_favorites {
                        if fav.id == item_id {
                            fav.label = trimmed.to_string();
                        }
                    }
                    let _ = guard.prefs.save_default();
                    if let Some(prof) = guard
                        .saved_connections
                        .connections
                        .iter_mut()
                        .find(|p| p.id == item_id)
                    {
                        prof.name = trimmed.to_string();
                        let _ = guard.saved_connections.save_default();
                    }
                    let toast = format!("Updated label to \"{trimmed}\"");
                    guard.logs.push(format!("[SIDEBAR] {toast}"));
                    guard.show_toast("success", toast);
                }
            }
            sync_window_from_runtime(&w, &mut guard);
        });
    }

    // Open With Application Picker confirmation
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_confirm_open_with(move |app_spec| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let app_str = app_spec.to_string();
            let trimmed = app_str.trim();
            if trimmed.is_empty() {
                return;
            }
            let target = w.get_open_with_target_path().to_string();
            let mut guard = rt.borrow_mut();
            let local_target = if split_remote_uri(&target).is_some() {
                let mut staged = None;
                if let Some(meta) = guard.selected_metadata().cloned() {
                    staged = guard.stage_remote_file_for_edit(&meta);
                }
                staged.unwrap_or_else(|| target.clone())
            } else {
                target.clone()
            };
            match launch_file_with_app(&local_target, trimmed) {
                Ok(app_label) => {
                    let log = format!("[OPEN_WITH] Opened {local_target} with {app_label}");
                    guard.logs.push(log);
                    let toast = format!("Opened with {app_label}");
                    guard.show_toast("info", toast);
                }
                Err(err) => {
                    let log = format!("[OPEN_WITH] Failed ({trimmed}): {err}");
                    guard.logs.push(log);
                    guard.show_toast("error", err);
                }
            }
            sync_window_from_runtime(&w, &mut guard);
        });
    }

    // Toolbar Trash / Delete selected items & Permanent Delete Confirmation Modal
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_delete_selected_item(move || {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                prompt_delete_confirmation(&w, &mut guard, false);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_permanent_delete_selected_item(move || {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                prompt_delete_confirmation(&w, &mut guard, true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_confirm_delete_modal(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let permanent = guard.pending_delete_permanent;
            let sel_list = guard.selected_metadata_list(is_right);
            if sel_list.is_empty() {
                return;
            }
            let cur_dir = guard.current_pane_path(is_right);
            let mut deleted_count = 0usize;
            let mut total_bytes = 0u64;
            let mut last_err: Option<String> = None;
            let first_name = sel_list[0].name.clone();

            for meta in &sel_list {
                total_bytes += meta.size.unwrap_or(4096);
                if permanent {
                    match guard.vfs_remove(&meta.path, true) {
                        Ok(()) => {
                            let msg = format!("[DELETE] Permanently removed {}", meta.name);
                            guard.logs.push(msg);
                            deleted_count += 1;
                        }
                        Err(err) => last_err = Some(err),
                    }
                } else {
                    let res = if split_remote_uri(&meta.path).is_some() {
                        guard
                            .vfs_remove(&meta.path, true)
                            .map(|()| "Removed on remote".to_string())
                    } else {
                        move_path_to_trash(&guard.local_vfs, &meta.path)
                    };
                    match res {
                        Ok(dst) => {
                            let msg = format!("[TRASH] {} -> {dst}", meta.name);
                            guard.logs.push(msg);
                            deleted_count += 1;
                        }
                        Err(err) => last_err = Some(err),
                    }
                }
            }

            if deleted_count > 0 {
                let label = if permanent {
                    if deleted_count == 1 {
                        format!("Deleted \"{first_name}\"")
                    } else {
                        format!("Deleted {deleted_count} items")
                    }
                } else if deleted_count == 1 {
                    format!("Moved \"{first_name}\" to Trash")
                } else {
                    format!("Moved {deleted_count} items to Trash")
                };
                let dst_label = if permanent { "Removed" } else { "Trash" };
                guard.start_tracked_task(&label, dst_label, total_bytes, label.clone(), "delete");
                let _ = cur_dir;
                guard.invalidate_dir_cache();
                guard.reload_pane_entries(false);
                guard.reload_pane_entries(true);
                guard.select_single_path_or_first(is_right, None);
            }
            if let Some(err) = last_err {
                guard.logs.push(format!("[DELETE_ERR] {err}"));
                guard.show_toast("error", err);
            }
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_clear_completed_tasks(move || {
            let mut guard = rt.borrow_mut();
            guard.transfers.retain(|t| {
                matches!(
                    t.status,
                    TransferStatus::InProgress | TransferStatus::Paused | TransferStatus::Queued
                )
            });
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_pause_transfer(move |task_id| {
            let mut guard = rt.borrow_mut();
            guard.toggle_pause_transfer(task_id.as_str());
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_cancel_transfer(move |task_id| {
            let mut guard = rt.borrow_mut();
            guard.cancel_transfer(task_id.as_str());
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Universal Drag-and-Drop (Files & Directories -> Folders, Panes, Tabs, Breadcrumbs, Sidebar)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_update_drag_cursor(move |from_right, row_idx, abs_x, abs_y| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let mut guard = rt.borrow_mut();
            if guard.native_os_drag_handed_off || !is_primary_mouse_button_pressed() {
                drop(guard);
                clear_window_drag_overlay(&w);
                schedule_async_pointer_release(w.as_weak(), abs_x, abs_y);
                return;
            }
            let items = guard.drag_items_for_row(from_right, row_idx.max(0) as usize);
            if items.is_empty() {
                return;
            }
            guard.sidebar_scroll_y = w.get_drag_sidebar_scroll_y();
            let coords = (
                abs_x,
                abs_y,
                w.get_drag_left_pane_w(),
                w.get_drag_left_scroll_y(),
                w.get_drag_right_scroll_y(),
            );
            let bounds = window_logical_bounds(&w);
            let is_mac = cfg!(target_os = "macos");
            let res = guard.resolve_drop_target_for_os(from_right, &items, coords, bounds, is_mac);
            if res.dst_dir.as_deref() == Some("__EXTERNAL_OS_DROP__") {
                let local_paths = guard.prepare_local_paths_for_external_drag(&items);
                copy_files_to_os_clipboard(&local_paths);
                if start_native_os_file_drag(&local_paths) {
                    guard.native_os_drag_handed_off = true;
                    let msg = format!(
                        "[DRAG-OUT] Handed off {} item(s) to native OS drag session",
                        local_paths.len()
                    );
                    guard.logs.push(msg);
                    drop(guard);
                    clear_window_drag_overlay(&w);
                    schedule_async_pointer_release(w.as_weak(), abs_x, abs_y);
                    return;
                }
            }
            let ghost_label = if items.len() == 1 {
                items[0].name.clone()
            } else {
                format!("{} items", items.len())
            };
            w.set_is_dragging_row(true);
            w.set_drag_ghost_x(abs_x);
            w.set_drag_ghost_y(abs_y);
            w.set_drag_ghost_label(ghost_label.into());
            w.set_drag_target_hint(res.dst_label.into());
            w.set_drag_hover_sidebar_idx(res.hover_sidebar_idx);
            w.set_drag_hover_left_tab(res.hover_left_tab);
            w.set_drag_hover_right_tab(res.hover_right_tab);
            w.set_drag_hover_left_row(res.hover_left_row);
            w.set_drag_hover_right_row(res.hover_right_row);
            w.set_drag_hover_left_pane(res.hover_left_pane);
            w.set_drag_hover_right_pane(res.hover_right_pane);
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_finish_drag_drop(move |from_right, row_idx, abs_x, abs_y| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            clear_window_drag_overlay(&w);

            let mut guard = rt.borrow_mut();
            if guard.native_os_drag_handed_off {
                guard.native_os_drag_handed_off = false;
                return;
            }
            let items = guard.drag_items_for_row(from_right, row_idx.max(0) as usize);
            if items.is_empty() {
                return;
            }
            guard.sidebar_scroll_y = w.get_drag_sidebar_scroll_y();
            let coords = (
                abs_x,
                abs_y,
                w.get_drag_left_pane_w(),
                w.get_drag_left_scroll_y(),
                w.get_drag_right_scroll_y(),
            );
            let bounds = window_logical_bounds(&w);
            let is_mac = cfg!(target_os = "macos");
            let res = guard.resolve_drop_target_for_os(from_right, &items, coords, bounds, is_mac);
            if let Some(dst_dir) = res.dst_dir {
                guard.execute_drop_target(&items, &dst_dir, res.is_move);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Drag-and-Drop row(s) across the Dual-Pane boundary or onto any Tab Pill
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_drag_drop_row_to_other_pane(move |from_right, row_idx| {
            let mut guard = rt.borrow_mut();
            if !guard.app_state.dual_pane_enabled {
                return;
            }
            let items = guard.drag_items_for_row(from_right, row_idx.max(0) as usize);
            if items.is_empty() {
                return;
            }
            let dst_dir = guard.current_pane_path(!from_right);
            guard.execute_drop_target(&items, &dst_dir, false);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_drop_row_on_tab(
            move |from_right, row_idx, target_is_right, target_tab_idx| {
                let mut guard = rt.borrow_mut();
                let items = guard.drag_items_for_row(from_right, row_idx.max(0) as usize);
                if items.is_empty() {
                    return;
                }
                let t_idx = target_tab_idx as usize;
                let dst_dir_opt = if target_is_right {
                    guard
                        .app_state
                        .right_tabs
                        .get(t_idx)
                        .map(|t| t.current_path.clone())
                } else {
                    guard
                        .app_state
                        .left_tabs
                        .get(t_idx)
                        .map(|t| t.current_path.clone())
                };
                let Some(dst_dir) = dst_dir_opt else {
                    return;
                };
                guard.execute_drop_target(&items, &dst_dir, false);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            },
        );
    }

    // Miller Columns: click parent row to switch sibling folder
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_parent_row(move |is_right, row_idx| {
            let mut guard = rt.borrow_mut();
            let idx = row_idx as usize;
            let entry = if is_right {
                guard.right_parent_entries.get(idx).cloned()
            } else {
                guard.left_parent_entries.get(idx).cloned()
            };
            if let Some(meta) = entry.filter(|m| m.is_dir) {
                let use_right = is_right && guard.app_state.dual_pane_enabled;
                guard.app_state.active_pane = if use_right {
                    ActivePane::Right
                } else {
                    ActivePane::Left
                };
                guard.navigate_to_path(use_right, meta.path);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Navigate up to parent directory
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_navigate_up(move |is_right| {
            let mut guard = rt.borrow_mut();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let cur = guard.current_pane_path(use_right);
            let parent = vfs_parent_dir(&cur).unwrap_or_else(|| "/".to_string());
            guard.navigate_to_path(use_right, parent);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Column sorting (Name | Size | Kind | Date Modified | Date Added | Date Created | Permissions)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_sort_by_column(move |is_right, col_str| {
            let mut guard = rt.borrow_mut();
            let target_col = str_to_sort_col(col_str.as_str());
            let tab_opt = if is_right {
                let idx = guard.app_state.right_active_tab;
                guard.app_state.right_tabs.get_mut(idx)
            } else {
                let idx = guard.app_state.left_active_tab;
                guard.app_state.left_tabs.get_mut(idx)
            };
            if let Some(tab) = tab_opt {
                if tab.sort_column == target_col {
                    tab.sort_direction = match tab.sort_direction {
                        SortDirection::Ascending => SortDirection::Descending,
                        SortDirection::Descending => SortDirection::Ascending,
                    };
                } else {
                    tab.sort_column = target_col;
                    tab.sort_direction = SortDirection::Ascending;
                }
            }
            guard.reload_pane_entries(is_right);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Per-Tab Search / Filter input box
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_filter_changed(move |query| {
            let mut guard = rt.borrow_mut();
            let q_str = query.to_string();
            guard.app_state.toolbar.search_query = q_str.clone();
            let is_right = guard.is_right_effective();
            if is_right {
                let idx = guard.app_state.right_active_tab;
                if let Some(tab) = guard.app_state.right_tabs.get_mut(idx) {
                    tab.inline_filter = q_str;
                }
            } else {
                let idx = guard.app_state.left_active_tab;
                if let Some(tab) = guard.app_state.left_tabs.get_mut(idx) {
                    tab.inline_filter = q_str;
                }
            }
            guard.reload_pane_entries(is_right);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_clear_pane_filter(move |is_right| {
            let mut guard = rt.borrow_mut();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.cancel_pane_search(use_right);
            guard.app_state.toolbar.search_query.clear();
            if use_right {
                let idx = guard.app_state.right_active_tab;
                if let Some(tab) = guard.app_state.right_tabs.get_mut(idx) {
                    tab.inline_filter.clear();
                }
            } else {
                let idx = guard.app_state.left_active_tab;
                if let Some(tab) = guard.app_state.left_tabs.get_mut(idx) {
                    tab.inline_filter.clear();
                }
            }
            guard.reload_pane_entries(use_right);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // View mode switcher (List, Icons, Columns)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_view_mode(move |mode_int| {
            let mut guard = rt.borrow_mut();
            let mode = int_to_view_mode(mode_int);
            let is_right = guard.is_right_effective();
            if is_right {
                let idx = guard.app_state.right_active_tab;
                if let Some(tab) = guard.app_state.right_tabs.get_mut(idx) {
                    tab.view_mode = mode;
                }
            } else {
                let idx = guard.app_state.left_active_tab;
                if let Some(tab) = guard.app_state.left_tabs.get_mut(idx) {
                    tab.view_mode = mode;
                }
            }
            guard.reload_pane_entries(is_right);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Toggle hidden files
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_hidden_files(move || {
            let mut guard = rt.borrow_mut();
            let prev = guard.app_state.toolbar.show_hidden_files;
            guard.app_state.toolbar.show_hidden_files = !prev;
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Toggle Right Inspector Drawer
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_inspector(move || {
            let mut guard = rt.borrow_mut();
            guard.app_state.inspector_open = !guard.app_state.inspector_open;
            if guard.app_state.inspector_open {
                guard.refresh_headers_and_inspector();
            }
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Switch Inspector Tab (Info | Activities | Logs)
    {
        let win_weak = window.as_weak();
        window.on_set_inspector_tab(move |tab_idx| {
            if let Some(w) = win_weak.upgrade() {
                w.set_inspector_tab_index(tab_idx);
            }
        });
    }

    // Interactive 3x3 R/W/X Permissions Matrix toggle (preserving setuid/setgid/sticky bits)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_permission_bit(move |row, col| {
            let mut guard = rt.borrow_mut();
            if let Some(meta) = guard.selected_metadata().cloned() {
                let mut matrix = PermissionsMatrix::from_mode(meta.permissions);
                let triad = match row {
                    0 => &mut matrix.owner,
                    1 => &mut matrix.group,
                    _ => &mut matrix.others,
                };
                match col {
                    0 => triad.read = !triad.read,
                    1 => triad.write = !triad.write,
                    _ => triad.execute = !triad.execute,
                }
                let new_mode = matrix
                    .with_preserved_special_bits(meta.permissions)
                    .to_mode();
                if let Err(err) = guard.vfs_set_permissions(&meta.path, new_mode) {
                    guard.show_toast("error", format!("Permissions update failed: {err}"));
                    if let Some(w) = win_weak.upgrade() {
                        sync_window_from_runtime(&w, &mut guard);
                    }
                    return;
                }
                let sym = matrix.to_symbolic_string();
                let oct = matrix.to_octal_string();
                guard
                    .logs
                    .push(format!("[PERM] {} -> {sym} ({oct})", meta.name));
                let is_right = guard.is_right_effective();
                guard.invalidate_dir_cache();
                guard.reload_pane_entries(is_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // 8-Swatch Color Label update on selected item(s)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_selected_color_label(move |swatch_idx| {
            let mut guard = rt.borrow_mut();
            let label = index_to_color_label(swatch_idx);
            let is_right = guard.is_right_effective();
            let mut selected_list = guard.selected_metadata_list(is_right);
            if selected_list.is_empty() {
                if let Some(meta) = guard.selected_metadata().cloned() {
                    selected_list.push(meta);
                }
            }
            if !selected_list.is_empty() {
                let mut matched_sidebar_ids = Vec::new();
                for meta in &selected_list {
                    let _ = guard.local_vfs.set_color_label(&meta.path, label);
                    guard.prefs.set_file_color_label(&meta.path, label);
                    if meta.is_dir {
                        let norm_meta = normalize_fav_target(&meta.path);
                        for (sb_id, sb_target) in &guard.sidebar_targets {
                            if normalize_fav_target(sb_target) == norm_meta {
                                matched_sidebar_ids.push(sb_id.clone());
                            }
                        }
                    }
                }
                for sb_id in matched_sidebar_ids {
                    if label == ColorLabel::None {
                        guard.sidebar_colors.remove(&sb_id);
                    } else {
                        guard.sidebar_colors.insert(sb_id.clone(), label);
                    }
                    guard.prefs.set_sidebar_color(&sb_id, label);
                }
                let _ = guard.prefs.save_default();
                let tag_name = color_label_to_name(label);
                let toast_msg = if label == ColorLabel::None {
                    if selected_list.len() == 1 {
                        format!("Cleared color tag on \"{}\"", selected_list[0].name)
                    } else {
                        format!("Cleared color tags on {} items", selected_list.len())
                    }
                } else if selected_list.len() == 1 {
                    format!("Tagged \"{}\" as {tag_name}", selected_list[0].name)
                } else {
                    format!("Tagged {} items as {tag_name}", selected_list.len())
                };
                guard.show_toast("info", toast_msg);
                guard.invalidate_dir_cache();
                guard.reload_pane_entries(is_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Hide extension toggle
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_hide_extension(move || {
            let mut guard = rt.borrow_mut();
            if let Some(meta) = guard.selected_metadata().cloned() {
                let next = !meta.hide_extension;
                let _ = guard.local_vfs.set_hide_extension(&meta.path, next);
                let is_right = guard.is_right_effective();
                guard.invalidate_dir_cache();
                guard.reload_pane_entries(is_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Locked attribute toggle
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_locked(move || {
            let mut guard = rt.borrow_mut();
            if let Some(meta) = guard.selected_metadata().cloned() {
                let next = !meta.is_locked;
                let _ = guard.local_vfs.set_locked(&meta.path, next);
                let is_right = guard.is_right_effective();
                guard.invalidate_dir_cache();
                guard.reload_pane_entries(is_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Calculate recursive directory size ("--" -> "4 KB") asynchronously
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_calculate_selected_dir_size(move || {
            let mut guard = rt.borrow_mut();
            if let Some(meta) = guard.selected_metadata().cloned() {
                let cached_sz = guard.dir_size_cache.borrow().get(&meta.path).copied();
                if let Some(cached) = cached_sz {
                    let formatted = manifold_vfs::format_bytes_comma(cached);
                    if let Some(vm) = &mut guard.app_state.inspector_vm {
                        vm.general.size = formatted.clone();
                        vm.subtitle = format!("{} - {}", meta.kind, formatted);
                    }
                } else {
                    guard.request_bg_dir_size(&meta.path);
                    if let Some(vm) = &mut guard.app_state.inspector_vm {
                        vm.general.size = "Calculating…".to_string();
                    }
                }
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }

    // Copy selected item(s) between panes or Duplicate in place
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_copy_selected_between_panes(move || {
            let mut guard = rt.borrow_mut();
            let from_right = guard.is_right_effective();
            let sel_list = guard.selected_metadata_list(from_right);
            let Some(src_meta) = sel_list.first().cloned() else {
                return;
            };
            if !guard.app_state.dual_pane_enabled {
                let mut dup_count = 0usize;
                let mut total_bytes = 0u64;
                for meta in &sel_list {
                    if let Ok(dup_path) = guard.duplicate_entry(meta) {
                        total_bytes += meta.size.unwrap_or(4096);
                        guard.logs.push(format!("[DUPLICATE] -> {dup_path}"));
                        dup_count += 1;
                    }
                }
                if dup_count > 0 {
                    let cur_dir = guard.current_pane_path(false);
                    let label = if dup_count == 1 {
                        format!("Duplicated {}", src_meta.name)
                    } else {
                        format!("Duplicated {dup_count} items")
                    };
                    let toast = label.clone();
                    guard.start_tracked_task(&label, &cur_dir, total_bytes, toast, "success");
                    guard.invalidate_dir_cache();
                    guard.reload_pane_entries(false);
                    if let Some(w) = win_weak.upgrade() {
                        sync_window_from_runtime(&w, &mut guard);
                    }
                }
                return;
            }

            let dst_dir = guard.current_pane_path(!from_right);
            guard.execute_drop_target(&sel_list, &dst_dir, false);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Move selected item(s) to opposite pane (`F6`, Section 2.3 & Section 3)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_move_selected_between_panes(move || {
            let mut guard = rt.borrow_mut();
            if !guard.app_state.dual_pane_enabled {
                return;
            }
            let from_right = guard.is_right_effective();
            let sel_list = guard.selected_metadata_list(from_right);
            if sel_list.is_empty() {
                return;
            }
            let dst_dir = guard.current_pane_path(!from_right);
            guard.execute_drop_target(&sel_list, &dst_dir, true);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Multi-Rename Rule Pipeline & Preset Favorites callbacks
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_save_batch_rename_favorite(move || {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                save_batch_rename_favorite_from_modal(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_apply_batch_rename_favorite(move |fav_idx| {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                apply_batch_rename_favorite_to_modal(&w, &mut guard, fav_idx.max(0) as usize);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_delete_batch_rename_favorite(move |fav_idx| {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                delete_batch_rename_favorite_from_modal(&w, &mut guard, fav_idx.max(0) as usize);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_add_batch_rename_rule(move |after_idx| {
            let mut guard = rt.borrow_mut();
            let insert_at = ((after_idx + 1).max(0) as usize).min(guard.batch_rename_rules.len());
            guard
                .batch_rename_rules
                .insert(insert_at, default_batch_rename_rule());
            if let Some(w) = win_weak.upgrade() {
                sync_batch_rename_modal(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_remove_batch_rename_rule(move |rule_idx| {
            let mut guard = rt.borrow_mut();
            let idx = rule_idx.max(0) as usize;
            if guard.batch_rename_rules.len() > 1 && idx < guard.batch_rename_rules.len() {
                guard.batch_rename_rules.remove(idx);
            } else if guard.batch_rename_rules.len() == 1 {
                guard.batch_rename_rules[0] = default_batch_rename_rule();
            }
            if let Some(w) = win_weak.upgrade() {
                sync_batch_rename_modal(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_update_batch_rename_rule(move |rule_idx, field, value| {
            let mut guard = rt.borrow_mut();
            let idx = rule_idx.max(0) as usize;
            let is_text_field = matches!(
                field.as_str(),
                "find_text"
                    | "replace_text"
                    | "seq_prefix"
                    | "seq_start"
                    | "seq_step"
                    | "seq_padding"
            );
            if let Some(rule) = guard.batch_rename_rules.get_mut(idx) {
                match field.as_str() {
                    "operation" => rule.operation = value,
                    "apply_to" => rule.apply_to = value,
                    "occurrences" => rule.occurrences = value,
                    "find_text" => rule.find_text = value,
                    "replace_text" => rule.replace_text = value,
                    "case_style" => rule.case_style = value,
                    "position" => rule.position = value,
                    "seq_prefix" => rule.seq_prefix = value,
                    "seq_start" => rule.seq_start = value,
                    "seq_step" => rule.seq_step = value,
                    "seq_padding" => rule.seq_padding = value,
                    "date_format" => rule.date_format = value,
                    _ => {}
                }
            }
            if let Some(w) = win_weak.upgrade() {
                if is_text_field {
                    sync_batch_rename_preview_only(&w, &mut guard);
                } else {
                    sync_batch_rename_rule_at(&w, &mut guard, idx);
                }
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_batch_rename_case_sensitive(move |rule_idx| {
            let mut guard = rt.borrow_mut();
            let idx = rule_idx.max(0) as usize;
            if let Some(rule) = guard.batch_rename_rules.get_mut(idx) {
                rule.case_sensitive = !rule.case_sensitive;
            }
            if let Some(w) = win_weak.upgrade() {
                sync_batch_rename_rule_at(&w, &mut guard, idx);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_reset_batch_rename_rules(move || {
            let mut guard = rt.borrow_mut();
            guard.batch_rename_rules = vec![default_batch_rename_rule()];
            if let Some(w) = win_weak.upgrade() {
                sync_batch_rename_modal(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_confirm_batch_rename_modal(move || {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                execute_batch_rename_from_modal(&w, &mut guard);
            }
        });
    }

    // Resolve Conflict Modal action
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_resolve_conflict(move |action_str| {
            let mut guard = rt.borrow_mut();
            let policy = match action_str.as_str() {
                "replace" => ConflictResolution::Replace,
                "replace_older" => ConflictResolution::ReplaceOlder,
                "keep_both" => ConflictResolution::KeepBoth,
                "skip" => ConflictResolution::Skip,
                _ => ConflictResolution::Stop,
            };
            let apply_all = win_weak
                .upgrade()
                .is_some_and(|w| w.get_conflict_apply_all());
            guard.resolve_batch_conflict(policy, apply_all);
            if let Some(w) = win_weak.upgrade() {
                if guard.active_conflict().is_none() {
                    w.set_conflict_apply_all(false);
                }
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Refresh Active / Dual Panes (decoupled from Directory Sync Modal)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_sync_panes(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_dir = guard.current_pane_path(is_right);
            let title = guard.format_tab_title(&cur_dir);
            let label = format!("Refreshed {title}");
            guard.start_tracked_task(&label, &cur_dir, 4096, label.clone(), "info");
            guard.invalidate_dir_cache();
            guard.reload_pane_entries(false);
            if guard.app_state.dual_pane_enabled {
                guard.reload_pane_entries(true);
            }
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_sync_modal(move || {
            let mut guard = rt.borrow_mut();
            if !guard.app_state.dual_pane_enabled {
                guard.app_state.dual_pane_enabled = true;
                guard.app_state.panes_layout = PanesLayout::SplitHorizontal;
            }
            if let Some(w) = win_weak.upgrade() {
                let strat = w.get_sync_strategy().to_string();
                let mode = w.get_sync_compare_mode().to_string();
                w.set_show_sync_modal(true);
                guard.spawn_bg_sync_plan(&strat, &mode);
                sync_sync_modal_to_window(&w, &guard);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_update_sync_options(move |strat, mode| {
            let mut guard = rt.borrow_mut();
            if let Some(w) = win_weak.upgrade() {
                w.set_sync_strategy(strat.clone());
                w.set_sync_compare_mode(mode.clone());
                guard.spawn_bg_sync_plan(strat.as_str(), mode.as_str());
                sync_sync_modal_to_window(&w, &guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_sync_row_approved(move |row_idx| {
            let mut guard = rt.borrow_mut();
            let idx = row_idx.max(0) as usize;
            if let Some(plan) = &mut guard.active_sync_plan {
                if let Some(entry) = plan.entries.get_mut(idx) {
                    entry.approved = !entry.approved;
                }
            }
            if let Some(w) = win_weak.upgrade() {
                sync_sync_modal_to_window(&w, &guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_execute_sync_plan(move || {
            let mut guard = rt.borrow_mut();
            guard.spawn_bg_sync_execute();
            if let Some(w) = win_weak.upgrade() {
                sync_sync_modal_to_window(&w, &guard);
                if !guard.sync_running {
                    w.set_show_sync_modal(false);
                }
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Preview Minion (Quick Look HUD) Toggle
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_preview_minion(move || {
            if let Some(w) = win_weak.upgrade() {
                let next = !w.get_show_preview_minion();
                w.set_preview_minion_zoom(1.0);
                w.set_show_preview_minion(next);
                if next {
                    let mut guard = rt.borrow_mut();
                    sync_inspector_to_window(&w, &mut guard);
                }
            }
        });
    }

    // Preferences & Themes Callbacks (persisted to ~/.config/manifold/preferences.toml)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_theme_preset(move |preset| {
            let mut guard = rt.borrow_mut();
            guard.prefs.theme_preset = preset.to_string();
            let _ = guard.prefs.save_default();
            let toast = format!("Theme set to \"{}\"", guard.prefs.theme_preset);
            guard.show_toast("info", toast);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_font_size_delta(move |delta| {
            let mut guard = rt.borrow_mut();
            let clamped = delta.clamp(-1, 5);
            guard.prefs.font_size_delta = clamped;
            let _ = guard.prefs.save_default();
            let toast = format!("Font size delta set to {clamped:+}");
            guard.show_toast("info", toast);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_font_family(move |family| {
            let mut guard = rt.borrow_mut();
            guard.prefs.font_family = family.to_string();
            let _ = guard.prefs.save_default();
            let toast = format!("Font family set to \"{}\"", guard.prefs.font_family);
            guard.show_toast("info", toast);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_font_bold(move || {
            let mut guard = rt.borrow_mut();
            guard.prefs.font_bold = !guard.prefs.font_bold;
            let _ = guard.prefs.save_default();
            let weight_lbl = if guard.prefs.font_bold {
                "Bold / High-Legibility"
            } else {
                "Regular Weight"
            };
            let toast = format!("Font style set to \"{weight_lbl}\"");
            guard.show_toast("info", toast);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_max_bandwidth_mbps(move |mbps| {
            let mut guard = rt.borrow_mut();
            let clamped = mbps.max(0) as u32;
            guard.prefs.max_bandwidth_mbps = clamped;
            if let Ok(mut lim) = guard.shared_limiter.lock() {
                *lim = TokenBucketLimiter::new(mbps_to_rate_bps(clamped));
            }
            let _ = guard.prefs.save_default();
            let toast = if clamped == 0 {
                "Transfer bandwidth set to Unlimited".to_string()
            } else {
                format!("Transfer bandwidth limited to {clamped} MB/s")
            };
            guard.show_toast("info", toast);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // View Options Popover Callbacks (persisted to ~/.config/manifold/preferences.toml)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_view_options_scope(move |scope_str| {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_dir = guard.current_pane_path(is_right);
            let target_scope = match scope_str.as_str() {
                "this_folder" => ViewOptionsScope::ThisFolder,
                "this_folder_and_subfolders" | "this_and_subfolders" => {
                    ViewOptionsScope::ThisFolderAndSubfolders
                }
                _ => ViewOptionsScope::AllFolders,
            };
            let mut vo = guard.prefs.effective_view_options(&cur_dir);
            vo.scope = target_scope;
            guard.prefs.save_view_options_for_path(&cur_dir, vo);
            let _ = guard.prefs.save_default();
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_view_options_sort(move |sort_str| {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_dir = guard.current_pane_path(is_right);
            let mut vo = guard.prefs.effective_view_options(&cur_dir);
            let target_col = str_to_sort_col(sort_str.as_str());
            vo.sort_column = target_col;
            if is_right {
                let idx = guard.app_state.right_active_tab;
                if let Some(tab) = guard.app_state.right_tabs.get_mut(idx) {
                    tab.sort_column = target_col;
                }
            } else {
                let idx = guard.app_state.left_active_tab;
                if let Some(tab) = guard.app_state.left_tabs.get_mut(idx) {
                    tab.sort_column = target_col;
                }
            }
            guard.prefs.save_view_options_for_path(&cur_dir, vo);
            let _ = guard.prefs.save_default();
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_set_view_options_icon_size(move |size_px| {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_dir = guard.current_pane_path(is_right);
            let mut vo = guard.prefs.effective_view_options(&cur_dir);
            vo.icon_size = (size_px.clamp(16, 32)) as u16;
            guard.prefs.save_view_options_for_path(&cur_dir, vo);
            let _ = guard.prefs.save_default();
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_toggle_view_options_option(move |opt_key| {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_dir = guard.current_pane_path(is_right);
            let mut vo = guard.prefs.effective_view_options(&cur_dir);
            match opt_key.as_str() {
                "col_size" => vo.col_size = !vo.col_size,
                "col_kind" => vo.col_kind = !vo.col_kind,
                "col_modified" => vo.col_date_modified = !vo.col_date_modified,
                "col_added" => vo.col_date_added = !vo.col_date_added,
                "col_created" => vo.col_date_created = !vo.col_date_created,
                "col_owner" => vo.col_owner = !vo.col_owner,
                "col_group" => vo.col_group = !vo.col_group,
                "col_permissions" => vo.col_permissions = !vo.col_permissions,
                "col_tags" => vo.col_tags = !vo.col_tags,
                "bold_folders" => vo.bold_folders = !vo.bold_folders,
                "show_hidden" => {
                    vo.show_hidden_files = !vo.show_hidden_files;
                    guard.app_state.toolbar.show_hidden_files = vo.show_hidden_files;
                }
                "show_icon_preview" | "icon_preview" => {
                    vo.show_icon_preview = !vo.show_icon_preview;
                }
                "treat_packages_as_folders" | "packages_as_folders" => {
                    vo.treat_packages_as_folders = !vo.treat_packages_as_folders;
                }
                "show_folders_on_top" | "folders_on_top" => {
                    vo.show_folders_on_top = !vo.show_folders_on_top;
                }
                "folders_always_alpha" | "folders_alpha" => {
                    vo.folders_always_alpha = !vo.folders_always_alpha;
                }
                "use_relative_dates" | "relative_dates" => {
                    vo.use_relative_dates = !vo.use_relative_dates;
                }
                "calculate_all_sizes" | "calc_all_sizes" => {
                    vo.calculate_all_sizes = !vo.calculate_all_sizes;
                }
                "use_alternating_bg" | "alt_bg" => {
                    vo.use_alternating_row_bg = !vo.use_alternating_row_bg;
                }
                _ => {}
            }
            guard.prefs.save_view_options_for_path(&cur_dir, vo);
            let _ = guard.prefs.save_default();
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_use_view_options_as_defaults(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_dir = guard.current_pane_path(is_right);
            let vo = guard.prefs.effective_view_options(&cur_dir);
            guard.prefs.use_view_options_as_defaults(&cur_dir, vo);
            let _ = guard.prefs.save_default();
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Embedded Bottom Terminal Drawer & External OS Terminal Callbacks
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_terminal(move || {
            let mut guard = rt.borrow_mut();
            guard.sync_terminal_cwd();
            let _ = guard.poll_pty_output();
            if let Some(w) = win_weak.upgrade() {
                let next = !w.get_show_terminal_drawer();
                w.set_show_terminal_drawer(next);
                sync_window_from_runtime(&w, &mut guard);
                if next {
                    w.invoke_focus_terminal_input();
                } else {
                    w.invoke_focus_main_key_router();
                }
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_clear_terminal(move || {
            let mut guard = rt.borrow_mut();
            guard.term_emulator.clear_scrollback_and_screen();
            if !guard.term_emulator.using_alt_screen() {
                if let Some(shell) = &guard.shell_session {
                    let _ = shell.send_bytes(&[0x0C]);
                }
            }
            let _ = guard.poll_pty_output();
            if let Some(w) = win_weak.upgrade() {
                sync_terminal_to_window(&w, &mut guard);
                w.invoke_focus_terminal_input();
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_restart_terminal(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cwd = guard.current_pane_path(is_right);
            guard.restart_terminal_in(&cwd);
            let _ = guard.poll_pty_output();
            if let Some(w) = win_weak.upgrade() {
                sync_terminal_to_window(&w, &mut guard);
                w.invoke_focus_terminal_input();
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_terminal_key_input(move |text, key_name, ctrl, alt, shift| {
            let mut guard = rt.borrow_mut();
            if shift
                && !guard.term_emulator.using_alt_screen()
                && (key_name == "page_up" || key_name == "page_down")
            {
                let delta = if key_name == "page_up" { 10 } else { -10 };
                guard.term_emulator.scroll_view_by(delta);
                if let Some(w) = win_weak.upgrade() {
                    sync_terminal_to_window(&w, &mut guard);
                }
                return;
            }
            let app_cursor = guard.term_emulator.app_cursor_keys();
            let encoded = encode_terminal_key_event(
                text.as_str(),
                key_name.as_str(),
                ctrl,
                alt,
                shift,
                app_cursor,
            );
            if let Some(bytes) = encoded {
                guard.term_emulator.reset_scroll_to_bottom();
                if let Some(shell) = &guard.shell_session {
                    let _ = shell.send_bytes(&bytes);
                }
                let _ = guard.poll_pty_output();
                if let Some(w) = win_weak.upgrade() {
                    sync_terminal_to_window(&w, &mut guard);
                }
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_terminal_paste(move || {
            let Some(clip_text) = read_string_from_system_clipboard() else {
                return;
            };
            if clip_text.is_empty() {
                return;
            }
            let mut guard = rt.borrow_mut();
            guard.term_emulator.reset_scroll_to_bottom();
            let normalized = crate::terminal::sanitize_bracketed_paste(&clip_text);
            let payload = if guard.term_emulator.bracketed_paste() {
                format!("\x1b[200~{normalized}\x1b[201~").into_bytes()
            } else {
                normalized.into_bytes()
            };
            if let Some(shell) = &guard.shell_session {
                let _ = shell.send_bytes(&payload);
            }
            let _ = guard.poll_pty_output();
            if let Some(w) = win_weak.upgrade() {
                sync_terminal_to_window(&w, &mut guard);
                w.invoke_focus_terminal_input();
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_sync_terminal_cwd(move || {
            let mut guard = rt.borrow_mut();
            guard.sync_terminal_cwd_explicit();
            let _ = guard.poll_pty_output();
            if let Some(w) = win_weak.upgrade() {
                sync_terminal_to_window(&w, &mut guard);
                w.invoke_focus_terminal_input();
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_terminal_scroll(move |delta_lines| {
            let mut guard = rt.borrow_mut();
            if guard.term_emulator.using_alt_screen() {
                let key = if delta_lines > 0 { "up" } else { "down" };
                let app_cur = guard.term_emulator.app_cursor_keys();
                if let Some(seq) = encode_terminal_key_event("", key, false, false, false, app_cur)
                {
                    if let Some(shell) = &guard.shell_session {
                        let count = delta_lines.unsigned_abs().clamp(1, 5);
                        for _ in 0..count {
                            let _ = shell.send_bytes(&seq);
                        }
                    }
                }
            } else {
                guard.term_emulator.scroll_view_by(delta_lines);
            }
            if let Some(w) = win_weak.upgrade() {
                sync_terminal_to_window(&w, &mut guard);
            }
        });
    }
    {
        let win_weak = window.as_weak();
        window.on_open_external_terminal(move || {
            if let Some(w) = win_weak.upgrade() {
                w.invoke_open_terminal_here();
            }
        });
    }

    // Open Integrated Bottom Terminal Here
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_terminal_here(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let sel_dir = guard
                .selected_metadata()
                .filter(|m| m.is_dir)
                .map(|m| m.path.clone())
                .unwrap_or_else(|| guard.current_pane_path(is_right));
            guard.open_terminal_at_path(&sel_dir);
            let _ = guard.poll_pty_output();
            let msg = format!("[TERM] Opened integrated bottom terminal at {sel_dir}");
            guard.logs.push(msg);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_terminal_drawer(true);
                sync_window_from_runtime(&w, &mut guard);
                w.invoke_focus_terminal_input();
            }
        });
    }

    // Connect Modal callbacks (doc/image3.png & media_1792955040883_8576b324.png)
    {
        let win_weak = window.as_weak();
        window.on_open_connect_dialog(move || {
            if let Some(w) = win_weak.upgrade() {
                reset_connect_modal_for_new(&w);
                w.set_show_connect_modal(true);
            }
        });
    }
    {
        let win_weak = window.as_weak();
        window.on_close_connect_dialog(move || {
            if let Some(w) = win_weak.upgrade() {
                w.set_connect_editing_id(SharedString::default());
                w.set_show_connect_modal(false);
            }
        });
    }
    {
        let win_weak = window.as_weak();
        window.on_select_connect_protocol(move |proto| {
            if let Some(w) = win_weak.upgrade() {
                let default_port = match proto.as_str() {
                    "S3" | "WebDAV" => "443",
                    "SMB" => "445",
                    "FTPS" => "990",
                    "FTP" => "21",
                    _ => "22",
                };
                w.set_connect_protocol(proto);
                w.set_connect_port(default_port.into());
            }
        });
    }
    {
        let win_weak = window.as_weak();
        window.on_select_connect_color(move |c_idx| {
            if let Some(w) = win_weak.upgrade() {
                w.set_connect_color_index(c_idx);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_confirm_connect_dialog(move || {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let mut guard = rt.borrow_mut();
            let editing_id = w.get_connect_editing_id().trim().to_string();
            let custom_name = w.get_connect_name().trim().to_string();
            let proto_str = w.get_connect_protocol().to_string();
            let server = w.get_connect_server().trim().to_string();
            let port_str = w.get_connect_port().trim().to_string();
            let user_raw = w.get_connect_username().trim().to_string();
            let s3_access = w.get_connect_s3_access_key().trim().to_string();
            let user = if proto_str == "S3" && !s3_access.is_empty() {
                s3_access
            } else {
                user_raw
            };
            let remote_path = w.get_connect_remote_path().trim().to_string();
            let local_path_raw = w.get_connect_local_path().trim().to_string();
            let base_url_raw = w.get_connect_base_url().trim().to_string();
            let encoding_raw = w.get_connect_encoding().trim().to_string();
            let note_raw = w.get_connect_note().trim().to_string();
            let show_adv = w.get_connect_show_advanced();
            let auth_method_str = w.get_connect_auth_method().to_string();
            let key_path_raw = w.get_connect_key_path().trim().to_string();
            let color = index_to_color_label(w.get_connect_color_index());

            let host_name = match proto_str.as_str() {
                "S3" => {
                    let bkt = w.get_connect_s3_bucket().trim().to_string();
                    if bkt.is_empty() {
                        "s3-bucket".to_string()
                    } else {
                        bkt
                    }
                }
                _ => {
                    if server.is_empty() {
                        "localhost".to_string()
                    } else {
                        server
                    }
                }
            };
            let base_path = match proto_str.as_str() {
                "SMB" => {
                    let share = w.get_connect_smb_share().to_string();
                    let s_trim = share.trim_matches('/');
                    let sub_trim = remote_path.trim_matches('/');
                    if s_trim.is_empty() {
                        "/".to_string()
                    } else if sub_trim.is_empty() {
                        format!("/{s_trim}")
                    } else {
                        format!("/{s_trim}/{sub_trim}")
                    }
                }
                _ => {
                    let trimmed = remote_path.trim_start_matches('/');
                    if trimmed.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{trimmed}")
                    }
                }
            };

            let host_slug: String = host_name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            let proto_low = proto_str.to_lowercase();
            let profile_id = if !editing_id.is_empty() {
                editing_id.clone()
            } else {
                format!("conn-{proto_low}-{host_slug}")
            };
            let parsed_port = port_str.parse::<u16>().unwrap_or(match proto_str.as_str() {
                "S3" | "WebDAV" => 443,
                "SMB" => 445,
                "FTPS" => 990,
                "FTP" => 21,
                _ => 22,
            });
            let display_label = if !custom_name.is_empty() {
                custom_name
            } else if user.is_empty() || proto_str == "S3" {
                host_name.clone()
            } else {
                format!("{user}@{host_name}")
            };

            let mut profile = ConnectionProfile::new_sftp_default(&profile_id, &display_label);
            profile.host = Some(host_name.clone());
            profile.port = Some(parsed_port);
            profile.username = user.clone();
            profile.base_path = base_path.clone();
            profile.local_path = (!local_path_raw.is_empty()).then_some(local_path_raw);
            profile.base_url = (!base_url_raw.is_empty()).then_some(base_url_raw);
            if !encoding_raw.is_empty() {
                profile.text_encoding = encoding_raw;
            }
            profile.note = note_raw;
            profile.color_label = color;
            profile.show_advanced = show_adv;
            profile.protocol = match proto_str.as_str() {
                "S3" => ProtocolType::S3,
                "SMB" => ProtocolType::Smb,
                "FTPS" => ProtocolType::Ftps,
                "FTP" => ProtocolType::Ftp,
                "WebDAV" => ProtocolType::WebDav,
                _ => ProtocolType::Sftp,
            };
            if proto_str == "SFTP" && auth_method_str == "private_key" {
                let raw_key = if key_path_raw.is_empty() {
                    "~/.ssh/id_ed25519"
                } else {
                    key_path_raw.as_str()
                };
                let expanded_key = expand_tilde_path(raw_key);
                profile.auth_type = AuthType::PublicKey;
                profile.key_path = Some(expanded_key);
            }
            if proto_str == "S3" {
                let reg_raw = w.get_connect_s3_region().trim().to_string();
                let end_raw = w.get_connect_s3_endpoint().trim().to_string();
                let reg_val = if reg_raw.is_empty() {
                    "us-east-1".to_string()
                } else {
                    reg_raw
                };
                let end_val = if end_raw.is_empty() {
                    "https://s3.amazonaws.com".to_string()
                } else {
                    end_raw
                };
                profile.bucket = Some(host_name.clone());
                profile.region = Some(reg_val);
                profile.endpoint = Some(end_val);
            }
            if let Err(err) = profile.validate() {
                guard.show_toast("error", format!("Invalid connection: {err}"));
                sync_window_from_runtime(&w, &mut guard);
                return;
            }

            let secret = match proto_str.as_str() {
                "S3" => w.get_connect_s3_secret_key().to_string(),
                _ => {
                    if auth_method_str == "private_key" {
                        w.get_connect_passphrase().to_string()
                    } else {
                        w.get_connect_password().to_string()
                    }
                }
            };
            let secret_opt = if !secret.is_empty() {
                Some(secret.clone())
            } else {
                guard
                    .credential_store
                    .get_secret(&profile.id)
                    .ok()
                    .flatten()
            };

            if w.get_connect_save_credentials() && !secret.is_empty() {
                let _ = guard.credential_store.set_secret(&profile.id, &secret);
            }

            if color == ColorLabel::None {
                guard.sidebar_colors.remove(&profile.id);
            } else {
                guard.sidebar_colors.insert(profile.id.clone(), color);
            }

            // Persist shortcut to connections.toml and add/update in Sidebar "Connections"
            guard.saved_connections.upsert_profile(profile.clone());
            let _ = guard.saved_connections.save_default();

            let vfs_url = profile_to_vfs_url(&profile);
            let sb_item = SidebarItem {
                id: profile.id.clone(),
                label: display_label.clone(),
                vfs_url: vfs_url.clone(),
                icon_kind: "globe".to_string(),
                capacity_badge: None,
                can_eject: true,
                children: vec![],
                expanded: false,
            };
            let conn_sec = guard
                .app_state
                .sidebar_sections
                .iter_mut()
                .find(|s| s.title == "Connections");
            if let Some(sec) = conn_sec {
                if let Some(existing) = sec.items.iter_mut().find(|i| i.id == profile.id) {
                    *existing = sb_item;
                } else {
                    sec.items.push(sb_item);
                }
            }

            match RemoteVfsBackend::connect_live(profile, secret_opt) {
                Ok(remote) => {
                    if let Some((prefix, _)) = split_remote_uri(&vfs_url) {
                        guard.remote_backends.insert(prefix, remote);
                    }
                    let is_right = guard.is_right_effective();
                    guard.navigate_to_path(is_right, vfs_url.clone());
                    let msg = format!("[CONNECT] Mounted {vfs_url}");
                    guard.logs.push(msg);
                    let label = if !editing_id.is_empty() {
                        format!("Saved & connected to {display_label}")
                    } else {
                        format!("Connected to {display_label}")
                    };
                    guard.start_tracked_task(&label, &vfs_url, 4096, label.clone(), "success");
                }
                Err(err) => {
                    let msg = format!("[ERROR] Connection to {host_name} failed: {err}");
                    guard.logs.push(msg);
                    let toast = format!("Connection to {host_name} failed: {err}");
                    guard.show_toast("error", toast);
                }
            }
            w.set_connect_editing_id(SharedString::default());
            w.set_show_connect_modal(false);
            sync_window_from_runtime(&w, &mut guard);
        });
    }

    // Centralized Cross-Platform Keyboard Shortcut Router (Section 3)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_handle_shortcut(move |key, ctrl, alt, shift, meta| {
            let Some(w) = win_weak.upgrade() else {
                return false;
            };
            let cmd = ctrl || meta;
            handle_keyboard_shortcut(&w, &rt, key.as_str(), (cmd, alt, shift))
        });
    }

    // 45ms UI Feedback Timer: drives the Tasks spinner, progress bar, async sync/thumbs, & status
    let anim_timer = Timer::default();
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        anim_timer.start(TimerMode::Repeated, Duration::from_millis(45), move || {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let mut guard = rt.borrow_mut();
            let resized = w.get_show_terminal_drawer() && guard.sync_terminal_dimensions(&w);
            if guard.poll_pty_output() || resized {
                sync_terminal_to_window(&w, &mut guard);
            }
            let mut need_transfer_sync = false;
            let mut need_full_sync = guard.poll_search_events();
            if guard.poll_bg_dir_sizes() || guard.drain_thumb_queue() {
                need_full_sync = true;
            }

            while let Ok(sync_ev) = guard.sync_rx.try_recv() {
                match sync_ev {
                    BgSyncEvent::PlanReady(res) => {
                        guard.sync_running = false;
                        match res {
                            Ok(plan) => guard.active_sync_plan = Some(plan),
                            Err(err) => {
                                guard.active_sync_plan = None;
                                guard.show_toast("error", err);
                            }
                        }
                        sync_sync_modal_to_window(&w, &guard);
                        need_full_sync = true;
                    }
                    BgSyncEvent::ExecFinished { dest_dir, result } => {
                        guard.sync_running = false;
                        guard.bg_tasks_in_flight = guard.bg_tasks_in_flight.saturating_sub(1);
                        match result {
                            Ok((total, applied)) => {
                                let msg = format!(
                                    "[SYNC] Synchronized {total} entries ({applied} updated)."
                                );
                                guard.logs.push(msg);
                                let label = format!("Synchronized {applied} of {total} entries");
                                guard.start_tracked_task(
                                    &label,
                                    &dest_dir,
                                    8192,
                                    label.clone(),
                                    "success",
                                );
                                w.set_show_sync_modal(false);
                            }
                            Err(err) => {
                                guard.logs.push(format!("[SYNC_ERR] {err}"));
                                guard.show_toast("error", err);
                                sync_sync_modal_to_window(&w, &guard);
                            }
                        }
                        guard.invalidate_dir_cache();
                        guard.reload_pane_entries(false);
                        guard.reload_pane_entries(true);
                        need_transfer_sync = true;
                        need_full_sync = true;
                    }
                }
            }

            while let Ok(ev) = guard.worker_rx.try_recv() {
                match ev {
                    BgWorkerEvent::Progress {
                        task_id,
                        transferred,
                        total,
                        label,
                    } => {
                        let tot = total.max(1);
                        let pct = ((transferred.saturating_mul(100)) / tot).clamp(5, 99);
                        guard.active_task_label = label.clone();
                        guard.active_task_progress = pct as i32;
                        for task in &mut guard.transfers {
                            if task.id == task_id {
                                task.transferred_bytes = transferred;
                                task.total_bytes = tot;
                            }
                        }
                        w.set_active_task_label(label.into());
                        w.set_active_task_progress(guard.active_task_progress);
                        need_transfer_sync = true;
                    }
                    BgWorkerEvent::Finished {
                        task_id,
                        ok_count,
                        err_msg,
                        toast_msg,
                    } => {
                        guard.bg_tasks_in_flight = guard.bg_tasks_in_flight.saturating_sub(1);
                        guard.transfer_controls.remove(&task_id);
                        let is_cancelled = err_msg
                            .as_deref()
                            .is_some_and(|m| m.starts_with("Cancelled"));
                        let is_ok = err_msg.is_none() && ok_count > 0;
                        for task in &mut guard.transfers {
                            if task.id == task_id {
                                if is_ok {
                                    task.transferred_bytes = task.total_bytes;
                                    task.status = TransferStatus::Completed;
                                } else if is_cancelled {
                                    task.status = TransferStatus::Cancelled;
                                } else {
                                    task.status = TransferStatus::Failed;
                                    task.error_message = err_msg.clone();
                                }
                            }
                        }
                        let log_line = match &err_msg {
                            Some(e) => format!("[BG-ERROR] {toast_msg} ({e})"),
                            None => format!("[BG-DONE] {toast_msg}"),
                        };
                        guard.logs.push(log_line);
                        let toast_kind = if is_ok {
                            "success"
                        } else if err_msg.is_some() && !is_cancelled {
                            "error"
                        } else {
                            "info"
                        };
                        let banner = match &err_msg {
                            Some(e) if !is_cancelled => format!("{toast_msg}: {e}"),
                            _ => toast_msg,
                        };
                        guard.show_toast(toast_kind, banner);
                        if guard.bg_tasks_in_flight == 0 && guard.active_task_ticks == 0 {
                            guard.active_task_progress = 100;
                        }
                        guard.invalidate_dir_cache();
                        guard.reload_pane_entries(false);
                        guard.reload_pane_entries(true);
                        need_transfer_sync = true;
                        need_full_sync = true;
                    }
                }
            }

            if guard.bg_tasks_in_flight > 0 {
                guard.spinner_frame = (guard.spinner_frame + 1) % 8;
                w.set_task_running(true);
                w.set_spinner_frame(guard.spinner_frame);
                w.set_active_task_progress(guard.active_task_progress);
            } else if guard.active_task_ticks > 0 {
                guard.active_task_ticks -= 1;
                guard.spinner_frame = (guard.spinner_frame + 1) % 8;
                let rem = guard.active_task_ticks as i32;
                let pct = (100 - rem * 11).clamp(15, 100);
                guard.active_task_progress = pct;
                let finished = guard.active_task_ticks == 0;

                for task in &mut guard.transfers {
                    if task.status == TransferStatus::InProgress {
                        if finished {
                            task.transferred_bytes = task.total_bytes;
                            task.status = TransferStatus::Completed;
                        } else {
                            let num = task.total_bytes.saturating_mul(pct as u64);
                            task.transferred_bytes = num / 100;
                        }
                        need_transfer_sync = true;
                    }
                }
                w.set_task_running(!finished);
                w.set_spinner_frame(guard.spinner_frame);
                w.set_active_task_progress(guard.active_task_progress);
            } else {
                w.set_task_running(false);
            }

            if guard.status_banner_ticks > 0 {
                guard.status_banner_ticks -= 1;
                if guard.status_banner_ticks == 0 {
                    guard.status_banner_text.clear();
                    guard.status_banner_is_error = false;
                    w.set_status_banner_text(SharedString::default());
                    w.set_status_banner_is_error(false);
                }
            }

            if (w.get_is_dragging_row() || guard.native_os_drag_handed_off)
                && !is_primary_mouse_button_pressed()
            {
                guard.native_os_drag_handed_off = false;
                let x = w.get_drag_ghost_x();
                let y = w.get_drag_ghost_y();
                clear_window_drag_overlay(&w);
                schedule_async_pointer_release(w.as_weak(), x, y);
            }

            if let Some((use_right, idx, clicked_at)) = guard.pending_rename_click {
                if w.get_is_dragging_row()
                    || w.get_show_context_menu()
                    || w.get_show_input_modal()
                    || w.get_show_delete_modal()
                {
                    guard.pending_rename_click = None;
                } else if clicked_at.elapsed() >= Duration::from_millis(320) {
                    guard.pending_rename_click = None;
                    let still_sole = if use_right {
                        guard.right_selected_indices.len() == 1
                            && guard.right_selected_indices.contains(&idx)
                    } else {
                        guard.left_selected_indices.len() == 1
                            && guard.left_selected_indices.contains(&idx)
                    };
                    let meta_opt = if !still_sole {
                        None
                    } else if use_right {
                        guard.right_entries.get(idx).map(|r| r.meta.clone())
                    } else {
                        guard.left_entries.get(idx).map(|r| r.meta.clone())
                    };
                    if let Some(meta) = meta_opt {
                        reset_slint_modifiers(&w);
                        let title = format!("Rename \"{}\"", meta.name);
                        w.set_input_modal_mode("rename".into());
                        w.set_input_modal_title(title.into());
                        w.set_input_modal_value(meta.name.into());
                        w.set_show_input_modal(true);
                        w.invoke_focus_input_modal();
                    }
                }
            }

            guard.fs_poll_tick = guard.fs_poll_tick.wrapping_add(1);
            if guard.fs_poll_tick.is_multiple_of(16)
                && guard.bg_tasks_in_flight == 0
                && !w.get_is_dragging_row()
                && !w.get_show_conflict_modal()
                && guard.poll_external_directory_changes()
            {
                need_full_sync = true;
            }

            guard.session_save_tick = guard.session_save_tick.wrapping_add(1);
            if (guard.session_dirty || guard.session_save_tick.is_multiple_of(64))
                && !need_full_sync
            {
                guard.session_dirty = false;
                guard.capture_and_save_session(&w);
            }

            if need_full_sync {
                sync_window_from_runtime(&w, &mut guard);
            } else if need_transfer_sync {
                let tr_rows: Vec<TransferRowSlint> = guard
                    .transfers
                    .iter()
                    .map(|t| {
                        let can_control = guard.transfer_controls.contains_key(&t.id)
                            && matches!(
                                t.status,
                                TransferStatus::InProgress
                                    | TransferStatus::Paused
                                    | TransferStatus::Queued
                            );
                        TransferRowSlint {
                            id: SharedString::from(&t.id),
                            priority_label: match t.priority {
                                TransferPriority::Foreground => SharedString::from("Foreground"),
                                TransferPriority::Background => SharedString::from("Background"),
                            },
                            source_name: SharedString::from(&t.source_path),
                            dest_path: SharedString::from(&t.destination_path),
                            status_text: SharedString::from(format!("{:?}", t.status)),
                            progress_pct: (t.progress_fraction() * 100.0) as i32,
                            is_paused: t.status == TransferStatus::Paused,
                            can_control,
                        }
                    })
                    .collect();
                w.set_transfer_rows(ModelRc::new(VecModel::from(tr_rows)));
            }
        });
    }

    let run_res = window.run().map_err(|e| e.to_string());
    rt.borrow().capture_and_save_session(&window);
    manifold_vfs::cleanup_secure_runtime_subdirs(&[
        "drag",
        "drag-out",
        "archive-preview",
        "cross-remote",
        "staging",
        "archive-contents",
    ]);
    run_res
}

#[cfg(test)]
mod tests;
