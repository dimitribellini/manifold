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
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use manifold_config::{
    AppPreferences, AuthType, ConnectionProfile, ConnectionsConfig, CredentialStore,
    FavoriteFolderEntry, PaneSessionConfig, PaneViewMode, ProtocolType, SessionState, SortColumn,
    SortDirection, SplitDirection, TabSessionConfig, WindowSessionConfig,
};
use manifold_preview::FileInspectorViewModel;
use manifold_sync::{ComparisonMode, SyncExecutionPlan, SyncStrategy};
use manifold_transfer::{
    ConflictModalDescriptor, ConflictResolution, TransferPriority, TransferStatus, TransferTask,
    DEFAULT_TRANSFER_CHUNK_SIZE,
};
use manifold_vfs::{
    ArchiveBackend, ArchiveFormat, ColorLabel, FileMetadata, FileSystemBackend, LocalFsBackend,
    PermissionsMatrix,
};
use manifold_vfs_remote::RemoteVfsBackend;
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
        TerminalSpanSlint,
        TerminalLineSlint
    } from "ui/manifold.slint";
}

#[derive(Clone)]
struct FlattenedRowEntry {
    meta: FileMetadata,
    depth: i32,
    is_expanded: bool,
}

#[derive(Clone, Debug, Default)]
struct ResolvedDropTarget {
    dst_dir: Option<String>,
    dst_label: String,
    is_move: bool,
    hover_sidebar_idx: i32,
    hover_left_tab: i32,
    hover_right_tab: i32,
    hover_left_row: i32,
    hover_right_row: i32,
    hover_left_pane: bool,
    hover_right_pane: bool,
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
    mounted_vfs: ArchiveBackend,
    remote_backends: HashMap<String, RemoteVfsBackend>,
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
    left_back_stack: Vec<String>,
    left_forward_stack: Vec<String>,
    right_back_stack: Vec<String>,
    right_forward_stack: Vec<String>,
    last_nav_instant: Option<Instant>,
    last_row_click: Option<(bool, usize, Instant)>,
    pending_rename_click: Option<(bool, usize, Instant)>,
    last_hovered_cell: Option<(bool, i32)>,
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
    spinner_frame: i32,
    active_task_ticks: u32,
    active_task_label: String,
    active_task_progress: i32,
    toast_ticks: u32,
    toast_text: String,
    toast_kind: String,
    terminal_cwd: String,
    last_pane_sync_path: String,
    active_shell_remote_prefix: Option<String>,
    term_emulator: TerminalEmulator,
    shell_session: Option<NativeShellSession>,
    term_last_size: (u16, u16),
    thumbnail_cache: HashMap<String, Option<Image>>,
    dir_cache: RefCell<HashMap<String, Vec<FileMetadata>>>,
    dir_fingerprints: RefCell<HashMap<String, u64>>,
    fs_poll_tick: u32,
    session_save_tick: u32,
    native_os_drag_handed_off: bool,
}

impl GuiRuntime {
    fn new(mut app_state: ManifoldAppState) -> Self {
        let mounted_vfs = ArchiveBackend::new("remote-mounts.tar", ArchiveFormat::Tar);
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
            mounted_vfs,
            remote_backends: HashMap::new(),
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
            left_back_stack: Vec::new(),
            left_forward_stack: Vec::new(),
            right_back_stack: Vec::new(),
            right_forward_stack: Vec::new(),
            last_nav_instant: None,
            last_row_click: None,
            pending_rename_click: None,
            last_hovered_cell: None,
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
            spinner_frame: 0,
            active_task_ticks: 0,
            active_task_label: String::new(),
            active_task_progress: 0,
            toast_ticks: 0,
            toast_text: String::new(),
            toast_kind: "info".to_string(),
            terminal_cwd: init_cwd.clone(),
            last_pane_sync_path: init_cwd,
            active_shell_remote_prefix: None,
            term_emulator,
            shell_session,
            term_last_size: (14, 110),
            thumbnail_cache: HashMap::new(),
            dir_cache: RefCell::new(HashMap::new()),
            dir_fingerprints: RefCell::new(HashMap::new()),
            fs_poll_tick: 0,
            session_save_tick: 0,
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
            let is_valid = split_remote_uri(&raw_path).is_some() || Path::new(&raw_path).is_dir();
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
            let is_valid = split_remote_uri(&raw_path).is_some() || Path::new(&raw_path).is_dir();
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
            });
        }
        if !restored_right.is_empty() {
            let max_r = restored_right.len() - 1;
            self.app_state.right_tabs = restored_right;
            self.app_state.right_active_tab = sess.right_pane.active_tab_index.min(max_r);
        }

        self.app_state.dual_pane_enabled = sess.window.dual_pane_enabled;
        self.app_state.panes_layout = if sess.window.dual_pane_enabled {
            PanesLayout::SplitHorizontal
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
        let win_cfg = WindowSessionConfig {
            width,
            height,
            split_ratio: f64::from(self.app_state.split_ratio),
            split_direction: SplitDirection::Vertical,
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
        left_changed || right_changed || mounts_changed
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
                    self.shell_session = NativeShellSession::spawn_remote_ssh(&spec, rows, cols);
                    return;
                }
            }
            let hdr = "\x1b[1;36m● Remote Session\x1b[0m";
            let banner = format!("{hdr} — \x1b[1;32m{conn_title}\x1b[0m ({clean_pref})\r\n");
            self.term_emulator.feed_bytes(banner.as_bytes());
            self.shell_session = None;
            return;
        }

        let keep_local = !force_restart
            && self.active_shell_remote_prefix.is_none()
            && self.shell_session.is_some();
        if keep_local {
            if let Some(shell) = &self.shell_session {
                shell.sync_cwd_if_idle(target_path);
            }
            return;
        }

        self.active_shell_remote_prefix = None;
        self.term_emulator = TerminalEmulator::new(rows as usize, cols as usize);
        self.shell_session = NativeShellSession::spawn(target_path, rows, cols);
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

        let tx = self.worker_tx.clone();
        let remotes = self.remote_backends.clone();
        std::thread::spawn(move || {
            let local_vfs = LocalFsBackend::new();
            let mut transferred_so_far = 0u64;
            let mut ok_count = 0usize;
            let mut last_err: Option<String> = None;

            for (src_path, dst_path, item_sz) in pairs {
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
                    (&tx, &task_id, &label, transferred_so_far, total_bytes),
                );
                match res {
                    Ok(()) => {
                        transferred_so_far = transferred_so_far.saturating_add(step_sz);
                        ok_count += 1;
                    }
                    Err(e) => {
                        last_err = Some(e);
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

    fn show_toast(&mut self, kind: &str, message: String) {
        self.toast_kind = kind.to_string();
        self.toast_text = message;
        self.toast_ticks = 0;
        play_system_sound(kind);
    }

    fn invalidate_dir_cache(&self) {
        self.dir_cache.borrow_mut().clear();
        *self.left_search_key.borrow_mut() = (String::new(), String::new(), false);
        *self.right_search_key.borrow_mut() = (String::new(), String::new(), false);
    }

    fn vfs_metadata(&self, path: &str) -> Option<FileMetadata> {
        if let Some(parent) = vfs_parent_dir(path) {
            if let Some(entries) = self.dir_cache.borrow().get(&parent) {
                if let Some(found) = entries.iter().find(|m| m.path == path) {
                    return Some(found.clone());
                }
            }
        }
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self.remote_backends.get(&prefix)?;
            let mut meta = remote.metadata(&remote_sub).ok()?;
            meta.path = format!("{prefix}{}", meta.path);
            return Some(meta);
        }
        self.mounted_vfs
            .metadata(path)
            .ok()
            .or_else(|| self.local_vfs.metadata(path).ok())
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
        if let Some((prefix, _)) = split_remote_uri(path) {
            if let Some(remote) = self.remote_backends.get(&prefix) {
                return remote;
            }
        }
        if self.mounted_vfs.metadata(path).is_ok() {
            &self.mounted_vfs
        } else {
            &self.local_vfs
        }
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
        if let Some((prefix, remote_sub)) = split_remote_uri(path) {
            let remote = self
                .remote_backends
                .get(&prefix)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            return remote.read_bytes(&remote_sub).map_err(|e| e.to_string());
        }
        if let Ok(bytes) = self.mounted_vfs.read_bytes(path) {
            return Ok(bytes);
        }
        self.local_vfs.read_bytes(path).map_err(|e| e.to_string())
    }

    fn vfs_write_bytes(&self, path: &str, data: &[u8]) -> Result<(), String> {
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

    fn vfs_copy_entry(&self, src_path: &str, dst_path: &str) -> Result<(), String> {
        self.invalidate_dir_cache();
        let src_slash = format!("{}/", src_path.trim_end_matches('/'));
        if dst_path == src_path || dst_path.starts_with(&src_slash) {
            return Err("Cannot copy or move a folder into itself".to_string());
        }
        let is_dir = if let Some((p, sub)) = split_remote_uri(src_path) {
            self.remote_backends
                .get(&p)
                .and_then(|r| r.metadata(&sub).ok())
                .is_some_and(|m| m.is_dir)
        } else if let Ok(m) = self.mounted_vfs.metadata(src_path) {
            m.is_dir
        } else {
            Path::new(src_path).is_dir()
        };
        if is_dir {
            if split_remote_uri(src_path).is_none()
                && split_remote_uri(dst_path).is_none()
                && Path::new(src_path).is_dir()
                && copy_local_dir_preserving_bundle(src_path, dst_path).is_ok()
            {
                return Ok(());
            }
            let _ = self.vfs_create_dir(dst_path);
            let children = self.list_sorted_dir(
                src_path,
                true,
                "",
                SortColumn::Name,
                SortDirection::Ascending,
            );
            for child in children {
                let child_dst = join_vfs_path(dst_path, &child.name);
                let _ = self.vfs_copy_entry(&child.path, &child_dst);
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
        if split_remote_uri(src_path).is_some() || split_remote_uri(dst_path).is_some() {
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
        let _ = self.vfs_remove(src_path, true);
        Ok(())
    }

    fn drag_items_for_row(&self, from_right: bool, row_idx: usize) -> Vec<FileMetadata> {
        let in_sel = if from_right {
            self.right_selected_indices.contains(&row_idx)
        } else {
            self.left_selected_indices.contains(&row_idx)
        };
        if in_sel {
            self.selected_metadata_list(from_right)
        } else {
            let single = if from_right {
                self.right_entries.get(row_idx).map(|r| r.meta.clone())
            } else {
                self.left_entries.get(row_idx).map(|r| r.meta.clone())
            };
            single.into_iter().collect()
        }
    }

    fn resolve_drop_target(
        &self,
        from_right: bool,
        items: &[FileMetadata],
        coords: (f32, f32, f32, f32, f32),
        bounds: (f32, f32, f32),
    ) -> ResolvedDropTarget {
        let mut res = ResolvedDropTarget {
            dst_dir: None,
            dst_label: String::new(),
            is_move: false,
            hover_sidebar_idx: -1,
            hover_left_tab: -1,
            hover_right_tab: -1,
            hover_left_row: -1,
            hover_right_row: -1,
            hover_left_pane: false,
            hover_right_pane: false,
        };
        if items.is_empty() {
            return res;
        }
        let (abs_x, abs_y, left_pane_w, left_scroll_y, right_scroll_y) = coords;
        let (sidebar_w, win_w, win_h) = bounds;
        let sb_w = sidebar_w.max(150.0);

        let is_outside_window = abs_x < 14.0
            || abs_y < 14.0
            || (win_w > 120.0 && abs_x > win_w - 14.0)
            || (win_h > 120.0 && abs_y > win_h - 14.0);
        if is_outside_window {
            res.dst_dir = Some("__EXTERNAL_OS_DROP__".to_string());
            res.dst_label = "→ Drop Outside Manifold (OS Drag & Drop)".to_string();
            return res;
        }

        // 1. Sidebar Hit-Testing (Devices, Connections, Favorites)
        // Top Toolbar (42px) + Workspace padding (6px) + Sidebar top padding (8px) = 56px
        let font_delta = (self.prefs.font_size_delta as f32).max(0.0);
        let sb_hdr_stride = 24.0 + font_delta + 2.0;
        let sb_item_h = 28.0 + font_delta * 1.5;
        let sb_item_stride = sb_item_h + 2.0;
        if (6.0..=(sb_w + 4.0)).contains(&abs_x) && abs_y >= 48.0 {
            let mut cur_y = 56.0 + self.sidebar_scroll_y;
            let mut sb_idx = 0usize;
            for section in &self.app_state.sidebar_sections {
                cur_y += sb_hdr_stride;
                sb_idx += 1;
                for item in &section.items {
                    if abs_y >= cur_y - 1.0 && abs_y < cur_y + sb_item_h + 1.0 {
                        let target = vfs_url_to_target(&item.vfs_url);
                        if !target.is_empty() && target != "connect://dialog" {
                            res.dst_dir = Some(target);
                            res.dst_label = format!("→ {}", item.label);
                            res.hover_sidebar_idx = sb_idx as i32;
                            return res;
                        }
                    }
                    cur_y += sb_item_stride;
                    sb_idx += 1;
                    for child in &item.children {
                        if abs_y >= cur_y - 1.0 && abs_y < cur_y + sb_item_h + 1.0 {
                            let target = vfs_url_to_target(&child.vfs_url);
                            if !target.is_empty() && target != "connect://dialog" {
                                res.dst_dir = Some(target);
                                res.dst_label = format!("→ {}", child.label);
                                res.hover_sidebar_idx = sb_idx as i32;
                                return res;
                            }
                        }
                        cur_y += sb_item_stride;
                        sb_idx += 1;
                    }
                }
            }
            return res;
        }

        if abs_x < sb_w + 6.0 {
            return res;
        }

        let left_pane_x = sb_w + 10.0;
        let pane_w = left_pane_w.max(260.0);
        let right_pane_x = left_pane_x + pane_w + 12.0;
        let target_is_right = self.app_state.dual_pane_enabled && abs_x >= right_pane_x - 6.0;
        let rel_x = if target_is_right {
            (abs_x - right_pane_x).max(0.0)
        } else {
            (abs_x - left_pane_x).max(0.0)
        };
        let split = self.app_state.split_ratio.clamp(0.2, 0.8);
        let target_pane_w = if target_is_right {
            (pane_w * (1.0 - split) / split).max(260.0)
        } else {
            pane_w
        };

        let tab_bar_top = 48.0f32;
        let tab_bar_bottom = tab_bar_top + 38.0 + font_delta * 1.5;
        let path_bar_top = tab_bar_bottom + 2.5;
        let path_bar_bottom = path_bar_top + 50.0 + font_delta * 2.0;

        // 2. Tab Bar Hit-Testing (Responsive to Pane Width & Tab Count)
        if (tab_bar_top..=tab_bar_bottom).contains(&abs_y) {
            let tab_count = if target_is_right {
                self.app_state.right_tabs.len().max(1) as f32
            } else {
                self.app_state.left_tabs.len().max(1) as f32
            };
            let avail_w = target_pane_w - 48.0 - (tab_count - 1.0).max(0.0) * 6.0;
            let tab_w = (avail_w / tab_count).clamp(56.0, 184.0);
            let tab_idx = ((rel_x - 8.0) / (tab_w + 6.0)).floor() as isize;
            if tab_idx >= 0 {
                let u_idx = tab_idx as usize;
                let tab_opt = if target_is_right {
                    self.app_state.right_tabs.get(u_idx)
                } else {
                    self.app_state.left_tabs.get(u_idx)
                };
                if let Some(tab) = tab_opt {
                    res.dst_dir = Some(tab.current_path.clone());
                    res.dst_label = format!("→ Tab: {}", tab.title);
                    if target_is_right {
                        res.hover_right_tab = tab_idx as i32;
                    } else {
                        res.hover_left_tab = tab_idx as i32;
                    }
                }
            }
            return res;
        }

        // 3. Breadcrumb / Path Bar Hit-Testing (Drop into Parent or Target Pane Root)
        if (path_bar_top..=path_bar_bottom).contains(&abs_y) {
            let pane_dir = self.current_pane_path(target_is_right);
            let same_pane = target_is_right == from_right;
            let dst = if same_pane {
                vfs_parent_dir(&pane_dir).unwrap_or(pane_dir)
            } else {
                pane_dir
            };
            let title = vfs_display_title(&dst);
            res.dst_dir = Some(dst);
            res.dst_label = if same_pane {
                format!("Move to {title}")
            } else {
                format!("→ {title}")
            };
            res.is_move = same_pane;
            if target_is_right {
                res.hover_right_pane = true;
            } else {
                res.hover_left_pane = true;
            }
            return res;
        }

        if abs_y <= path_bar_bottom {
            return res;
        }

        // 4. File Viewport Hit-Testing (Folder Row in Same/Other Pane, or Other Pane Root)
        let view_mode = if target_is_right {
            self.app_state
                .right_tabs
                .get(self.app_state.right_active_tab)
                .map(|t| view_mode_to_int(t.view_mode))
                .unwrap_or(0)
        } else {
            self.app_state
                .left_tabs
                .get(self.app_state.left_active_tab)
                .map(|t| view_mode_to_int(t.view_mode))
                .unwrap_or(0)
        };
        let scroll_y = if target_is_right {
            right_scroll_y
        } else {
            left_scroll_y
        };
        let hovered_idx = if view_mode == 0 || view_mode == 3 {
            let table_hdr_h = 26.0 + font_delta;
            let list_rows_top = path_bar_bottom + table_hdr_h + 4.0;
            let row_stride = 29.0 + font_delta * 2.0;
            if abs_y >= path_bar_bottom + table_hdr_h {
                ((abs_y - list_rows_top - scroll_y) / row_stride).floor() as isize
            } else {
                -1
            }
        } else if view_mode == 1 {
            let grid_top = path_bar_bottom + 12.0;
            let cell_stride = 138.0 + font_delta * 4.0;
            let row_i = ((abs_y - grid_top - scroll_y) / cell_stride).floor() as isize;
            let cell_w = (target_pane_w - 24.0).max(180.0) / 3.0;
            let col_i = ((rel_x - 12.0) / cell_w).floor().clamp(0.0, 2.0) as isize;
            if row_i >= 0 {
                row_i * 3 + col_i
            } else {
                -1
            }
        } else if rel_x >= 195.0 {
            ((abs_y - path_bar_bottom - scroll_y) / 26.0).floor() as isize
        } else {
            -1
        };

        if hovered_idx >= 0 {
            let u_idx = hovered_idx as usize;
            let entry_opt = if target_is_right {
                self.right_entries.get(u_idx)
            } else {
                self.left_entries.get(u_idx)
            };
            if let Some(entry) = entry_opt.filter(|e| is_browsable_folder(&e.meta)) {
                let valid_folder = items.iter().all(|it| {
                    let slash = format!("{}/", it.path.trim_end_matches('/'));
                    entry.meta.path != it.path && !entry.meta.path.starts_with(&slash)
                });
                if valid_folder {
                    let same_pane = target_is_right == from_right;
                    res.dst_dir = Some(entry.meta.path.clone());
                    res.dst_label = if same_pane {
                        format!("Move to {}", entry.meta.name)
                    } else {
                        format!("→ {}", entry.meta.name)
                    };
                    res.is_move = same_pane;
                    if target_is_right {
                        res.hover_right_row = hovered_idx as i32;
                    } else {
                        res.hover_left_row = hovered_idx as i32;
                    }
                    return res;
                }
            }
        }

        if target_is_right != from_right {
            let dst = self.current_pane_path(target_is_right);
            let title = vfs_display_title(&dst);
            res.dst_dir = Some(dst);
            res.dst_label = format!("→ {title}");
            res.is_move = false;
            if target_is_right {
                res.hover_right_pane = true;
            } else {
                res.hover_left_pane = true;
            }
        } else {
            let pane_dir = self.current_pane_path(from_right);
            let from_sub = items
                .iter()
                .any(|it| vfs_parent_dir(&it.path).as_deref() != Some(pane_dir.as_str()));
            if from_sub {
                let title = vfs_display_title(&pane_dir);
                res.dst_dir = Some(pane_dir);
                res.dst_label = format!("Move to {title}");
                res.is_move = true;
                if target_is_right {
                    res.hover_right_pane = true;
                } else {
                    res.hover_left_pane = true;
                }
            }
        }
        res
    }

    fn begin_batch_transfer(
        &mut self,
        items: Vec<FileMetadata>,
        dst_dir: String,
        is_move: bool,
        clear_cut_clipboard: bool,
    ) {
        if items.is_empty() {
            return;
        }
        if let Some((prefix, _)) = split_remote_uri(&dst_dir) {
            self.ensure_remote_connected(&prefix);
        }
        let first_name = items[0].name.clone();
        self.pending_batch = Some(PendingBatchTransfer {
            queue: VecDeque::from(items),
            current_collision: None,
            dst_dir,
            is_move,
            clear_cut_clipboard,
            apply_all_policy: None,
            sync_count: 0,
            sync_bytes: 0,
            first_name,
            bg_pairs: Vec::new(),
        });
        self.advance_batch_transfer();
    }

    fn apply_resolved_batch_item(
        &mut self,
        batch: &mut PendingBatchTransfer,
        src_meta: &FileMetadata,
        ex_meta: Option<&FileMetadata>,
        policy: ConflictResolution,
    ) -> bool {
        let mut final_dst = join_vfs_path(&batch.dst_dir, &src_meta.name);
        let mut remove_existing = false;

        if let Some(ex) = ex_meta {
            match policy {
                ConflictResolution::Stop => return false,
                ConflictResolution::Skip => {
                    let msg = format!("[CONFLICT] Skipped {}", src_meta.name);
                    self.logs.push(msg);
                    return true;
                }
                ConflictResolution::Replace => {
                    if src_meta.path == ex.path {
                        return true;
                    }
                    remove_existing = true;
                }
                ConflictResolution::ReplaceOlder => {
                    let is_newer = match (src_meta.modified, ex.modified) {
                        (Some(s), Some(e)) => s > e,
                        _ => true,
                    };
                    if !is_newer || src_meta.path == ex.path {
                        let msg = format!("[CONFLICT] Kept newer existing {}", src_meta.name);
                        self.logs.push(msg);
                        return true;
                    }
                    remove_existing = true;
                }
                ConflictResolution::KeepBoth => {
                    final_dst = self.unique_keep_both_path(&batch.dst_dir, &src_meta.name);
                }
                _ => {}
            }
        }

        if remove_existing && final_dst != src_meta.path {
            let _ = self.vfs_remove(&final_dst, true);
        }

        let src_remote = split_remote_uri(&src_meta.path).is_some();
        let dst_remote = split_remote_uri(&batch.dst_dir).is_some();
        let sz = src_meta.size.unwrap_or(4096);
        if src_remote || dst_remote || sz >= 4 * 1024 * 1024 {
            batch.bg_pairs.push((src_meta.path.clone(), final_dst, sz));
        } else {
            let ok = if batch.is_move {
                self.vfs_move_entry(&src_meta.path, &final_dst).is_ok()
            } else {
                self.vfs_copy_entry(&src_meta.path, &final_dst).is_ok()
            };
            if ok {
                batch.sync_bytes += sz;
                let tag = if batch.is_move { "MOVE" } else { "COPY" };
                let msg = format!("[{tag}] {} -> {final_dst}", src_meta.name);
                self.logs.push(msg);
                batch.sync_count += 1;
            }
        }
        true
    }

    fn advance_batch_transfer(&mut self) {
        let Some(mut batch) = self.pending_batch.take() else {
            return;
        };
        while let Some(src_meta) = batch.queue.pop_front() {
            if let Some((prefix, _)) = split_remote_uri(&src_meta.path) {
                self.ensure_remote_connected(&prefix);
            }
            let dst_path = join_vfs_path(&batch.dst_dir, &src_meta.name);
            if batch.is_move && dst_path == src_meta.path {
                continue;
            }
            if let Some(ex_meta) = self.vfs_metadata(&dst_path) {
                if let Some(policy) = batch.apply_all_policy {
                    let keep_going = self.apply_resolved_batch_item(
                        &mut batch,
                        &src_meta,
                        Some(&ex_meta),
                        policy,
                    );
                    if !keep_going {
                        break;
                    }
                    continue;
                }
                batch.current_collision = Some((src_meta, ex_meta));
                self.pending_batch = Some(batch);
                return;
            }
            let _ = self.apply_resolved_batch_item(
                &mut batch,
                &src_meta,
                None,
                ConflictResolution::Replace,
            );
        }
        self.finalize_batch_transfer(batch);
    }

    fn resolve_batch_conflict(&mut self, policy: ConflictResolution, apply_all: bool) {
        let Some(mut batch) = self.pending_batch.take() else {
            return;
        };
        if apply_all && policy != ConflictResolution::Stop {
            batch.apply_all_policy = Some(policy);
        }
        if let Some((src_meta, ex_meta)) = batch.current_collision.take() {
            let keep_going =
                self.apply_resolved_batch_item(&mut batch, &src_meta, Some(&ex_meta), policy);
            if !keep_going {
                self.finalize_batch_transfer(batch);
                return;
            }
        }
        self.pending_batch = Some(batch);
        self.advance_batch_transfer();
    }

    fn finalize_batch_transfer(&mut self, batch: PendingBatchTransfer) {
        if batch.clear_cut_clipboard && (batch.sync_count > 0 || !batch.bg_pairs.is_empty()) {
            self.clipboard_paths.clear();
            self.clipboard_cut = false;
        }
        let dst_title = self.format_tab_title(&batch.dst_dir);
        let verb = if batch.is_move { "Moved" } else { "Copied" };
        let first_name = batch.first_name;
        if !batch.bg_pairs.is_empty() {
            let n = batch.bg_pairs.len();
            let label = if n == 1 {
                format!("{verb} {first_name}")
            } else {
                format!("{verb} {n} items")
            };
            let toast = format!("{label} to {dst_title}");
            self.spawn_bg_transfer_batch(
                batch.bg_pairs,
                batch.is_move,
                label,
                batch.dst_dir.clone(),
                toast,
            );
        }
        if batch.sync_count > 0 {
            let count = batch.sync_count;
            let label = if count == 1 {
                format!("{verb} {first_name}")
            } else {
                format!("{verb} {count} items")
            };
            let toast = format!("{label} to {dst_title}");
            self.start_tracked_task(&label, &batch.dst_dir, batch.sync_bytes, toast, "success");
            self.invalidate_dir_cache();
            self.reload_pane_entries(false);
            self.reload_pane_entries(true);
        }
    }

    fn prepare_local_paths_for_external_drag(&mut self, items: &[FileMetadata]) -> Vec<String> {
        let mut out = Vec::with_capacity(items.len());
        let stage_dir = std::env::temp_dir().join("manifold-drag");
        for meta in items {
            if split_remote_uri(&meta.path).is_none() {
                if Path::new(&meta.path).exists() {
                    sanitize_macos_app_bundle(Path::new(&meta.path), cfg!(target_os = "macos"));
                    out.push(meta.path.clone());
                }
                continue;
            }
            if meta.is_dir {
                continue;
            }
            if std::fs::create_dir_all(&stage_dir).is_ok() {
                let dst_file = stage_dir.join(&meta.name);
                let dst_str = dst_file.to_string_lossy().to_string();
                let staged_ok = if let Some((prefix, sub)) = split_remote_uri(&meta.path) {
                    self.ensure_remote_connected(&prefix);
                    self.remote_backends
                        .get(&prefix)
                        .is_some_and(|r| r.download_to_local_file(&sub, &dst_file).is_ok())
                } else {
                    self.backend_for_path(&meta.path)
                        .read_bytes(&meta.path)
                        .ok()
                        .is_some_and(|b| std::fs::write(&dst_file, b).is_ok())
                };
                if staged_ok {
                    out.push(dst_str);
                }
            }
        }
        out
    }

    fn execute_drop_target(&mut self, items: &[FileMetadata], dst_dir: &str, is_move: bool) {
        if dst_dir == "__EXTERNAL_OS_DROP__" {
            let local_paths = self.prepare_local_paths_for_external_drag(items);
            self.clipboard_paths = local_paths.clone();
            self.clipboard_cut = false;
            copy_files_to_os_clipboard(&local_paths);

            if start_native_os_file_drag(&local_paths) {
                let msg = format!(
                    "[DRAG-OUT] Handed off {} item(s) to native OS drag session",
                    local_paths.len()
                );
                self.logs.push(msg);
                return;
            }

            if let Some(desktop_dir) = resolve_user_desktop_dir() {
                let not_already_on_desktop = items
                    .iter()
                    .any(|m| vfs_parent_dir(&m.path).as_deref() != Some(desktop_dir.as_str()));
                if not_already_on_desktop {
                    self.begin_batch_transfer(items.to_vec(), desktop_dir, false, false);
                    return;
                }
            }
            let msg = format!(
                "[DRAG-OUT] Exported {} item(s) to OS File Clipboard",
                items.len()
            );
            self.logs.push(msg);
            return;
        }
        self.begin_batch_transfer(items.to_vec(), dst_dir.to_string(), is_move, false);
    }

    fn paste_clipboard_into(&mut self, dst_dir: &str) {
        if self.clipboard_paths.is_empty() {
            return;
        }
        let sources = self.clipboard_paths.clone();
        let is_cut = self.clipboard_cut;
        let mut items = Vec::with_capacity(sources.len());
        for src_path in &sources {
            if let Some((prefix, _)) = split_remote_uri(src_path) {
                self.ensure_remote_connected(&prefix);
            }
            if let Some(meta) = self.vfs_metadata(src_path) {
                items.push(meta);
            } else {
                let name = vfs_file_name(src_path);
                items.push(FileMetadata {
                    name,
                    path: src_path.clone(),
                    extension: None,
                    kind: "Document".to_string(),
                    size: Some(4096),
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
                    owner: None,
                    group: None,
                });
            }
        }
        self.begin_batch_transfer(items, dst_dir.to_string(), is_cut, is_cut);
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
                self.right_back_stack.push(cur);
                self.right_forward_stack.clear();
            } else {
                self.left_back_stack.push(cur);
                self.left_forward_stack.clear();
            }
        }
        self.set_current_pane_path(is_right, new_path);
    }

    fn go_back(&mut self) {
        let is_right = self.is_right_effective();
        let cur = self.current_pane_path(is_right);
        let prev_opt = if is_right {
            self.right_back_stack.pop()
        } else {
            self.left_back_stack.pop()
        };
        if let Some(prev) = prev_opt {
            if is_right {
                self.right_forward_stack.push(cur);
            } else {
                self.left_forward_stack.push(cur);
            }
            self.set_current_pane_path(is_right, prev);
        }
    }

    fn go_forward(&mut self) {
        let is_right = self.is_right_effective();
        let cur = self.current_pane_path(is_right);
        let next_opt = if is_right {
            self.right_forward_stack.pop()
        } else {
            self.left_forward_stack.pop()
        };
        if let Some(next) = next_opt {
            if is_right {
                self.right_back_stack.push(cur);
            } else {
                self.left_back_stack.push(cur);
            }
            self.set_current_pane_path(is_right, next);
        }
    }

    fn set_current_pane_path(&mut self, is_right: bool, new_path: String) {
        if let Some((prefix, _)) = split_remote_uri(&new_path) {
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
        let raw_items = if let Some((prefix, remote_sub)) = split_remote_uri(dir_path) {
            if let Some(remote) = self.remote_backends.get(&prefix) {
                let mut entries = remote.list_directory(&remote_sub).unwrap_or_default();
                for entry in &mut entries {
                    entry.path = format!("{prefix}{}", entry.path);
                }
                entries
            } else {
                Vec::new()
            }
        } else if let Ok(items) = self.mounted_vfs.list_directory(dir_path) {
            items
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
        let raw_items = self.list_raw_dir_cached(dir_path);
        let q_low = query.trim().to_lowercase();
        let mut filtered: Vec<FileMetadata> = raw_items
            .into_iter()
            .filter(|m| show_hidden || !m.is_hidden)
            .filter(|m| matches_search_query_lower(m, &q_low))
            .collect();

        sort_metadata_slice(&mut filtered, sort_col, sort_dir);
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
        let show_hidden = self.app_state.toolbar.show_hidden_files;
        let query = if depth == 0 { opts.0 } else { "" };
        let items = self.list_sorted_dir(dir_path, show_hidden, query, opts.1, opts.2);
        for meta in items {
            let is_exp = is_browsable_folder(&meta) && expanded_set.contains(&meta.path);
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

        let mut sorted = if is_right {
            self.right_search_results.clone()
        } else {
            self.left_search_results.clone()
        };
        sort_metadata_slice(&mut sorted, sort_col, sort_dir);
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
        if let Some((prefix, _)) = split_remote_uri(&path) {
            self.ensure_remote_connected(&prefix);
        }
        let show_hidden = self.app_state.toolbar.show_hidden_files;

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
            sort_metadata_slice(&mut results, sort_col, sort_dir);
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
        let left_vol = if split_remote_uri(&left_path).is_some() {
            None
        } else {
            self.local_vfs
                .volume_stats(&left_path)
                .or_else(|_| self.local_vfs.volume_stats("/"))
                .ok()
        };
        let r_rem = split_remote_uri(&right_path).is_some();
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

        if is_browsable_folder(&meta) {
            self.last_nav_instant = Some(Instant::now());
            let msg = format!("[OPEN] Directory -> {}", meta.path);
            self.logs.push(msg);
            self.navigate_to_path(is_right, meta.path);
        } else {
            let msg = format!("[OPEN] File -> {}", meta.path);
            self.logs.push(msg);
            open_with_system_handler(&meta.path);
        }
    }
}

const fn play_system_sound(_kind: &str) {}

fn open_with_system_handler(path: &str) {
    if cfg!(target_os = "macos") {
        sanitize_macos_app_bundle(Path::new(path), true);
        let _ = Command::new("open").arg(path).spawn();
    } else if cfg!(target_os = "windows") {
        let _ = Command::new("explorer").arg(path).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(path).spawn();
    }
}

fn reveal_in_system_file_manager(path: &str) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("open").args(["-R", path]).spawn();
    } else if cfg!(target_os = "windows") {
        let select_arg = format!("/select,{path}");
        let _ = Command::new("explorer").arg(select_arg).spawn();
    } else if let Some(parent) = Path::new(path).parent().and_then(|p| p.to_str()) {
        let _ = Command::new("xdg-open").arg(parent).spawn();
    }
}

fn copy_string_to_system_clipboard(text: &str) {
    let cmd_name = if cfg!(target_os = "macos") {
        "pbcopy"
    } else if cfg!(target_os = "windows") {
        "clip"
    } else {
        "xclip"
    };
    let child_res = Command::new(cmd_name).stdin(Stdio::piped()).spawn();
    if let Ok(mut child) = child_res {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn read_string_from_system_clipboard() -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = Command::new("pbpaste").output().ok()?;
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        return None;
    }
    if cfg!(target_os = "windows") {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-Clipboard -Raw",
            ])
            .output()
            .ok()?;
        if out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        return None;
    }
    let candidates: [(&str, &[&str]); 3] = [
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(args).output() {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

fn copy_files_to_os_clipboard(paths: &[String]) {
    let local_paths: Vec<&str> = paths
        .iter()
        .map(String::as_str)
        .filter(|p| split_remote_uri(p).is_none() && Path::new(p).exists())
        .collect();
    if local_paths.is_empty() {
        return;
    }
    if cfg!(target_os = "macos") {
        let items: Vec<String> = local_paths
            .iter()
            .map(|p| {
                let escaped = p.replace('\\', "\\\\").replace('"', "\\\"");
                format!("POSIX file \"{escaped}\"")
            })
            .collect();
        let list_expr = items.join(", ");
        let script = format!("set the clipboard to {{{list_expr}}}");
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    } else if cfg!(target_os = "windows") {
        let items: Vec<String> = local_paths
            .iter()
            .map(|p| {
                let escaped = p.replace('\'', "''");
                format!("'{escaped}'")
            })
            .collect();
        let list_expr = items.join(",");
        let ps_cmd = format!("Set-Clipboard -Path {list_expr}");
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .spawn();
    } else {
        let uri_list: String = local_paths
            .iter()
            .map(|p| format!("file://{p}\r\n"))
            .collect();
        let child_res = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "text/uri-list"])
            .stdin(Stdio::piped())
            .spawn();
        if let Ok(mut child) = child_res {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(uri_list.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

fn resolve_user_desktop_dir() -> Option<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    if home.trim().is_empty() {
        return None;
    }
    let desk = Path::new(&home).join("Desktop");
    if desk.is_dir() {
        Some(desk.to_string_lossy().to_string())
    } else {
        None
    }
}

fn compute_local_dir_fingerprint(dir_path: &str) -> Option<u64> {
    if split_remote_uri(dir_path).is_some() {
        return None;
    }
    let rd = std::fs::read_dir(dir_path).ok()?;
    let mut count = 0u64;
    let mut acc = 0x9E37_79B9_7F4A_7C15_u64;
    for entry in rd.flatten() {
        count = count.wrapping_add(1);
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        for &b in entry.file_name().as_encoded_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        if let Ok(meta) = entry.metadata() {
            h ^= meta.len();
            h = h.wrapping_mul(0x0100_0000_01b3);
            if meta.is_dir() {
                h ^= 0x5555_AAAA_5555_AAAA;
            }
            if let Ok(mtime) = meta.modified() {
                if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    let secs_rot = dur.as_secs().rotate_left(16);
                    let nanos = u64::from(dur.subsec_nanos());
                    h ^= secs_rot ^ nanos;
                    h = h.wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        acc = acc.wrapping_add(h);
    }
    Some(acc ^ count.rotate_left(32))
}

fn window_logical_bounds(w: &ManifoldWindow) -> (f32, f32, f32) {
    let scale = w.window().scale_factor().max(1.0);
    let phys = w.window().size();
    let win_w = ((phys.width as f32) / scale).max(600.0);
    let win_h = ((phys.height as f32) / scale).max(400.0);
    (w.get_sidebar_width(), win_w, win_h)
}

fn clear_window_drag_overlay(w: &ManifoldWindow) {
    w.set_is_dragging_row(false);
    w.set_drag_ghost_label("".into());
    w.set_drag_target_hint("".into());
    w.set_drag_hover_sidebar_idx(-1);
    w.set_drag_hover_left_tab(-1);
    w.set_drag_hover_right_tab(-1);
    w.set_drag_hover_left_row(-1);
    w.set_drag_hover_right_row(-1);
    w.set_drag_hover_left_pane(false);
    w.set_drag_hover_right_pane(false);
}

fn schedule_async_pointer_release(win_weak: slint::Weak<ManifoldWindow>, abs_x: f32, abs_y: f32) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = win_weak.upgrade() {
            clear_window_drag_overlay(&w);
            let rel_ev = slint::platform::WindowEvent::PointerReleased {
                position: slint::LogicalPosition::new(abs_x, abs_y),
                button: slint::platform::PointerEventButton::Left,
            };
            w.window().dispatch_event(rel_ev);
            reset_slint_modifiers(&w);
        }
    });
}

fn reset_slint_modifiers(w: &ManifoldWindow) {
    let mod_keys = [
        slint::platform::Key::Shift,
        slint::platform::Key::Control,
        slint::platform::Key::Alt,
        slint::platform::Key::Meta,
    ];
    for key in mod_keys {
        let ev = slint::platform::WindowEvent::KeyReleased { text: key.into() };
        w.window().dispatch_event(ev);
    }
}

#[cfg(target_os = "macos")]
fn sanitize_click_modifiers(w: &ManifoldWindow, is_shift: bool, is_cmd: bool) -> (bool, bool) {
    if !is_shift && !is_cmd {
        return (false, false);
    }
    type MsgRetUsize = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> usize;
    let flags = unsafe {
        let ev_cls = objc_getClass(c"NSEvent".as_ptr());
        if ev_cls.is_null() {
            return (is_shift, is_cmd);
        }
        let msg_usize: MsgRetUsize = std::mem::transmute(objc_msgSend as *const ());
        msg_usize(ev_cls, sel_registerName(c"modifierFlags".as_ptr()))
    };
    let os_shift = (flags & (1 << 17)) != 0;
    let os_cmd = (flags & ((1 << 18) | (1 << 20))) != 0;
    let clean_shift = is_shift && os_shift;
    let clean_cmd = is_cmd && os_cmd;
    if (is_shift && !clean_shift) || (is_cmd && !clean_cmd) {
        reset_slint_modifiers(w);
    }
    (clean_shift, clean_cmd)
}

#[cfg(not(target_os = "macos"))]
fn sanitize_click_modifiers(_w: &ManifoldWindow, is_shift: bool, is_cmd: bool) -> (bool, bool) {
    (is_shift, is_cmd)
}

#[cfg(target_os = "macos")]
fn is_primary_mouse_button_pressed() -> bool {
    type MsgRetUsize = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> usize;
    unsafe {
        let ev_cls = objc_getClass(c"NSEvent".as_ptr());
        if ev_cls.is_null() {
            return true;
        }
        let msg_usize: MsgRetUsize = std::mem::transmute(objc_msgSend as *const ());
        let mask = msg_usize(ev_cls, sel_registerName(c"pressedMouseButtons".as_ptr()));
        (mask & 1) != 0
    }
}

#[cfg(not(target_os = "macos"))]
fn is_primary_mouse_button_pressed() -> bool {
    true
}

/// Formats any raw version string (e.g. `"v.0.3.1"`, `".0.3.1.2"`, `"0.3.1.2"`)
/// into the canonical `"v.<version>"` display format (`"v.0.3.1.2"`).
pub fn format_app_version_label(raw: &str) -> String {
    let cleaned = raw.trim().trim_start_matches(['v', 'V', '.']);
    let ver = if cleaned.is_empty() { "0.1.0" } else { cleaned };
    format!("v.{ver}")
}

/// Returns the current application version formatted as `"v.<version>"`.
pub fn current_app_version_label() -> String {
    let raw = option_env!("APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    format_app_version_label(raw)
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MacOsPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MacOsSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MacOsRect {
    origin: MacOsPoint,
    size: MacOsSize,
}

#[cfg(target_os = "macos")]
extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn objc_msgSend();
    fn objc_allocateClassPair(
        superclass: *mut std::ffi::c_void,
        name: *const std::ffi::c_char,
        extra_bytes: usize,
    ) -> *mut std::ffi::c_void;
    fn class_addMethod(
        cls: *mut std::ffi::c_void,
        name: *mut std::ffi::c_void,
        imp: *const std::ffi::c_void,
        types: *const std::ffi::c_char,
    ) -> bool;
    fn objc_registerClassPair(cls: *mut std::ffi::c_void);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn manifold_dragging_source_mask(
    _this: *mut std::ffi::c_void,
    _cmd: *mut std::ffi::c_void,
    _session: *mut std::ffi::c_void,
    _context: isize,
) -> usize {
    usize::MAX
}

#[cfg(target_os = "macos")]
unsafe fn ensure_macos_dragging_source_class() -> *mut std::ffi::c_void {
    static REGISTER_ONCE: std::sync::Once = std::sync::Once::new();
    let cls_name = c"ManifoldNativeDraggingSource";
    REGISTER_ONCE.call_once(|| {
        let existing = objc_getClass(cls_name.as_ptr());
        if existing.is_null() {
            let ns_obj = objc_getClass(c"NSObject".as_ptr());
            let pair = objc_allocateClassPair(ns_obj, cls_name.as_ptr(), 0);
            if !pair.is_null() {
                let sel = sel_registerName(
                    c"draggingSession:sourceOperationMaskForDraggingContext:".as_ptr(),
                );
                let imp = manifold_dragging_source_mask as *const std::ffi::c_void;
                let _ = class_addMethod(pair, sel, imp, c"Q@:@q".as_ptr());
                objc_registerClassPair(pair);
            }
        }
    });
    objc_getClass(cls_name.as_ptr())
}

#[cfg(target_os = "macos")]
fn start_native_os_file_drag(paths: &[String]) -> bool {
    type Msg0 =
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    type Msg1Ptr = unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *const std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    type Msg1Void =
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, *mut std::ffi::c_void);
    type MsgRetPoint =
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> MacOsPoint;
    type MsgConvertPt = unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        MacOsPoint,
        *mut std::ffi::c_void,
    ) -> MacOsPoint;
    type MsgSetFrame = unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        MacOsRect,
        *mut std::ffi::c_void,
    );
    type MsgBeginDrag = unsafe extern "C" fn(
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
        *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;

    if paths.is_empty() {
        return false;
    }

    unsafe {
        let fn_ptr = objc_msgSend as *const ();
        let msg0: Msg0 = std::mem::transmute(fn_ptr);
        let msg1_ptr: Msg1Ptr = std::mem::transmute(fn_ptr);
        let msg1_void: Msg1Void = std::mem::transmute(fn_ptr);
        let msg_pt: MsgRetPoint = std::mem::transmute(fn_ptr);
        let msg_conv_pt: MsgConvertPt = std::mem::transmute(fn_ptr);
        let msg_set_frame: MsgSetFrame = std::mem::transmute(fn_ptr);
        let msg_begin_drag: MsgBeginDrag = std::mem::transmute(fn_ptr);

        let ns_app_cls = objc_getClass(c"NSApplication".as_ptr());
        if ns_app_cls.is_null() {
            return false;
        }
        let ns_app = msg0(ns_app_cls, sel_registerName(c"sharedApplication".as_ptr()));
        if ns_app.is_null() {
            return false;
        }
        let mut ns_win = msg0(ns_app, sel_registerName(c"keyWindow".as_ptr()));
        if ns_win.is_null() {
            let wins = msg0(ns_app, sel_registerName(c"windows".as_ptr()));
            if !wins.is_null() {
                ns_win = msg0(wins, sel_registerName(c"firstObject".as_ptr()));
            }
        }
        if ns_win.is_null() {
            return false;
        }
        let content_view = msg0(ns_win, sel_registerName(c"contentView".as_ptr()));
        if content_view.is_null() {
            return false;
        }
        let drag_ev = msg0(ns_app, sel_registerName(c"currentEvent".as_ptr()));
        if drag_ev.is_null() {
            return false;
        }

        let src_cls = ensure_macos_dragging_source_class();
        if src_cls.is_null() {
            return false;
        }
        let drag_source = msg0(src_cls, sel_registerName(c"new".as_ptr()));
        if drag_source.is_null() {
            return false;
        }

        let sel_mouse_loc = sel_registerName(c"mouseLocationOutsideOfEventStream".as_ptr());
        let mouse_loc = msg_pt(ns_win, sel_mouse_loc);
        let sel_conv = sel_registerName(c"convertPoint:fromView:".as_ptr());
        let view_pt = msg_conv_pt(content_view, sel_conv, mouse_loc, std::ptr::null_mut());

        let arr_cls = objc_getClass(c"NSMutableArray".as_ptr());
        let items_arr = msg0(arr_cls, sel_registerName(c"array".as_ptr()));
        let str_cls = objc_getClass(c"NSString".as_ptr());
        let url_cls = objc_getClass(c"NSURL".as_ptr());
        let item_cls = objc_getClass(c"NSDraggingItem".as_ptr());
        let ws_cls = objc_getClass(c"NSWorkspace".as_ptr());
        let workspace = msg0(ws_cls, sel_registerName(c"sharedWorkspace".as_ptr()));

        let sel_utf8 = sel_registerName(c"stringWithUTF8String:".as_ptr());
        let sel_url = sel_registerName(c"fileURLWithPath:".as_ptr());
        let sel_alloc = sel_registerName(c"alloc".as_ptr());
        let sel_writer = sel_registerName(c"initWithPasteboardWriter:".as_ptr());
        let sel_icon = sel_registerName(c"iconForFile:".as_ptr());
        let sel_frame = sel_registerName(c"setDraggingFrame:contents:".as_ptr());
        let sel_add = sel_registerName(c"addObject:".as_ptr());

        let mut added_count = 0usize;
        for path in paths {
            let Ok(c_path) = std::ffi::CString::new(path.as_str()) else {
                continue;
            };
            let raw_ptr = c_path.as_ptr() as *const std::ffi::c_void;
            let ns_str = msg1_ptr(str_cls, sel_utf8, raw_ptr);
            if ns_str.is_null() {
                continue;
            }
            let file_url = msg1_ptr(url_cls, sel_url, ns_str);
            if file_url.is_null() {
                continue;
            }
            let alloc_item = msg0(item_cls, sel_alloc);
            if alloc_item.is_null() {
                continue;
            }
            let drag_item = msg1_ptr(alloc_item, sel_writer, file_url);
            if drag_item.is_null() {
                continue;
            }
            let icon = if !workspace.is_null() {
                msg1_ptr(workspace, sel_icon, ns_str)
            } else {
                std::ptr::null_mut()
            };
            let offset = (added_count as f64) * 6.0;
            let frame = MacOsRect {
                origin: MacOsPoint {
                    x: view_pt.x - 16.0 + offset,
                    y: view_pt.y - 16.0 - offset,
                },
                size: MacOsSize {
                    width: 32.0,
                    height: 32.0,
                },
            };
            msg_set_frame(drag_item, sel_frame, frame, icon);
            msg1_void(items_arr, sel_add, drag_item);
            added_count += 1;
        }

        if added_count == 0 {
            return false;
        }

        let sel_begin = sel_registerName(c"beginDraggingSessionWithItems:event:source:".as_ptr());
        let session = msg_begin_drag(content_view, sel_begin, items_arr, drag_ev, drag_source);
        !session.is_null()
    }
}

#[cfg(not(target_os = "macos"))]
fn start_native_os_file_drag(_paths: &[String]) -> bool {
    false
}

fn is_executable_file(meta: &FileMetadata) -> bool {
    if meta.is_dir {
        return false;
    }
    let ext_opt = meta
        .extension
        .as_deref()
        .map(|s| s.trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    if let Some(ext) = ext_opt.as_deref() {
        return matches!(
            ext,
            "sh" | "bash"
                | "zsh"
                | "fish"
                | "command"
                | "run"
                | "bin"
                | "exe"
                | "bat"
                | "cmd"
                | "ps1"
                | "app"
                | "apk"
                | "msi"
        );
    }
    (meta.permissions & 0o111 != 0) && (meta.permissions & 0o777 != 0o777)
}

fn move_path_to_trash(vfs: &LocalFsBackend, path: &str) -> Result<String, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let trimmed_home = home.trim_end_matches('/');
    let trash_dir = if cfg!(target_os = "macos") {
        format!("{trimmed_home}/.Trash")
    } else {
        format!("{trimmed_home}/.local/share/Trash/files")
    };
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("item");

    if !home.is_empty() && Path::new(&trash_dir).is_dir() {
        let mut candidate = format!("{trash_dir}/{name}");
        let mut counter = 2u32;
        while Path::new(&candidate).exists() {
            candidate = format!("{trash_dir}/{name} {counter}");
            counter += 1;
        }
        if std::fs::rename(path, &candidate).is_ok() {
            return Ok(candidate);
        }
    }
    vfs.remove(path, true).map_err(|e| e.to_string())?;
    Ok("Removed permanently".to_string())
}

fn duplicate_path_on_disk(vfs: &LocalFsBackend, meta: &FileMetadata) -> Result<String, String> {
    let parent = Path::new(&meta.path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("/");
    let keep_ext = !meta.is_dir || is_macos_app_bundle(meta);
    let (stem, ext_suffix) = match &meta.extension {
        Some(ext) if !ext.is_empty() && keep_ext => {
            let sfx = format!(".{ext}");
            let st = meta.name.strip_suffix(&sfx).unwrap_or(&meta.name);
            (st.to_string(), sfx)
        }
        _ => (meta.name.clone(), String::new()),
    };

    let trimmed_parent = parent.trim_end_matches('/');
    let mut dst_path = format!("{trimmed_parent}/{stem} copy{ext_suffix}");
    let mut idx = 2u32;
    while Path::new(&dst_path).exists() {
        dst_path = format!("{trimmed_parent}/{stem} copy {idx}{ext_suffix}");
        idx += 1;
    }

    if meta.is_dir {
        if copy_local_dir_preserving_bundle(&meta.path, &dst_path).is_err() {
            vfs.create_dir(&dst_path).map_err(|e| e.to_string())?;
        }
    } else {
        vfs.copy_file(&meta.path, &dst_path)
            .map_err(|e| e.to_string())?;
    }
    Ok(dst_path)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn matches_search_query_str(file_name: &str, q_low: &str) -> bool {
    if q_low.is_empty() {
        return true;
    }
    let name = file_name.to_lowercase();
    if q_low.contains('*') || q_low.contains('?') {
        let anchored = format!("*{q_low}*");
        wildcard_match(q_low, &name) || wildcard_match(&anchored, &name)
    } else if q_low.contains(' ') {
        q_low.split_whitespace().all(|tok| name.contains(tok))
    } else {
        name.contains(q_low)
    }
}

fn matches_search_query_lower(meta: &FileMetadata, q_low: &str) -> bool {
    matches_search_query_str(&meta.name, q_low)
}

#[cfg(test)]
fn matches_search_query(meta: &FileMetadata, query: &str) -> bool {
    let q_low = query.trim().to_lowercase();
    matches_search_query_lower(meta, &q_low)
}

#[cfg(unix)]
fn record_visited_dir(path: &Path, visited: &mut HashSet<(u64, u64)>) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    visited.insert((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn record_visited_dir(_path: &Path, _visited: &mut HashSet<(u64, u64)>) -> bool {
    true
}

struct LocalBfsSearchParams<'a> {
    is_right: bool,
    my_gen: u64,
    gen_arc: &'a Arc<AtomicU64>,
    root_dir: &'a str,
    q_low: &'a str,
    show_hidden: bool,
    color_labels: &'a HashMap<String, ColorLabel>,
    tx: &'a mpsc::Sender<BgSearchEvent>,
}

fn run_local_recursive_bfs_search(params: LocalBfsSearchParams<'_>) {
    const MAX_SEARCH_DEPTH: usize = 16;
    const MAX_SEARCH_RESULTS: usize = 2500;
    const BATCH_LIMIT: usize = 64;

    let LocalBfsSearchParams {
        is_right,
        my_gen,
        gen_arc,
        root_dir,
        q_low,
        show_hidden,
        color_labels,
        tx,
    } = params;

    if q_low.is_empty() || gen_arc.load(Ordering::Relaxed) != my_gen {
        return;
    }

    let local_vfs = LocalFsBackend::new();
    local_vfs.restore_color_labels(color_labels);
    let mut visited: HashSet<(u64, u64)> = HashSet::new();
    let root_pb = PathBuf::from(root_dir);
    let _ = record_visited_dir(&root_pb, &mut visited);

    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    if let Ok(rd) = std::fs::read_dir(&root_pb) {
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            let name_os = entry.file_name();
            let name_str = name_os.to_string_lossy();
            if !show_hidden && name_str.starts_with('.') {
                continue;
            }
            let sub_path = entry.path();
            if record_visited_dir(&sub_path, &mut visited) {
                queue.push_back((sub_path, 1));
            }
        }
    }

    let mut total_found = 0usize;
    let mut batch: Vec<FileMetadata> = Vec::with_capacity(BATCH_LIMIT);
    let mut last_flush = Instant::now();
    let mut step_counter = 0usize;

    while let Some((dir_pb, depth)) = queue.pop_front() {
        if gen_arc.load(Ordering::Relaxed) != my_gen {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&dir_pb) else {
            continue;
        };

        for entry in rd.flatten() {
            step_counter = step_counter.wrapping_add(1);
            if step_counter.is_multiple_of(64) && gen_arc.load(Ordering::Relaxed) != my_gen {
                return;
            }

            let name_os = entry.file_name();
            let name_str = name_os.to_string_lossy();
            if !show_hidden && name_str.starts_with('.') {
                continue;
            }

            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let entry_path = entry.path();

            if matches_search_query_str(&name_str, q_low) {
                let p_str = entry_path.to_string_lossy().to_string();
                if let Ok(meta) = local_vfs.metadata(&p_str) {
                    batch.push(meta);
                    total_found += 1;
                    if batch.len() >= BATCH_LIMIT
                        || last_flush.elapsed() >= Duration::from_millis(45)
                    {
                        let items = std::mem::take(&mut batch);
                        if tx
                            .send(BgSearchEvent::Batch {
                                is_right,
                                gen_id: my_gen,
                                items,
                            })
                            .is_err()
                        {
                            return;
                        }
                        last_flush = Instant::now();
                    }
                    if total_found >= MAX_SEARCH_RESULTS {
                        break;
                    }
                }
            }

            let is_app_bundle =
                is_macos_app_bundle_for_os(&name_str, ft.is_dir(), cfg!(target_os = "macos"));
            if ft.is_dir()
                && !ft.is_symlink()
                && !is_app_bundle
                && depth < MAX_SEARCH_DEPTH
                && record_visited_dir(&entry_path, &mut visited)
            {
                queue.push_back((entry_path, depth + 1));
            }
        }

        if total_found >= MAX_SEARCH_RESULTS {
            break;
        }
    }

    if gen_arc.load(Ordering::Relaxed) != my_gen {
        return;
    }
    if !batch.is_empty() {
        let _ = tx.send(BgSearchEvent::Batch {
            is_right,
            gen_id: my_gen,
            items: batch,
        });
    }
    let _ = tx.send(BgSearchEvent::Finished {
        is_right,
        gen_id: my_gen,
    });
}

fn is_macos_app_bundle_for_os(name: &str, is_dir: bool, is_macos: bool) -> bool {
    is_macos && is_dir && name.to_ascii_lowercase().ends_with(".app")
}

fn is_macos_app_bundle(meta: &FileMetadata) -> bool {
    is_macos_app_bundle_for_os(&meta.name, meta.is_dir, cfg!(target_os = "macos"))
}

fn is_browsable_folder(meta: &FileMetadata) -> bool {
    meta.is_dir && !is_macos_app_bundle(meta)
}

#[allow(clippy::too_many_arguments)]
fn sort_metadata_slice(items: &mut [FileMetadata], column: SortColumn, direction: SortDirection) {
    let desc = direction == SortDirection::Descending;
    match (column, desc) {
        (SortColumn::Size, false) => {
            items.sort_by_key(|a| {
                let sz = a.size.unwrap_or(0);
                (!is_browsable_folder(a), sz, a.name.to_lowercase())
            });
        }
        (SortColumn::Size, true) => {
            items.sort_by_key(|a| {
                let rev_sz = std::cmp::Reverse(a.size.unwrap_or(0));
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!is_browsable_folder(a), rev_sz, rev_name)
            });
        }
        (SortColumn::Modified | SortColumn::Created, false) => {
            items.sort_by_key(|a| {
                let name = a.name.to_lowercase();
                (!is_browsable_folder(a), a.modified, name)
            });
        }
        (SortColumn::Modified | SortColumn::Created, true) => {
            items.sort_by_key(|a| {
                let rev_mod = std::cmp::Reverse(a.modified);
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!is_browsable_folder(a), rev_mod, rev_name)
            });
        }
        (SortColumn::Kind, false) => {
            items.sort_by_key(|a| {
                let kind = a.kind.to_lowercase();
                (!is_browsable_folder(a), kind, a.name.to_lowercase())
            });
        }
        (SortColumn::Kind, true) => {
            items.sort_by_key(|a| {
                let rev_kind = std::cmp::Reverse(a.kind.to_lowercase());
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!is_browsable_folder(a), rev_kind, rev_name)
            });
        }
        (_, false) => {
            items.sort_by_key(|a| (!is_browsable_folder(a), a.name.to_lowercase()));
        }
        (_, true) => {
            items.sort_by_key(|a| {
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!is_browsable_folder(a), rev_name)
            });
        }
    }
}

fn is_image_extension(ext: Option<&str>) -> bool {
    let Some(e) = ext else {
        return false;
    };
    matches!(
        e.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "heic" | "bmp" | "tiff" | "ico"
    )
}

fn format_ext_badge(ext: Option<&str>) -> String {
    let Some(e) = ext else {
        return String::new();
    };
    e.chars().take(4).collect::<String>().to_ascii_uppercase()
}

fn color_label_to_slint_color(label: ColorLabel) -> Color {
    match label {
        ColorLabel::None => Color::from_argb_u8(0, 0, 0, 0),
        ColorLabel::Gray => Color::from_rgb_u8(0x8E, 0x8E, 0x93),
        ColorLabel::Green => Color::from_rgb_u8(0x5A, 0xC8, 0x5A),
        ColorLabel::Purple => Color::from_rgb_u8(0xAF, 0x52, 0xDE),
        ColorLabel::Blue => Color::from_rgb_u8(0x0A, 0x84, 0xFF),
        ColorLabel::Yellow => Color::from_rgb_u8(0xFF, 0xD6, 0x0A),
        ColorLabel::Red => Color::from_rgb_u8(0xFF, 0x45, 0x3A),
        ColorLabel::Orange => Color::from_rgb_u8(0xFF, 0x9F, 0x0A),
    }
}

fn color_label_to_tint_color(label: ColorLabel) -> Color {
    match label {
        ColorLabel::None => Color::from_argb_u8(0, 0, 0, 0),
        ColorLabel::Gray => Color::from_argb_u8(0x2A, 0x8E, 0x8E, 0x93),
        ColorLabel::Green => Color::from_argb_u8(0x2A, 0x5A, 0xC8, 0x5A),
        ColorLabel::Purple => Color::from_argb_u8(0x2A, 0xAF, 0x52, 0xDE),
        ColorLabel::Blue => Color::from_argb_u8(0x2A, 0x0A, 0x84, 0xFF),
        ColorLabel::Yellow => Color::from_argb_u8(0x2A, 0xFF, 0xD6, 0x0A),
        ColorLabel::Red => Color::from_argb_u8(0x2A, 0xFF, 0x45, 0x3A),
        ColorLabel::Orange => Color::from_argb_u8(0x2A, 0xFF, 0x9F, 0x0A),
    }
}

fn color_label_to_name(label: ColorLabel) -> &'static str {
    match label {
        ColorLabel::None => "",
        ColorLabel::Gray => "Gray",
        ColorLabel::Green => "Green",
        ColorLabel::Purple => "Purple",
        ColorLabel::Blue => "Blue",
        ColorLabel::Yellow => "Yellow",
        ColorLabel::Red => "Red",
        ColorLabel::Orange => "Orange",
    }
}

fn color_label_to_index(label: ColorLabel) -> i32 {
    match label {
        ColorLabel::None => 0,
        ColorLabel::Gray => 1,
        ColorLabel::Green => 2,
        ColorLabel::Purple => 3,
        ColorLabel::Blue => 4,
        ColorLabel::Yellow => 5,
        ColorLabel::Red => 6,
        ColorLabel::Orange => 7,
    }
}

fn index_to_color_label(idx: i32) -> ColorLabel {
    let swatches = ColorLabel::all_swatches();
    match swatches.get(idx as usize) {
        Some(color) => *color,
        None => ColorLabel::None,
    }
}

fn split_remote_uri(path: &str) -> Option<(String, String)> {
    let (scheme, rest) = path.split_once("://")?;
    if scheme == "file" || scheme == "connect" {
        return None;
    }
    match rest.split_once('/') {
        Some((auth, sub)) => {
            let prefix = format!("{scheme}://{auth}");
            let remote_path = format!("/{}", sub.trim_start_matches('/'));
            Some((prefix, remote_path))
        }
        None => {
            let prefix = format!("{scheme}://{rest}");
            Some((prefix, "/".to_string()))
        }
    }
}

fn vfs_display_title(path: &str) -> String {
    if let Some((prefix, sub)) = split_remote_uri(path) {
        let leaf = Path::new(&sub)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty());
        if let Some(name) = leaf {
            return name.to_string();
        }
        return prefix
            .split_once("://")
            .map(|(_, auth)| auth.to_string())
            .unwrap_or_else(|| prefix.clone());
    }
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("/")
        .to_string()
}

fn vfs_file_name(path: &str) -> String {
    if let Some((_, sub)) = split_remote_uri(path) {
        return Path::new(&sub)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("item")
            .to_string();
    }
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("item")
        .to_string()
}

fn vfs_parent_dir(path: &str) -> Option<String> {
    if let Some((prefix, sub)) = split_remote_uri(path) {
        if sub == "/" || sub.is_empty() {
            return Some(format!("{prefix}/"));
        }
        let parent = Path::new(&sub)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("/");
        if parent == "/" {
            return Some(format!("{prefix}/"));
        }
        return Some(format!("{prefix}{parent}"));
    }
    Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| Some("/".to_string()))
}

fn join_vfs_path(dir: &str, name: &str) -> String {
    let trimmed_dir = dir.trim_end_matches('/');
    let trimmed_name = name.trim_start_matches('/');
    if trimmed_dir.is_empty() {
        format!("/{trimmed_name}")
    } else {
        format!("{trimmed_dir}/{trimmed_name}")
    }
}

fn split_name_stem_ext(name: &str) -> (String, String) {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() && !ext.is_empty() {
            return (stem.to_string(), format!(".{ext}"));
        }
    }
    (name.to_string(), String::new())
}

fn sort_col_to_str(col: SortColumn) -> &'static str {
    match col {
        SortColumn::Size => "size",
        SortColumn::Modified | SortColumn::Created => "modified",
        _ => "name",
    }
}

fn view_mode_to_int(mode: PaneViewMode) -> i32 {
    match mode {
        PaneViewMode::List => 0,
        PaneViewMode::Icons => 1,
        PaneViewMode::Columns => 2,
        PaneViewMode::Tree => 3,
    }
}

fn int_to_view_mode(v: i32) -> PaneViewMode {
    match v {
        1 => PaneViewMode::Icons,
        2 => PaneViewMode::Columns,
        3 => PaneViewMode::Tree,
        _ => PaneViewMode::List,
    }
}

fn perform_bg_single_transfer(
    local_vfs: &LocalFsBackend,
    remotes: &HashMap<String, RemoteVfsBackend>,
    src_path: &str,
    dst_path: &str,
    is_move: bool,
    prog: (&mpsc::Sender<BgWorkerEvent>, &str, &str, u64, u64),
) -> Result<(), String> {
    let src_slash = format!("{}/", src_path.trim_end_matches('/'));
    if dst_path == src_path || dst_path.starts_with(&src_slash) {
        return Err("Cannot copy or move a folder into itself".to_string());
    }
    let (tx, task_id, label, base_transferred, total_bytes) = prog;
    let src_rem = split_remote_uri(src_path);
    let dst_rem = split_remote_uri(dst_path);

    match (src_rem, dst_rem) {
        // 1. Same-Remote copy or move
        (Some((p1, sub1)), Some((p2, sub2))) if p1 == p2 => {
            let remote = remotes
                .get(&p1)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            if is_move {
                return remote.rename(&sub1, &sub2).map_err(|e| e.to_string());
            }
            return remote.copy_file(&sub1, &sub2).map_err(|e| e.to_string());
        }
        // 2. Local -> Remote (Direct streaming via scp without reading multi-GB ISO into RAM)
        (None, Some((p2, sub2))) => {
            let remote = remotes
                .get(&p2)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            if Path::new(src_path).is_dir() {
                let _ = remote.create_dir(&sub2);
                if let Ok(entries) = local_vfs.list_directory(src_path) {
                    for child in entries {
                        let child_dst = join_vfs_path(dst_path, &child.name);
                        perform_bg_single_transfer(
                            local_vfs,
                            remotes,
                            &child.path,
                            &child_dst,
                            false,
                            prog,
                        )?;
                    }
                }
            } else {
                remote
                    .upload_local_file(Path::new(src_path), &sub2)
                    .map_err(|e| e.to_string())?;
            }
            if is_move {
                let _ = local_vfs.remove(src_path, true);
            }
            return Ok(());
        }
        // 3. Remote -> Local (Direct streaming via scp without buffering multi-GB ISO in RAM)
        (Some((p1, sub1)), None) => {
            let remote = remotes
                .get(&p1)
                .ok_or_else(|| "Remote VFS not connected".to_string())?;
            let is_dir = remote.metadata(&sub1).is_ok_and(|m| m.is_dir);
            if is_dir {
                let _ = local_vfs.create_dir(dst_path);
                if let Ok(entries) = remote.list_directory(&sub1) {
                    for child in entries {
                        let child_src = format!("{p1}{}", child.path);
                        let child_dst = join_vfs_path(dst_path, &child.name);
                        perform_bg_single_transfer(
                            local_vfs, remotes, &child_src, &child_dst, false, prog,
                        )?;
                    }
                }
            } else {
                remote
                    .download_to_local_file(&sub1, Path::new(dst_path))
                    .map_err(|e| e.to_string())?;
            }
            if is_move {
                let _ = remote.remove(&sub1, true);
            }
            return Ok(());
        }
        _ => {}
    }

    // 4. Local -> Local (4 MB chunked streaming with live progress updates)
    if is_move && local_vfs.rename(src_path, dst_path).is_ok() {
        sanitize_macos_app_bundle(Path::new(dst_path), cfg!(target_os = "macos"));
        return Ok(());
    }
    if Path::new(src_path).is_dir() {
        copy_local_dir_preserving_bundle(src_path, dst_path)?;
        if is_move {
            let _ = local_vfs.remove(src_path, true);
        }
        return Ok(());
    }
    let mut reader = std::fs::File::open(src_path).map_err(|e| e.to_string())?;
    let mut writer = std::fs::File::create(dst_path).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut copied = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        copied += n as u64;
        let _ = tx.send(BgWorkerEvent::Progress {
            task_id: task_id.to_string(),
            transferred: base_transferred.saturating_add(copied),
            total: total_bytes,
            label: label.to_string(),
        });
    }
    if is_move {
        let _ = local_vfs.remove(src_path, true);
    }
    Ok(())
}

fn apply_preferences_to_window(w: &ManifoldWindow, prefs: &AppPreferences) {
    let norm_family = match prefs.font_family.trim() {
        "" | "sans-serif" | "System Sans" => "sans-serif",
        "Inter" | "Inter / UI" => "Inter",
        "monospace" | "Monospace" => "monospace",
        "serif" | "Serif" => "serif",
        other => other,
    };
    let resolved_family = match norm_family {
        "Inter" => {
            if cfg!(target_os = "macos") {
                "Avenir Next"
            } else if cfg!(target_os = "windows") {
                "Segoe UI"
            } else {
                "DejaVu Sans"
            }
        }
        "monospace" => {
            if cfg!(target_os = "macos") {
                "Menlo"
            } else if cfg!(target_os = "windows") {
                "Consolas"
            } else {
                "DejaVu Sans Mono"
            }
        }
        "serif" => {
            if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
                "Georgia"
            } else {
                "DejaVu Serif"
            }
        }
        _ => "",
    };
    let resolved_mono = if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    };
    let norm_theme = match prefs.theme_preset.trim() {
        "midnight_ocean" | "Midnight Ocean" => "midnight_ocean",
        "dracula_pro" | "Dracula Pro" => "dracula_pro",
        "nordic_slate" | "Nordic Slate" => "nordic_slate",
        "solarized_dark" | "Solarized Dark" => "solarized_dark",
        "high_contrast" | "High Contrast" => "high_contrast",
        _ => "dark_zinc",
    };

    w.set_ui_font_size_delta(prefs.font_size_delta);
    w.set_ui_font_family(norm_family.into());
    w.set_resolved_font_family(resolved_family.into());
    w.set_resolved_mono_font_family(resolved_mono.into());
    w.set_ui_font_bold(prefs.font_bold);
    w.set_ui_theme_preset(norm_theme.into());

    let (canvas, toolbar, sidebar, surface, border, accent, hover, text_pri, text_sec) =
        match norm_theme {
            "midnight_ocean" => (
                Color::from_rgb_u8(0x0B, 0x11, 0x20),
                Color::from_rgb_u8(0x13, 0x1B, 0x2E),
                Color::from_rgb_u8(0x0F, 0x17, 0x2A),
                Color::from_rgb_u8(0x16, 0x20, 0x35),
                Color::from_rgb_u8(0x23, 0x32, 0x4E),
                Color::from_rgb_u8(0x02, 0x84, 0xC7),
                Color::from_rgb_u8(0x1E, 0x2D, 0x47),
                Color::from_rgb_u8(0xF1, 0xF5, 0xF9),
                Color::from_rgb_u8(0x94, 0xA3, 0xB8),
            ),
            "dracula_pro" => (
                Color::from_rgb_u8(0x19, 0x1A, 0x21),
                Color::from_rgb_u8(0x21, 0x22, 0x2C),
                Color::from_rgb_u8(0x1E, 0x1F, 0x29),
                Color::from_rgb_u8(0x28, 0x2A, 0x36),
                Color::from_rgb_u8(0x3B, 0x3E, 0x52),
                Color::from_rgb_u8(0x8B, 0x5C, 0xF6),
                Color::from_rgb_u8(0x34, 0x37, 0x46),
                Color::from_rgb_u8(0xF8, 0xF8, 0xF2),
                Color::from_rgb_u8(0xA4, 0xA8, 0xC4),
            ),
            "nordic_slate" => (
                Color::from_rgb_u8(0x1D, 0x21, 0x2A),
                Color::from_rgb_u8(0x24, 0x29, 0x33),
                Color::from_rgb_u8(0x21, 0x26, 0x30),
                Color::from_rgb_u8(0x2E, 0x34, 0x40),
                Color::from_rgb_u8(0x3B, 0x42, 0x52),
                Color::from_rgb_u8(0x5E, 0x81, 0xAC),
                Color::from_rgb_u8(0x3B, 0x42, 0x52),
                Color::from_rgb_u8(0xEC, 0xEF, 0xF4),
                Color::from_rgb_u8(0x9C, 0xA6, 0xB8),
            ),
            "solarized_dark" => (
                Color::from_rgb_u8(0x00, 0x1E, 0x26),
                Color::from_rgb_u8(0x00, 0x2B, 0x36),
                Color::from_rgb_u8(0x00, 0x24, 0x2E),
                Color::from_rgb_u8(0x07, 0x36, 0x42),
                Color::from_rgb_u8(0x16, 0x4B, 0x58),
                Color::from_rgb_u8(0x26, 0x8B, 0xD2),
                Color::from_rgb_u8(0x0D, 0x42, 0x50),
                Color::from_rgb_u8(0xEE, 0xE8, 0xD5),
                Color::from_rgb_u8(0x93, 0xA1, 0xA1),
            ),
            "high_contrast" => (
                Color::from_rgb_u8(0x05, 0x05, 0x07),
                Color::from_rgb_u8(0x11, 0x11, 0x15),
                Color::from_rgb_u8(0x09, 0x09, 0x0B),
                Color::from_rgb_u8(0x0C, 0x0C, 0x0F),
                Color::from_rgb_u8(0x3F, 0x3F, 0x48),
                Color::from_rgb_u8(0x25, 0x63, 0xEB),
                Color::from_rgb_u8(0x22, 0x22, 0x2A),
                Color::from_rgb_u8(0xFF, 0xFF, 0xFF),
                Color::from_rgb_u8(0xC4, 0xC4, 0xCE),
            ),
            _ => (
                Color::from_rgb_u8(0x12, 0x12, 0x14),
                Color::from_rgb_u8(0x1E, 0x1E, 0x22),
                Color::from_rgb_u8(0x16, 0x16, 0x18),
                Color::from_rgb_u8(0x1A, 0x1A, 0x1E),
                Color::from_rgb_u8(0x28, 0x28, 0x2D),
                Color::from_rgb_u8(0x0D, 0x52, 0xBF),
                Color::from_rgb_u8(0x26, 0x26, 0x2B),
                Color::from_rgb_u8(0xE1, 0xE1, 0xE6),
                Color::from_rgb_u8(0x88, 0x88, 0x92),
            ),
        };
    w.set_theme_bg_canvas(canvas.into());
    w.set_theme_bg_toolbar(toolbar.into());
    w.set_theme_bg_sidebar(sidebar.into());
    w.set_theme_bg_surface(surface.into());
    w.set_theme_border(border.into());
    w.set_theme_accent(accent.into());
    w.set_theme_row_hover(hover.into());
    w.set_theme_text_primary(text_pri.into());
    w.set_theme_text_secondary(text_sec.into());
}

fn argb_to_slint_color(argb: u32) -> Color {
    let a = ((argb >> 24) & 0xFF) as u8;
    let r = ((argb >> 16) & 0xFF) as u8;
    let g = ((argb >> 8) & 0xFF) as u8;
    let b = (argb & 0xFF) as u8;
    Color::from_argb_u8(a, r, g, b)
}

fn styled_span_to_slint(span: Option<&StyledSpan>) -> TerminalSpanSlint {
    let Some(s) = span else {
        return TerminalSpanSlint::default();
    };
    TerminalSpanSlint {
        text: SharedString::from(&s.text),
        char_count: s.char_count as i32,
        fg: argb_to_slint_color(s.fg).into(),
        bg: argb_to_slint_color(s.bg).into(),
        has_bg: s.bg != 0,
        bold: s.bold,
        underline: s.underline,
    }
}

fn sync_terminal_to_window(window: &ManifoldWindow, rt: &mut GuiRuntime) {
    let _ = rt.sync_terminal_dimensions(window);
    window.set_terminal_cwd(rt.terminal_cwd.clone().into());
    window.set_terminal_scroll_offset(rt.term_emulator.scroll_offset() as i32);
    window.set_terminal_max_scroll(rt.term_emulator.max_scroll_offset() as i32);
    window.set_terminal_alt_screen(rt.term_emulator.using_alt_screen());

    let (rows, cols) = rt.term_last_size;
    let shell_name = rt
        .shell_session
        .as_ref()
        .map(NativeShellSession::shell_name)
        .unwrap_or("shell");
    let term_session_lbl = if split_remote_uri(&rt.terminal_cwd).is_some() {
        let conn_title = rt.format_tab_title(&rt.terminal_cwd);
        format!("{shell_name} ({cols}×{rows}) — {conn_title}")
    } else {
        format!("{shell_name} ({cols}×{rows})")
    };
    window.set_terminal_session_label(term_session_lbl.into());

    let focused = window.get_terminal_focused();
    let rendered = rt.term_emulator.render_visible_lines(focused);
    let slint_lines: Vec<TerminalLineSlint> = rendered
        .iter()
        .map(|line| {
            let sp = &line.spans;
            TerminalLineSlint {
                s0: styled_span_to_slint(sp.first()),
                s1: styled_span_to_slint(sp.get(1)),
                s2: styled_span_to_slint(sp.get(2)),
                s3: styled_span_to_slint(sp.get(3)),
                s4: styled_span_to_slint(sp.get(4)),
                s5: styled_span_to_slint(sp.get(5)),
                s6: styled_span_to_slint(sp.get(6)),
                s7: styled_span_to_slint(sp.get(7)),
                s8: styled_span_to_slint(sp.get(8)),
                s9: styled_span_to_slint(sp.get(9)),
                s10: styled_span_to_slint(sp.get(10)),
                s11: styled_span_to_slint(sp.get(11)),
                s12: styled_span_to_slint(sp.get(12)),
                s13: styled_span_to_slint(sp.get(13)),
                s14: styled_span_to_slint(sp.get(14)),
                s15: styled_span_to_slint(sp.get(15)),
            }
        })
        .collect();
    window.set_terminal_lines(ModelRc::new(VecModel::from(slint_lines)));
}

fn sync_window_from_runtime(window: &ManifoldWindow, rt: &mut GuiRuntime) {
    apply_preferences_to_window(window, &rt.prefs);
    let folder_title = rt.app_state.toolbar.active_folder_title.clone();
    let show_hidden = rt.app_state.toolbar.show_hidden_files;
    let dual_mode = rt.app_state.dual_pane_enabled;
    let is_right_active = rt.is_right_effective();
    let can_back = if is_right_active {
        !rt.right_back_stack.is_empty()
    } else {
        !rt.left_back_stack.is_empty()
    };
    let can_fwd = if is_right_active {
        !rt.right_forward_stack.is_empty()
    } else {
        !rt.left_forward_stack.is_empty()
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
    window.set_show_toast(rt.toast_ticks > 0);
    window.set_toast_text(rt.toast_text.clone().into());
    window.set_toast_kind(rt.toast_kind.clone().into());

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

    // Rows, 3-Column Polaroid Icon Triples, and Miller Parent Columns
    let l_base_opt = (!l_filter.is_empty()).then_some(l_cur_path.as_str());
    let l_rows = build_slint_rows_from_flattened(
        &rt.left_entries,
        &rt.left_selected_indices,
        &mut rt.thumbnail_cache,
        true,
        l_base_opt,
    );
    let l_triples = if l_mode == 1 {
        build_icon_triples(&l_rows)
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
            &mut rt.thumbnail_cache,
        )
    } else {
        Vec::new()
    };
    window.set_left_rows(ModelRc::new(VecModel::from(l_rows)));
    window.set_left_icon_triples(ModelRc::new(VecModel::from(l_triples)));
    window.set_left_parent_rows(ModelRc::new(VecModel::from(l_parent_rows)));

    if dual_mode {
        let r_base_opt = (!r_filter.is_empty()).then_some(r_cur_path.as_str());
        let r_rows = build_slint_rows_from_flattened(
            &rt.right_entries,
            &rt.right_selected_indices,
            &mut rt.thumbnail_cache,
            true,
            r_base_opt,
        );
        let r_triples = if r_mode == 1 {
            build_icon_triples(&r_rows)
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
                &mut rt.thumbnail_cache,
            )
        } else {
            Vec::new()
        };
        window.set_right_rows(ModelRc::new(VecModel::from(r_rows)));
        window.set_right_icon_triples(ModelRc::new(VecModel::from(r_triples)));
        window.set_right_parent_rows(ModelRc::new(VecModel::from(r_parent_rows)));
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
        .map(|t| TransferRowSlint {
            id: SharedString::from(&t.id),
            priority_label: match t.priority {
                TransferPriority::Foreground => SharedString::from("Foreground"),
                TransferPriority::Background => SharedString::from("Background"),
            },
            source_name: SharedString::from(&t.source_path),
            dest_path: SharedString::from(&t.destination_path),
            status_text: SharedString::from(format!("{:?}", t.status)),
            progress_pct: (t.progress_fraction() * 100.0) as i32,
        })
        .collect();
    window.set_transfer_rows(ModelRc::new(VecModel::from(tr_rows)));

    let log_strings: Vec<SharedString> = rt.logs.iter().map(SharedString::from).collect();
    window.set_log_rows(ModelRc::new(VecModel::from(log_strings)));
    rt.capture_and_save_session(window);
}

fn update_rows_selection_in_place(rows: &ModelRc<FileRowSlint>, selected_indices: &HashSet<usize>) {
    let count = rows.row_count();
    for idx in 0..count {
        if let Some(mut row) = rows.row_data(idx) {
            let should_sel = selected_indices.contains(&idx);
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
            let base = idx * 3;
            let sel0 = triple.has_0 && selected_indices.contains(&base);
            let sel1 = triple.has_1 && selected_indices.contains(&(base + 1));
            let sel2 = triple.has_2 && selected_indices.contains(&(base + 2));
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
    update_rows_selection_in_place(&l_rows, &rt.left_selected_indices);
    let l_triples = window.get_left_icon_triples();
    update_triples_selection_in_place(&l_triples, &rt.left_selected_indices);
    let r_rows = window.get_right_rows();
    update_rows_selection_in_place(&r_rows, &rt.right_selected_indices);
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

fn build_slint_rows_from_flattened(
    entries: &[FlattenedRowEntry],
    selected_indices: &HashSet<usize>,
    thumb_cache: &mut HashMap<String, Option<Image>>,
    load_thumbs: bool,
    search_base_dir: Option<&str>,
) -> Vec<FileRowSlint> {
    entries
        .iter()
        .enumerate()
        .take(320)
        .map(|(idx, entry)| {
            let mut row = meta_to_slint_row(
                &entry.meta,
                entry.depth,
                entry.is_expanded,
                selected_indices.contains(&idx),
                thumb_cache,
                load_thumbs,
            );
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
    thumb_cache: &mut HashMap<String, Option<Image>>,
) -> Vec<FileRowSlint> {
    entries
        .iter()
        .enumerate()
        .take(320)
        .map(|(idx, meta)| {
            let is_sel = selected_idx == Some(idx);
            meta_to_slint_row(meta, 0, false, is_sel, thumb_cache, true)
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
        let cache_dir = std::env::temp_dir().join("manifold-app-icons");
        let _ = std::fs::create_dir_all(&cache_dir);
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
    depth: i32,
    is_expanded: bool,
    is_selected: bool,
    thumb_cache: &mut HashMap<String, Option<Image>>,
    load_thumbs: bool,
) -> FileRowSlint {
    let hex = meta.color_label.hex_color().unwrap_or("");
    let tag_name = color_label_to_name(meta.color_label);
    let brush = color_label_to_slint_color(meta.color_label);
    let tint = color_label_to_tint_color(meta.color_label);
    let ext_ref = meta.extension.as_deref();
    let is_app = is_macos_app_bundle(meta);
    let is_browsable = is_browsable_folder(meta);
    let is_img = is_image_extension(ext_ref);
    let is_exec = is_executable_file(meta);
    let is_local = split_remote_uri(&meta.path).is_none();
    let can_thumb = load_thumbs
        && is_img
        && !meta.is_dir
        && is_local
        && meta.size.unwrap_or(0) <= 16 * 1024 * 1024;
    let thumb_opt = if load_thumbs && is_app && is_local {
        if thumb_cache.len() > 2048 && !thumb_cache.contains_key(&meta.path) {
            thumb_cache.clear();
        }
        thumb_cache
            .entry(meta.path.clone())
            .or_insert_with(|| load_macos_app_bundle_icon(&meta.path))
            .clone()
    } else if can_thumb {
        if thumb_cache.len() > 2048 && !thumb_cache.contains_key(&meta.path) {
            thumb_cache.clear();
        }
        thumb_cache
            .entry(meta.path.clone())
            .or_insert_with(|| Image::load_from_path(Path::new(&meta.path)).ok())
            .clone()
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
    FileRowSlint {
        name: SharedString::from(meta.display_name()),
        path: SharedString::from(&meta.path),
        ext_badge: SharedString::from(badge_str),
        size_text: SharedString::from(meta.formatted_size()),
        modified_text: SharedString::from(meta.formatted_iso_datetime()),
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

fn build_icon_triples(rows: &[FileRowSlint]) -> Vec<IconGridTripleSlint> {
    let mut triples = Vec::new();
    let limit = rows.len().min(195);
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
        triples.push(IconGridTripleSlint {
            has_0: true,
            idx_0: idx as i32,
            item_0,
            has_1,
            idx_1: (idx + 1) as i32,
            item_1,
            has_2,
            idx_2: (idx + 2) as i32,
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

    // Toggle Dual-Pane vs Single-Pane mode
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

    // Draggable Dual-Pane Splitter Handle (Section 2.2)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_adjust_split_ratio(move |delta_px| {
            let mut guard = rt.borrow_mut();
            let delta = delta_px / 960.0;
            let next_ratio = (guard.app_state.split_ratio + delta).clamp(0.20, 0.80);
            guard.app_state.split_ratio = next_ratio;
            if let Some(w) = win_weak.upgrade() {
                w.set_split_ratio(next_ratio);
            }
        });
    }
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
            let is_mounted = guard.mounted_vfs.metadata(&expanded).is_ok();
            let target = if is_remote || is_mounted || Path::new(&expanded).is_dir() {
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
            let idx = row_idx as usize;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.last_row_click = Some((use_right, idx, Instant::now()));
            guard.apply_row_selection(use_right, idx, false, false);
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
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
                w.set_show_tab_preview(true);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Mouseover File Row / Cell Details & Non-Binary Preview Card
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_hover_file_row(move |is_right, row_idx, abs_x, abs_y| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let mut guard = rt.borrow_mut();
            if row_idx < 0 {
                guard.last_hovered_cell = None;
                w.set_show_hover_preview(false);
                return;
            }
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            if guard.last_hovered_cell == Some((use_right, row_idx)) {
                w.set_hover_preview_x(abs_x);
                w.set_hover_preview_y(abs_y);
                w.set_show_hover_preview(true);
                return;
            }
            guard.last_hovered_cell = Some((use_right, row_idx));
            let idx = row_idx as usize;
            let meta_opt = if use_right {
                guard.right_entries.get(idx).map(|r| r.meta.clone())
            } else {
                guard.left_entries.get(idx).map(|r| r.meta.clone())
            };
            let Some(meta) = meta_opt else {
                w.set_show_hover_preview(false);
                return;
            };

            let is_img = is_image_extension(meta.extension.as_deref());
            let can_img = is_img
                && !meta.is_dir
                && split_remote_uri(&meta.path).is_none()
                && meta.size.unwrap_or(0) <= 16 * 1024 * 1024;
            let loaded_img = if can_img {
                guard
                    .thumbnail_cache
                    .entry(meta.path.clone())
                    .or_insert_with(|| Image::load_from_path(Path::new(&meta.path)).ok())
                    .clone()
            } else {
                None
            };
            let img_dims = loaded_img.as_ref().map(|im| {
                let s = im.size();
                (s.width, s.height)
            });

            let (m_cat, m_l1, m_l2, _) = extract_media_metadata(&guard, &meta, img_dims);
            let media_line = if !m_l1.is_empty() && !m_l2.is_empty() {
                format!("{m_l1} • {m_l2}")
            } else if !m_l1.is_empty() {
                m_l1.clone()
            } else {
                m_cat
            };

            let (prev_text, has_text) = if !meta.is_dir && !is_img {
                build_inspector_preview(&guard, &meta)
            } else {
                (String::new(), false)
            };

            let sym_perm = meta.permissions_matrix().to_symbolic_string();
            let oct_perm = meta.permissions_matrix().to_octal_string();
            let perms_s = format!("{sym_perm} ({oct_perm})");

            w.set_hover_preview_x(abs_x);
            w.set_hover_preview_y(abs_y);
            w.set_hover_file_name(meta.display_name().into());
            w.set_hover_file_kind(meta.kind.clone().into());
            w.set_hover_file_size(meta.formatted_size().into());
            w.set_hover_file_modified(meta.formatted_iso_datetime().into());
            w.set_hover_file_perms(perms_s.into());
            w.set_hover_media_info(media_line.into());
            if let Some(im) = loaded_img {
                w.set_hover_image(im);
                w.set_hover_has_image(true);
            } else {
                w.set_hover_has_image(false);
            }
            if has_text {
                let short_snip: String = prev_text.lines().take(7).collect::<Vec<_>>().join("\n");
                w.set_hover_text(short_snip.into());
                w.set_hover_has_text(true);
            } else {
                w.set_hover_has_text(false);
            }
            w.set_show_hover_preview(true);
        });
    }

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
                w.set_has_clipboard_item(!guard.clipboard_paths.is_empty());
                sync_selection_and_inspector(&w, &mut guard);
                w.set_context_is_blank(false);
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
                w.set_has_clipboard_item(!guard.clipboard_paths.is_empty());
                sync_selection_and_inspector(&w, &mut guard);
                w.set_context_is_blank(true);
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
                    if let (Some(meta), Some(w)) = (sel_meta, win_weak.upgrade()) {
                        let title = format!("Rename \"{}\"", meta.name);
                        w.set_input_modal_mode("rename".into());
                        w.set_input_modal_title(title.into());
                        w.set_input_modal_value(meta.name.into());
                        w.set_show_input_modal(true);
                        w.invoke_focus_input_modal();
                        return;
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
                            }
                        }
                    }
                }
                "duplicate" => {
                    let mut dup_count = 0usize;
                    let mut total_bytes = 0u64;
                    let mut first_name = String::new();
                    let mut last_dup_path = String::new();
                    for meta in &sel_list {
                        if let Ok(dup_path) = duplicate_path_on_disk(&guard.local_vfs, meta) {
                            if first_name.is_empty() {
                                first_name = meta.name.clone();
                            }
                            last_dup_path = dup_path.clone();
                            total_bytes += meta.size.unwrap_or(4096);
                            guard.logs.push(format!("[DUPLICATE] -> {dup_path}"));
                            dup_count += 1;
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
                    }
                }
                "make_alias" => {
                    if let Some(meta) = sel_meta {
                        let alias_path = format!("{} alias", meta.path);
                        #[cfg(unix)]
                        {
                            let _ = std::os::unix::fs::symlink(&meta.path, &alias_path);
                        }
                        #[cfg(windows)]
                        {
                            let sym_res = if meta.is_dir {
                                std::os::windows::fs::symlink_dir(&meta.path, &alias_path)
                            } else {
                                std::os::windows::fs::symlink_file(&meta.path, &alias_path)
                            };
                            if sym_res.is_err() && !meta.is_dir {
                                let _ = std::fs::copy(&meta.path, &alias_path);
                            }
                        }
                        guard.logs.push(format!("[ALIAS] -> {alias_path}"));
                        let toast = format!("Created symlink for {}", meta.name);
                        guard.show_toast("success", toast);
                        guard.invalidate_dir_cache();
                        guard.reload_pane_entries(is_right);
                        guard.select_single_path_or_first(is_right, Some(&alias_path));
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
            guard.pending_rename_click = None;
            guard.last_row_click = None;
            let Some(meta) = guard.selected_metadata().cloned() else {
                return;
            };
            if let Some(w) = win_weak.upgrade() {
                reset_slint_modifiers(&w);
                let title = format!("Rename \"{}\"", meta.name);
                w.set_input_modal_mode("rename".into());
                w.set_input_modal_title(title.into());
                w.set_input_modal_value(meta.name.into());
                w.set_show_input_modal(true);
                w.invoke_focus_input_modal();
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
                if guard.vfs_create_dir(&new_dir).is_ok() {
                    guard.logs.push(format!("[MKDIR] Created {new_dir}"));
                    let toast = format!("Created folder \"{trimmed}\"");
                    guard.show_toast("success", toast);
                    guard.reload_pane_entries(is_right);
                    guard.select_single_path_or_first(is_right, Some(&new_dir));
                }
            } else if mode == "new_file" {
                let cur_dir = guard.current_pane_path(is_right);
                let new_file = join_vfs_path(&cur_dir, trimmed);
                if guard.vfs_write_bytes(&new_file, b"").is_ok() {
                    guard.logs.push(format!("[TOUCH] Created {new_file}"));
                    let toast = format!("Created file \"{trimmed}\"");
                    guard.show_toast("success", toast);
                    guard.reload_pane_entries(is_right);
                    guard.select_single_path_or_first(is_right, Some(&new_file));
                }
            } else if mode == "rename" {
                let Some(meta) = guard.selected_metadata().cloned() else {
                    return;
                };
                let parent = vfs_parent_dir(&meta.path).unwrap_or_else(|| "/".to_string());
                let dst = join_vfs_path(&parent, trimmed);
                if guard.vfs_rename(&meta.path, &dst).is_ok() {
                    let msg = format!("[RENAME] {} -> {trimmed}", meta.name);
                    guard.logs.push(msg);
                    let toast = format!("Renamed to \"{trimmed}\"");
                    guard.show_toast("success", toast);
                    guard.reload_pane_entries(false);
                    guard.reload_pane_entries(true);
                    guard.select_single_path_or_first(is_right, Some(&dst));
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
            let local_target = if target.starts_with("sftp://")
                || target.starts_with("s3://")
                || target.starts_with("smb://")
                || target.starts_with("ftp://")
                || target.starts_with("webdav://")
            {
                let mut staged = None;
                if let Some(meta) = guard.selected_metadata().cloned() {
                    let staged_vec = guard.prepare_local_paths_for_external_drag(&[meta]);
                    staged = staged_vec.into_iter().next();
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
            let first_name = sel_list[0].name.clone();

            for meta in &sel_list {
                total_bytes += meta.size.unwrap_or(4096);
                if permanent {
                    if guard.vfs_remove(&meta.path, true).is_ok() {
                        let msg = format!("[DELETE] Permanently removed {}", meta.name);
                        guard.logs.push(msg);
                        deleted_count += 1;
                    }
                } else {
                    let res = if split_remote_uri(&meta.path).is_some() {
                        guard
                            .vfs_remove(&meta.path, true)
                            .map(|()| "Removed on remote".to_string())
                    } else {
                        move_path_to_trash(&guard.local_vfs, &meta.path)
                    };
                    if let Ok(dst) = res {
                        let msg = format!("[TRASH] {} -> {dst}", meta.name);
                        guard.logs.push(msg);
                        deleted_count += 1;
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
            guard
                .transfers
                .retain(|t| t.status == TransferStatus::InProgress);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
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
            let res = guard.resolve_drop_target(from_right, &items, coords, bounds);
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
            let res = guard.resolve_drop_target(from_right, &items, coords, bounds);
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

    // Column sorting (Name | Size | Date Modified)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_sort_by_column(move |is_right, col_str| {
            let mut guard = rt.borrow_mut();
            let target_col = match col_str.as_str() {
                "size" => SortColumn::Size,
                "modified" => SortColumn::Modified,
                _ => SortColumn::Name,
            };
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

    // Interactive 3x3 R/W/X Permissions Matrix toggle
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
                let new_mode = matrix.to_mode();
                let _ = guard.mounted_vfs.set_permissions(&meta.path, new_mode);
                let _ = guard.local_vfs.set_permissions(&meta.path, new_mode);
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
                    let _ = guard.mounted_vfs.set_color_label(&meta.path, label);
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
                let _ = guard.mounted_vfs.set_hide_extension(&meta.path, next);
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
                let _ = guard.mounted_vfs.set_locked(&meta.path, next);
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

    // Calculate recursive directory size ("--" -> "4 KB")
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_calculate_selected_dir_size(move || {
            let mut guard = rt.borrow_mut();
            if let Some(meta) = guard.selected_metadata().cloned() {
                let vfs = guard.backend_for_path(&meta.path);
                let total = vfs.calculate_dir_size(&meta.path).unwrap_or(0);
                let formatted = manifold_vfs::format_bytes_comma(total);
                if let Some(vm) = &mut guard.app_state.inspector_vm {
                    vm.general.size = formatted.clone();
                    vm.subtitle = format!("{} - {}", meta.kind, formatted);
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
                    if let Ok(dup_path) = duplicate_path_on_disk(&guard.local_vfs, meta) {
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

    // Batch Rename callback
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_batch_rename_selected(move || {
            let guard = rt.borrow();
            let Some(meta) = guard.selected_metadata() else {
                return;
            };
            if let Some(w) = win_weak.upgrade() {
                let title = format!("Rename \"{}\"", meta.name);
                w.set_input_modal_mode("rename".into());
                w.set_input_modal_title(title.into());
                w.set_input_modal_value(meta.name.clone().into());
                w.set_show_input_modal(true);
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

    // Refresh (Single-Pane) or Directory Sync (Dual-Pane)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_sync_panes(move || {
            let mut guard = rt.borrow_mut();
            if guard.app_state.dual_pane_enabled {
                let l_path = guard.current_pane_path(false);
                let r_path = guard.current_pane_path(true);
                let plan = SyncExecutionPlan::build(
                    &guard.local_vfs,
                    &l_path,
                    &guard.local_vfs,
                    &r_path,
                    SyncStrategy::LeftToRight,
                    ComparisonMode::ContentHash,
                );
                if let Ok(plan) = plan {
                    let _ = plan.execute(&guard.local_vfs, &guard.local_vfs);
                    let count = plan.entries.len();
                    let msg = format!("[SYNC] Synchronized {count} entries.");
                    guard.logs.push(msg);
                    let label = format!("Synchronized {count} entries");
                    guard.start_tracked_task(&label, &r_path, 8192, label.clone(), "success");
                }
            } else {
                let cur_dir = guard.current_pane_path(false);
                let title = guard.format_tab_title(&cur_dir);
                let label = format!("Refreshed {title}");
                guard.start_tracked_task(&label, &cur_dir, 4096, label.clone(), "info");
            }
            guard.invalidate_dir_cache();
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_tab_preview(true);
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
            std::thread::sleep(Duration::from_millis(12));
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
            std::thread::sleep(Duration::from_millis(25));
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
                std::thread::sleep(Duration::from_millis(8));
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
            let normalized = clip_text.replace("\r\n", "\r").replace('\n', "\r");
            let payload = if guard.term_emulator.bracketed_paste() {
                format!("\x1b[200~{normalized}\x1b[201~").into_bytes()
            } else {
                normalized.into_bytes()
            };
            if let Some(shell) = &guard.shell_session {
                let _ = shell.send_bytes(&payload);
            }
            std::thread::sleep(Duration::from_millis(12));
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
            std::thread::sleep(Duration::from_millis(15));
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
            let parsed_port = port_str.parse::<u16>().unwrap_or(22);
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
                    let toast = format!("Saved \"{display_label}\" (offline: {host_name})");
                    guard.show_toast("info", toast);
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

    // 45ms UI Feedback Timer: drives the Tasks circular-arrow spinner, progress bar, and Toast HUD
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
                        let is_ok = err_msg.is_none() && ok_count > 0;
                        for task in &mut guard.transfers {
                            if task.id == task_id {
                                if is_ok {
                                    task.transferred_bytes = task.total_bytes;
                                    task.status = TransferStatus::Completed;
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
                        let toast_kind = if is_ok { "success" } else { "info" };
                        guard.show_toast(toast_kind, toast_msg);
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

            if guard.toast_ticks > 0 {
                guard.toast_ticks -= 1;
                if guard.toast_ticks == 0 {
                    w.set_show_toast(false);
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
            if guard.session_save_tick.is_multiple_of(64) && !need_full_sync {
                guard.capture_and_save_session(&w);
            }

            if need_full_sync {
                sync_window_from_runtime(&w, &mut guard);
            } else if need_transfer_sync {
                let tr_rows: Vec<TransferRowSlint> = guard
                    .transfers
                    .iter()
                    .map(|t| TransferRowSlint {
                        id: SharedString::from(&t.id),
                        priority_label: match t.priority {
                            TransferPriority::Foreground => SharedString::from("Foreground"),
                            TransferPriority::Background => SharedString::from("Background"),
                        },
                        source_name: SharedString::from(&t.source_path),
                        dest_path: SharedString::from(&t.destination_path),
                        status_text: SharedString::from(format!("{:?}", t.status)),
                        progress_pct: (t.progress_fraction() * 100.0) as i32,
                    })
                    .collect();
                w.set_transfer_rows(ModelRc::new(VecModel::from(tr_rows)));
            }
        });
    }

    let run_res = window.run().map_err(|e| e.to_string());
    rt.borrow().capture_and_save_session(&window);
    run_res
}

fn prompt_delete_confirmation(w: &ManifoldWindow, guard: &mut GuiRuntime, permanent: bool) {
    let is_right = guard.is_right_effective();
    let sel_list = guard.selected_metadata_list(is_right);
    let Some(first) = sel_list.first() else {
        return;
    };
    guard.pending_delete_permanent = permanent;
    let count = sel_list.len();
    let (title, msg) = if permanent {
        if count == 1 {
            (
                format!("Permanently delete \"{}\"?", first.name),
                "This item will be deleted immediately. You cannot undo this action.".to_string(),
            )
        } else {
            (
                format!("Permanently delete {count} selected items?"),
                "These items will be deleted immediately. You cannot undo this action.".to_string(),
            )
        }
    } else if count == 1 {
        (
            format!("Move \"{}\" to Trash?", first.name),
            "This item will be moved to the system Trash / Bin.".to_string(),
        )
    } else {
        (
            format!("Move {count} selected items to Trash?"),
            "These items will be moved to the system Trash / Bin.".to_string(),
        )
    };
    play_system_sound("info");
    w.set_delete_modal_permanent(permanent);
    w.set_delete_modal_title(title.into());
    w.set_delete_modal_message(msg.into());
    w.set_show_delete_modal(true);
}

fn reset_connect_modal_for_new(w: &ManifoldWindow) {
    w.set_connect_editing_id(SharedString::default());
    w.set_connect_name(SharedString::default());
    w.set_connect_protocol("SFTP".into());
    w.set_connect_server(SharedString::default());
    w.set_connect_port("22".into());
    w.set_connect_username(SharedString::default());
    w.set_connect_auth_method("password".into());
    w.set_connect_password(SharedString::default());
    w.set_connect_key_path(SharedString::default());
    w.set_connect_passphrase(SharedString::default());
    w.set_connect_remote_path(SharedString::default());
    w.set_connect_local_path(SharedString::default());
    w.set_connect_base_url(SharedString::default());
    w.set_connect_encoding(SharedString::default());
    w.set_connect_note(SharedString::default());
    w.set_connect_s3_endpoint(SharedString::default());
    w.set_connect_s3_bucket(SharedString::default());
    w.set_connect_s3_region(SharedString::default());
    w.set_connect_s3_access_key(SharedString::default());
    w.set_connect_s3_secret_key(SharedString::default());
    w.set_connect_smb_share(SharedString::default());
    w.set_connect_smb_domain(SharedString::default());
    w.set_connect_color_index(0);
    w.set_connect_save_credentials(true);
}

fn populate_connect_modal_from_profile(
    w: &ManifoldWindow,
    guard: &GuiRuntime,
    prof: &ConnectionProfile,
) {
    reset_connect_modal_for_new(w);
    let proto_str = match prof.protocol {
        ProtocolType::S3 => "S3",
        ProtocolType::Smb => "SMB",
        ProtocolType::Ftp => "FTP",
        ProtocolType::WebDav => "WebDAV",
        _ => "SFTP",
    };
    let host_s = prof.host.clone().unwrap_or_default();
    let default_port = match prof.protocol {
        ProtocolType::S3 | ProtocolType::WebDav => 443,
        ProtocolType::Smb => 445,
        ProtocolType::Ftp => 21,
        _ => 22,
    };
    let port_s = prof.port.unwrap_or(default_port).to_string();
    let key_s = prof.key_path.clone().unwrap_or_default();
    let auth_s = match prof.auth_type {
        AuthType::PublicKey => "private_key",
        _ => "password",
    };
    let saved_sec = guard
        .credential_store
        .get_secret(&prof.id)
        .ok()
        .flatten()
        .unwrap_or_default();

    w.set_connect_editing_id(prof.id.clone().into());
    w.set_connect_name(prof.name.clone().into());
    w.set_connect_protocol(proto_str.into());
    w.set_connect_server(host_s.into());
    w.set_connect_port(port_s.into());
    w.set_connect_username(prof.username.clone().into());
    w.set_connect_auth_method(auth_s.into());
    w.set_connect_key_path(key_s.into());
    w.set_connect_password(saved_sec.clone().into());
    w.set_connect_passphrase(saved_sec.clone().into());
    w.set_connect_remote_path(prof.base_path.clone().into());
    let loc_s = prof.local_path.clone().unwrap_or_default();
    let url_s = prof.base_url.clone().unwrap_or_default();
    w.set_connect_local_path(loc_s.into());
    w.set_connect_base_url(url_s.into());
    if prof.protocol == ProtocolType::Ftp {
        w.set_connect_encoding(prof.text_encoding.clone().into());
    }
    w.set_connect_note(prof.note.clone().into());
    w.set_connect_show_advanced(prof.show_advanced);
    let color_idx = color_label_to_index(prof.color_label);
    w.set_connect_color_index(color_idx);
    if prof.protocol == ProtocolType::S3 {
        let bkt_s = prof.bucket.clone().unwrap_or_default();
        let reg_s = prof.region.clone().unwrap_or_default();
        let end_s = prof.endpoint.clone().unwrap_or_default();
        w.set_connect_s3_bucket(bkt_s.into());
        w.set_connect_s3_region(reg_s.into());
        w.set_connect_s3_endpoint(end_s.into());
        w.set_connect_s3_access_key(prof.username.clone().into());
        w.set_connect_s3_secret_key(saved_sec.into());
    } else if prof.protocol == ProtocolType::Smb {
        let trimmed = prof.base_path.trim_matches('/');
        if let Some((share, sub)) = trimmed.split_once('/') {
            w.set_connect_smb_share(share.into());
            w.set_connect_remote_path(format!("/{sub}").into());
        } else {
            w.set_connect_smb_share(trimmed.into());
            w.set_connect_remote_path(SharedString::default());
        }
    }
}

fn expand_tilde_path(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let trimmed = home.trim_end_matches('/');
        if raw == "~" {
            return home;
        }
        let suffix = raw.strip_prefix("~/").unwrap_or("");
        return format!("{trimmed}/{suffix}");
    }
    raw.to_string()
}

fn complete_directory_prefix(vfs: &LocalFsBackend, input: &str) -> Option<String> {
    let path = Path::new(input);
    let (parent_dir, prefix) = if input.ends_with('/') {
        (input.to_string(), String::new())
    } else {
        let par = path
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("/");
        let pref = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        (par.to_string(), pref.to_lowercase())
    };
    let entries = vfs.list_directory(&parent_dir).ok()?;
    let mut dirs: Vec<FileMetadata> = entries
        .into_iter()
        .filter(|m| m.is_dir && m.name.to_lowercase().starts_with(&prefix))
        .collect();
    dirs.sort_by_key(|m| m.name.to_lowercase());
    let first = dirs.into_iter().next()?;
    Some(format!("{}/", first.path.trim_end_matches('/')))
}

fn handle_keyboard_shortcut(
    w: &ManifoldWindow,
    rt: &Rc<RefCell<GuiRuntime>>,
    key: &str,
    mods: (bool, bool, bool),
) -> bool {
    let (cmd, alt, shift) = mods;
    let k_low = key.to_lowercase();

    // Escape closes active popovers, modals, hover preview, path edit mode, or terminal drawer
    if key == "\u{001b}" {
        let had_overlay = w.get_show_app_menu()
            || w.get_show_about_modal()
            || w.get_show_context_menu()
            || w.get_show_sidebar_context_menu()
            || w.get_show_open_with_modal()
            || w.get_show_sibling_dropdown()
            || w.get_show_tasks_popover()
            || w.get_show_preview_minion()
            || w.get_show_hover_preview()
            || w.get_show_preferences_modal()
            || w.get_show_input_modal()
            || w.get_show_delete_modal()
            || w.get_show_connect_modal()
            || w.get_show_conflict_modal()
            || w.get_left_path_edit_mode()
            || w.get_right_path_edit_mode();
        w.set_show_app_menu(false);
        w.set_show_about_modal(false);
        w.set_show_context_menu(false);
        w.set_show_sidebar_context_menu(false);
        w.set_show_open_with_modal(false);
        w.set_show_sibling_dropdown(false);
        w.set_show_tasks_popover(false);
        w.set_show_preview_minion(false);
        w.set_show_hover_preview(false);
        w.set_show_preferences_modal(false);
        w.set_show_input_modal(false);
        w.set_show_delete_modal(false);
        w.set_show_connect_modal(false);
        if w.get_show_conflict_modal() {
            rt.borrow_mut().pending_batch = None;
            w.set_conflict_apply_all(false);
        }
        w.set_show_conflict_modal(false);
        w.set_left_path_edit_mode(false);
        w.set_right_path_edit_mode(false);
        if !had_overlay {
            if w.get_show_terminal_drawer() {
                w.set_show_terminal_drawer(false);
            } else if !w.get_search_query().is_empty() {
                let is_right = rt.borrow().is_right_effective();
                w.set_search_query(SharedString::default());
                w.invoke_clear_pane_filter(is_right);
            }
        }
        w.invoke_focus_main_key_router();
        return true;
    }

    // Ctrl+, / Cmd+,: Open Preferences & Themes Modal
    if cmd && !alt && key == "," {
        w.set_show_app_menu(false);
        w.set_show_preferences_modal(true);
        return true;
    }

    // Enter confirms the Delete Modal when open
    if w.get_show_delete_modal() && (key == "\n" || key == "\r") {
        w.set_show_delete_modal(false);
        w.invoke_confirm_delete_modal();
        return true;
    }

    // Tab switches active pane or cycles tabs (`Ctrl+Tab` / `Ctrl+Shift+Tab`)
    if key == "\t" {
        let mut guard = rt.borrow_mut();
        if cmd {
            let is_right = guard.is_right_effective();
            let count = if is_right {
                guard.app_state.right_tabs.len()
            } else {
                guard.app_state.left_tabs.len()
            };
            if count > 1 {
                let cur = if is_right {
                    guard.app_state.right_active_tab
                } else {
                    guard.app_state.left_active_tab
                };
                let next = if shift {
                    (cur + count - 1) % count
                } else {
                    (cur + 1) % count
                };
                if is_right {
                    guard.app_state.right_active_tab = next;
                } else {
                    guard.app_state.left_active_tab = next;
                }
                guard.reload_pane_entries(is_right);
                sync_window_from_runtime(w, &mut guard);
            }
            return true;
        }
        if guard.app_state.dual_pane_enabled {
            guard.app_state.active_pane = match guard.app_state.active_pane {
                ActivePane::Left => ActivePane::Right,
                ActivePane::Right => ActivePane::Left,
            };
            guard.refresh_headers_and_inspector();
            sync_window_from_runtime(w, &mut guard);
            return true;
        }
    }

    // Cmd/Ctrl + A: Select All in active pane
    if cmd && !alt && !shift && k_low == "a" {
        let mut guard = rt.borrow_mut();
        let is_right = guard.is_right_effective();
        let len = if is_right {
            guard.right_entries.len()
        } else {
            guard.left_entries.len()
        };
        if len > 0 {
            if is_right {
                guard.right_selected_indices = (0..len).collect();
                guard.right_selected_idx = Some(0);
            } else {
                guard.left_selected_indices = (0..len).collect();
                guard.left_selected_idx = Some(0);
            }
            let toast = format!("Selected all {len} items");
            guard.show_toast("info", toast);
            guard.refresh_headers_and_inspector();
            sync_window_from_runtime(w, &mut guard);
        }
        return true;
    }

    // Classic File Manager Clipboard & Action Shortcuts (`Cmd/Ctrl + C/X/V/D/N/R/I`)
    if cmd && !alt && k_low == "c" {
        if shift {
            w.invoke_execute_context_action("copy_path".into());
        } else {
            w.invoke_execute_context_action("copy".into());
        }
        return true;
    }
    if cmd && !alt && !shift && k_low == "x" {
        w.invoke_execute_context_action("cut".into());
        return true;
    }
    if cmd && !alt && !shift && k_low == "v" {
        w.invoke_execute_context_action("paste".into());
        return true;
    }
    if cmd && !alt && !shift && k_low == "d" {
        w.invoke_execute_context_action("duplicate".into());
        return true;
    }
    if cmd && !alt && k_low == "n" {
        w.invoke_open_new_folder_modal();
        return true;
    }
    if cmd && !alt && !shift && k_low == "r" {
        w.invoke_sync_panes();
        return true;
    }
    if cmd && !alt && !shift && k_low == "i" {
        w.invoke_execute_context_action("get_info".into());
        return true;
    }

    // Alt+Left / Cmd+[ and Alt+Right / Cmd+]: Back / Forward History
    if (alt && key == "\u{f702}") || (cmd && key == "[") {
        w.invoke_go_back();
        return true;
    }
    if (alt && key == "\u{f703}") || (cmd && key == "]") {
        w.invoke_go_forward();
        return true;
    }

    // Ctrl/Cmd + 1..4: Switch View Mode (1=Icons, 2=List, 3=Columns, 4=Tree)
    if cmd && !alt && !shift && matches!(key, "1" | "2" | "3" | "4") {
        let mode_int = match key {
            "1" => 1,
            "2" => 0,
            "3" => 2,
            _ => 3,
        };
        w.invoke_set_view_mode(mode_int);
        return true;
    }

    // Ctrl+Alt+T / Cmd+Option+T: Toggle Single / Dual-Pane View
    if cmd && alt && k_low == "t" {
        w.invoke_toggle_dual_pane();
        return true;
    }

    // Ctrl+L / Cmd+L: Focus Path Bar (Edit Mode)
    if cmd && !alt && k_low == "l" {
        let guard = rt.borrow();
        let is_right = guard.is_right_effective();
        let cur = guard.current_pane_path(is_right);
        drop(guard);
        if is_right {
            w.set_right_path_input(cur.into());
            w.set_right_path_edit_mode(true);
        } else {
            w.set_left_path_input(cur.into());
            w.set_left_path_edit_mode(true);
        }
        return true;
    }

    // Ctrl+T / Cmd+T: New Tab in Active Pane
    if cmd && !alt && k_low == "t" {
        let is_right = rt.borrow().is_right_effective();
        w.invoke_add_tab(is_right);
        return true;
    }

    // Ctrl+W / Cmd+W: Close Active Tab
    if cmd && !alt && k_low == "w" {
        let (is_right, active_idx) = {
            let guard = rt.borrow();
            let r = guard.is_right_effective();
            let idx = if r {
                guard.app_state.right_active_tab
            } else {
                guard.app_state.left_active_tab
            };
            (r, idx as i32)
        };
        w.invoke_close_tab(is_right, active_idx);
        return true;
    }

    // Ctrl+H / Cmd+Shift+. : Toggle Hidden Files
    if (cmd && k_low == "h") || (cmd && shift && key == ".") {
        w.invoke_toggle_hidden_files();
        return true;
    }

    // Ctrl+K / Cmd+K: Open Connect to Server Modal
    if cmd && k_low == "k" {
        w.set_show_connect_modal(true);
        return true;
    }

    // Ctrl+F / Cmd+F: Focus inline filter box in top toolbar
    if cmd && k_low == "f" {
        w.invoke_focus_filter_box();
        return true;
    }

    // Space: Toggle Preview Minion (Quick Look HUD)
    if key == " " && !cmd && !alt {
        w.invoke_toggle_preview_minion();
        return true;
    }

    // Enter / Return or Cmd+Down: Open Selected Folder / Launch File
    if key == "\n" || key == "\r" || (cmd && key == "\u{f701}") {
        let is_right = rt.borrow().is_right_effective();
        w.invoke_open_file_row(is_right, -1);
        return true;
    }

    // Alt+Up / Backspace / Cmd+Up: Go to Parent Directory
    if key == "\u{0008}" || ((alt || cmd) && key == "\u{f700}") {
        if cmd && alt {
            w.invoke_permanent_delete_selected_item();
            return true;
        }
        if cmd && key == "\u{0008}" {
            w.invoke_delete_selected_item();
            return true;
        }
        let is_right = rt.borrow().is_right_effective();
        w.invoke_navigate_up(is_right);
        return true;
    }

    // Delete (`\u{007f}`): Move to Bin or Permanent Delete (`Shift+Delete`)
    if key == "\u{007f}" {
        if shift {
            w.invoke_permanent_delete_selected_item();
        } else {
            w.invoke_delete_selected_item();
        }
        return true;
    }

    // LeftArrow (`\u{f702}`) / RightArrow (`\u{f703}`): Enter folder / Parent dir or Icon Grid step
    if !cmd && !alt && (key == "\u{f702}" || key == "\u{f703}") {
        let mut guard = rt.borrow_mut();
        let is_right = guard.is_right_effective();
        let view_mode = guard.app_state.toolbar.active_view_mode;
        if view_mode == PaneViewMode::Icons {
            let len = if is_right {
                guard.right_entries.len()
            } else {
                guard.left_entries.len()
            };
            if len > 0 {
                let cur = if is_right {
                    guard.right_selected_idx.unwrap_or(0)
                } else {
                    guard.left_selected_idx.unwrap_or(0)
                };
                let next = if key == "\u{f702}" {
                    cur.saturating_sub(1)
                } else {
                    (cur + 1).min(len - 1)
                };
                guard.apply_row_selection(is_right, next, shift, false);
                guard.refresh_headers_and_inspector();
                sync_selection_and_inspector(w, &mut guard);
                drop(guard);
                w.invoke_ensure_row_visible(is_right, next as i32);
            }
            return true;
        }
        if view_mode == PaneViewMode::Tree {
            let cur_opt = if is_right {
                guard.right_selected_idx
            } else {
                guard.left_selected_idx
            };
            if let Some(cur) = cur_opt {
                let row_info = if is_right {
                    guard
                        .right_entries
                        .get(cur)
                        .map(|r| (is_browsable_folder(&r.meta), r.is_expanded, r.depth))
                } else {
                    guard
                        .left_entries
                        .get(cur)
                        .map(|r| (is_browsable_folder(&r.meta), r.is_expanded, r.depth))
                };
                if let Some((is_dir, is_expanded, depth)) = row_info {
                    if key == "\u{f703}" {
                        if is_dir && !is_expanded {
                            drop(guard);
                            w.invoke_toggle_expand_folder(is_right, cur as i32);
                        } else if is_dir && is_expanded {
                            let len = if is_right {
                                guard.right_entries.len()
                            } else {
                                guard.left_entries.len()
                            };
                            let next = (cur + 1).min(len.saturating_sub(1));
                            guard.apply_row_selection(is_right, next, shift, false);
                            guard.refresh_headers_and_inspector();
                            sync_selection_and_inspector(w, &mut guard);
                            drop(guard);
                            w.invoke_ensure_row_visible(is_right, next as i32);
                        }
                        return true;
                    }
                    if is_dir && is_expanded {
                        drop(guard);
                        w.invoke_toggle_expand_folder(is_right, cur as i32);
                        return true;
                    }
                    if depth > 0 {
                        let parent_idx = if is_right {
                            (0..cur)
                                .rev()
                                .find(|&i| guard.right_entries[i].depth < depth)
                        } else {
                            (0..cur)
                                .rev()
                                .find(|&i| guard.left_entries[i].depth < depth)
                        };
                        if let Some(p_idx) = parent_idx {
                            guard.apply_row_selection(is_right, p_idx, shift, false);
                            guard.refresh_headers_and_inspector();
                            sync_selection_and_inspector(w, &mut guard);
                            drop(guard);
                            w.invoke_ensure_row_visible(is_right, p_idx as i32);
                            return true;
                        }
                    }
                    drop(guard);
                    w.invoke_navigate_up(is_right);
                    return true;
                }
            }
            return true;
        }
        if key == "\u{f702}" {
            drop(guard);
            w.invoke_navigate_up(is_right);
            return true;
        }
        let cur_opt = if is_right {
            guard.right_selected_idx
        } else {
            guard.left_selected_idx
        };
        if let Some(cur) = cur_opt {
            let is_dir = if is_right {
                guard
                    .right_entries
                    .get(cur)
                    .is_some_and(|r| is_browsable_folder(&r.meta))
            } else {
                guard
                    .left_entries
                    .get(cur)
                    .is_some_and(|r| is_browsable_folder(&r.meta))
            };
            if is_dir {
                guard.open_entry_at(is_right, Some(cur));
                sync_window_from_runtime(w, &mut guard);
            }
        }
        return true;
    }

    // UpArrow / DownArrow / Home / End / PageUp / PageDown: Move or Shift-extend selection
    if key == "\u{f700}"
        || key == "\u{f701}"
        || key == "\u{f729}"
        || key == "\u{f72b}"
        || key == "\u{f72c}"
        || key == "\u{f72d}"
    {
        let mut guard = rt.borrow_mut();
        let is_right = guard.is_right_effective();
        let view_mode = guard.app_state.toolbar.active_view_mode;
        let len = if is_right {
            guard.right_entries.len()
        } else {
            guard.left_entries.len()
        };
        if len > 0 {
            let cur = if is_right {
                guard.right_selected_idx.unwrap_or(0)
            } else {
                guard.left_selected_idx.unwrap_or(0)
            };
            let step = if view_mode == PaneViewMode::Icons {
                3
            } else {
                1
            };
            let next = match key {
                "\u{f700}" => cur.saturating_sub(step),
                "\u{f701}" => (cur + step).min(len - 1),
                "\u{f729}" => 0,
                "\u{f72b}" => len - 1,
                "\u{f72c}" => cur.saturating_sub(12),
                "\u{f72d}" => (cur + 12).min(len - 1),
                _ => cur,
            };
            guard.apply_row_selection(is_right, next, shift, false);
            guard.refresh_headers_and_inspector();
            sync_selection_and_inspector(w, &mut guard);
            drop(guard);
            w.invoke_ensure_row_visible(is_right, next as i32);
        }
        return true;
    }

    // F2 (`\u{f705}`): Inline Rename
    if key == "\u{f705}" {
        w.invoke_open_rename_modal();
        return true;
    }

    // F5 (`\u{f708}`): Copy Selected to Inactive Pane
    if key == "\u{f708}" {
        w.invoke_copy_selected_between_panes();
        return true;
    }

    // F6 (`\u{f709}`): Move Selected to Inactive Pane
    if key == "\u{f709}" {
        w.invoke_move_selected_between_panes();
        return true;
    }

    false
}

fn normalize_fav_target(raw: &str) -> String {
    let trimmed = raw.trim().strip_prefix("file://").unwrap_or(raw.trim());
    if trimmed == "/" {
        "/".to_string()
    } else {
        trimmed.trim_end_matches('/').to_string()
    }
}

fn push_unique_open_with_app(
    out: &mut Vec<OpenWithAppSlint>,
    id: &str,
    name: &str,
    detail: &str,
    icon_kind: &str,
) {
    if out.iter().any(|a| a.id.as_str() == id) {
        return;
    }
    out.push(OpenWithAppSlint {
        id: id.into(),
        name: name.into(),
        detail: detail.into(),
        icon_kind: icon_kind.into(),
    });
}

fn discover_open_with_apps(meta: &FileMetadata) -> Vec<OpenWithAppSlint> {
    let mut apps = Vec::new();
    let default_detail = if cfg!(target_os = "macos") {
        "System LaunchServices default handler (/usr/bin/open)"
    } else if cfg!(target_os = "windows") {
        "Windows Shell default handler (explorer)"
    } else {
        "Freedesktop MIME default handler (xdg-open)"
    };
    push_unique_open_with_app(
        &mut apps,
        "__DEFAULT__",
        "System Default Application",
        default_detail,
        "external-link",
    );

    let ext = meta
        .extension
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('.')
        .to_lowercase();
    let is_image = matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tiff" | "heic"
    );
    let is_media = matches!(
        ext.as_str(),
        "mp3" | "wav" | "flac" | "m4a" | "ogg" | "aac" | "mp4" | "mov" | "mkv" | "webm" | "avi"
    );
    let is_pdf_or_doc = matches!(
        ext.as_str(),
        "pdf" | "doc" | "docx" | "rtf" | "odt" | "pages" | "xls" | "xlsx" | "ppt" | "pptx"
    );
    let is_archive = matches!(
        ext.as_str(),
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "iso"
    );

    if cfg!(target_os = "macos") {
        let mut mac_candidates: Vec<(&str, &str, &str)> = Vec::new();
        if meta.is_dir {
            mac_candidates.push((
                "/System/Applications/Utilities/Terminal.app",
                "Terminal",
                "terminal",
            ));
            mac_candidates.push(("/Applications/iTerm.app", "iTerm", "terminal"));
            mac_candidates.push((
                "/Applications/Visual Studio Code.app",
                "Visual Studio Code",
                "code",
            ));
            mac_candidates.push(("/Applications/Zed.app", "Zed", "code"));
            mac_candidates.push(("/Applications/Cursor.app", "Cursor", "code"));
        } else {
            if is_image || is_pdf_or_doc {
                mac_candidates.push(("/System/Applications/Preview.app", "Preview", "image"));
            }
            if is_media {
                mac_candidates.push((
                    "/System/Applications/QuickTime Player.app",
                    "QuickTime Player",
                    "movies",
                ));
                mac_candidates.push(("/Applications/VLC.app", "VLC", "movies"));
                mac_candidates.push(("/Applications/IINA.app", "IINA", "movies"));
                mac_candidates.push(("/System/Applications/Music.app", "Music", "music"));
            }
            if is_archive {
                mac_candidates.push((
                    "/System/Library/CoreServices/Applications/Archive Utility.app",
                    "Archive Utility",
                    "archive",
                ));
                mac_candidates.push((
                    "/Applications/The Unarchiver.app",
                    "The Unarchiver",
                    "archive",
                ));
                mac_candidates.push(("/Applications/Keka.app", "Keka", "archive"));
            }
            mac_candidates.push((
                "/Applications/Visual Studio Code.app",
                "Visual Studio Code",
                "code",
            ));
            mac_candidates.push(("/Applications/Cursor.app", "Cursor", "code"));
            mac_candidates.push(("/Applications/Zed.app", "Zed", "code"));
            mac_candidates.push(("/Applications/Sublime Text.app", "Sublime Text", "code"));
            mac_candidates.push(("/System/Applications/TextEdit.app", "TextEdit", "documents"));
            mac_candidates.push(("/System/Applications/Preview.app", "Preview", "image"));
            mac_candidates.push(("/Applications/Google Chrome.app", "Google Chrome", "globe"));
            mac_candidates.push(("/Applications/Firefox.app", "Firefox", "globe"));
            mac_candidates.push(("/Applications/Safari.app", "Safari", "globe"));
            mac_candidates.push(("/Applications/LibreOffice.app", "LibreOffice", "documents"));
            mac_candidates.push((
                "/Applications/Microsoft Word.app",
                "Microsoft Word",
                "documents",
            ));
            mac_candidates.push((
                "/Applications/Microsoft Excel.app",
                "Microsoft Excel",
                "documents",
            ));
        }
        for (bundle_path, label, icon) in mac_candidates {
            if std::path::Path::new(bundle_path).exists() {
                push_unique_open_with_app(&mut apps, bundle_path, label, bundle_path, icon);
            }
        }
        if let Ok(entries) = std::fs::read_dir("/Applications") {
            let mut extra_apps: Vec<(String, String)> = entries
                .flatten()
                .filter_map(|e| {
                    let path = e.path();
                    let name = path.file_name()?.to_string_lossy().to_string();
                    if name.ends_with(".app") {
                        let label = name.trim_end_matches(".app").to_string();
                        Some((path.to_string_lossy().to_string(), label))
                    } else {
                        None
                    }
                })
                .collect();
            extra_apps.sort_by_key(|(_, label)| label.to_lowercase());
            for (path_str, label) in extra_apps.into_iter().take(18) {
                push_unique_open_with_app(&mut apps, &path_str, &label, &path_str, "folder");
            }
        }
    } else if cfg!(target_os = "windows") {
        let win_bins: [(&str, &str, &str, &str); 5] = [
            (
                "notepad.exe",
                "Notepad",
                "Windows Text Editor (notepad.exe)",
                "documents",
            ),
            ("code", "Visual Studio Code", "Code Editor (code)", "code"),
            (
                "explorer.exe",
                "Windows Explorer",
                "File Explorer (explorer.exe)",
                "folder",
            ),
            (
                "mspaint.exe",
                "Paint",
                "Image Editor (mspaint.exe)",
                "image",
            ),
            (
                "powershell.exe",
                "PowerShell",
                "Windows PowerShell",
                "terminal",
            ),
        ];
        for (bin, label, detail, icon) in win_bins {
            push_unique_open_with_app(&mut apps, bin, label, detail, icon);
        }
    } else {
        let linux_bins: [(&str, &str, &str); 18] = [
            ("code", "Visual Studio Code", "code"),
            ("codium", "VSCodium", "code"),
            ("zed", "Zed Editor", "code"),
            ("subl", "Sublime Text", "code"),
            ("gedit", "GNOME Text Editor (gedit)", "documents"),
            ("kate", "KDE Kate", "documents"),
            ("mousepad", "Mousepad", "documents"),
            ("nano", "GNU Nano (Terminal)", "terminal"),
            ("vim", "Vim (Terminal)", "terminal"),
            ("evince", "Document Viewer (Evince)", "documents"),
            ("okular", "Okular Document Viewer", "documents"),
            ("eog", "Image Viewer (Eye of GNOME)", "image"),
            ("feh", "Feh Image Viewer", "image"),
            ("gimp", "GNU Image Manipulation Program", "image"),
            ("vlc", "VLC Media Player", "movies"),
            ("mpv", "mpv Media Player", "movies"),
            ("libreoffice", "LibreOffice", "documents"),
            ("firefox", "Mozilla Firefox", "globe"),
        ];
        let search_dirs = ["/usr/bin", "/usr/local/bin", "/bin", "/snap/bin"];
        for (bin, label, icon) in linux_bins {
            for dir in search_dirs {
                let full = format!("{dir}/{bin}");
                if std::path::Path::new(&full).exists() {
                    push_unique_open_with_app(&mut apps, &full, label, &full, icon);
                    break;
                }
            }
        }
    }
    apps
}

fn launch_file_with_app(target_path: &str, app_spec: &str) -> Result<String, String> {
    let trimmed = app_spec.trim();
    if trimmed.is_empty() {
        return Err("No application specified".to_string());
    }
    if trimmed == "__DEFAULT__" {
        if cfg!(target_os = "macos") {
            sanitize_macos_app_bundle(Path::new(target_path), true);
        }
        #[cfg(target_os = "macos")]
        let status = Command::new("/usr/bin/open")
            .arg(target_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(target_os = "windows")]
        let status = Command::new("explorer")
            .arg(target_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let status = Command::new("xdg-open")
            .arg(target_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        return status
            .map(|_| "System Default Application".to_string())
            .map_err(|e| format!("Failed to launch system handler: {e}"));
    }

    #[cfg(target_os = "macos")]
    {
        let label = std::path::Path::new(trimmed)
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".app").to_string())
            .unwrap_or_else(|| trimmed.to_string());
        if trimmed.ends_with(".app") || !trimmed.contains('/') {
            return Command::new("/usr/bin/open")
                .args(["-a", trimmed, target_path])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|_| label)
                .map_err(|e| format!("Failed to open with {trimmed}: {e}"));
        }
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    let program = parts[0];
    let extra_args = &parts[1..];
    let label = std::path::Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string());
    Command::new(program)
        .args(extra_args)
        .arg(target_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| label)
        .map_err(|e| format!("Failed to execute {program}: {e}"))
}

fn is_extractable_archive(meta: &FileMetadata) -> bool {
    if meta.is_dir {
        return false;
    }
    let lower = meta.name.to_lowercase();
    let suffixes = [
        ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar", ".zip", ".7z", ".rar",
        ".gz", ".bz2", ".xz",
    ];
    suffixes.iter().any(|s| lower.ends_with(s))
}

fn archive_output_stem(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    let compound = [
        ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz", ".tar", ".zip", ".7z", ".rar",
        ".gz", ".bz2", ".xz",
    ];
    for suf in compound {
        if lower.ends_with(suf) && file_name.len() > suf.len() {
            let cut = file_name.len() - suf.len();
            return file_name[..cut].to_string();
        }
    }
    format!("{file_name}_extracted")
}

fn should_show_in_enclosing_folder(
    inline_filter: &str,
    main_folder_path: &str,
    selected: &[FileMetadata],
) -> bool {
    if inline_filter.trim().is_empty() || selected.len() != 1 {
        return false;
    }
    let Some(item_parent) = vfs_parent_dir(&selected[0].path) else {
        return false;
    };
    let norm_main = main_folder_path.trim_end_matches('/');
    let norm_parent = item_parent.trim_end_matches('/');
    let main_cmp = if norm_main.is_empty() { "/" } else { norm_main };
    let parent_cmp = if norm_parent.is_empty() {
        "/"
    } else {
        norm_parent
    };
    main_cmp != parent_cmp
}

fn extract_archive_into_dir(
    archive_path: &str,
    archive_name: &str,
    dest_dir: &str,
    stem: &str,
) -> Result<(), String> {
    let lower = archive_name.to_lowercase();
    let status = if lower.ends_with(".zip") {
        Command::new("unzip")
            .args(["-q", "-o", archive_path, "-d", dest_dir])
            .status()
            .or_else(|_| {
                Command::new("tar")
                    .args(["-xf", archive_path, "-C", dest_dir])
                    .status()
            })
    } else if lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".tbz2")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
        || lower.ends_with(".tar")
    {
        if cfg!(target_os = "macos") {
            Command::new("tar")
                .args(["--no-mac-metadata", "-xf", archive_path, "-C", dest_dir])
                .status()
                .or_else(|_| {
                    Command::new("tar")
                        .args(["-xf", archive_path, "-C", dest_dir])
                        .status()
                })
        } else {
            Command::new("tar")
                .args(["-xf", archive_path, "-C", dest_dir])
                .status()
        }
    } else if lower.ends_with(".7z") {
        let out_arg = format!("-o{dest_dir}");
        Command::new("7z")
            .args(["x", "-y", archive_path, &out_arg])
            .status()
    } else if lower.ends_with(".rar") {
        Command::new("unrar")
            .args(["x", "-y", archive_path, dest_dir])
            .status()
    } else if lower.ends_with(".gz") {
        let out_file = format!("{dest_dir}/{stem}");
        let cmd = format!("gzip -dc \"{archive_path}\" > \"{out_file}\"");
        Command::new("sh").args(["-c", &cmd]).status()
    } else if lower.ends_with(".bz2") {
        let out_file = format!("{dest_dir}/{stem}");
        let cmd = format!("bzip2 -dc \"{archive_path}\" > \"{out_file}\"");
        Command::new("sh").args(["-c", &cmd]).status()
    } else if lower.ends_with(".xz") {
        let out_file = format!("{dest_dir}/{stem}");
        let cmd = format!("xz -dc \"{archive_path}\" > \"{out_file}\"");
        Command::new("sh").args(["-c", &cmd]).status()
    } else {
        Command::new("tar")
            .args(["-xf", archive_path, "-C", dest_dir])
            .status()
    };

    match status {
        Ok(s) if s.success() => {
            sanitize_macos_app_bundle(Path::new(dest_dir), cfg!(target_os = "macos"));
            Ok(())
        }
        Ok(s) => Err(format!("Extractor exited with status {s}")),
        Err(e) => Err(format!("Failed to run archive extractor: {e}")),
    }
}

fn collect_macos_app_bundles(root: &Path) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let root_is_app = root
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".app"));
    if root_is_app {
        return vec![root.to_path_buf()];
    }
    let mut bundles = Vec::new();
    for child in significant_dir_children(root) {
        if !child.is_dir() {
            continue;
        }
        let is_app = child
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.to_ascii_lowercase().ends_with(".app"));
        if is_app {
            bundles.push(child);
            continue;
        }
        for sub in significant_dir_children(&child) {
            let sub_is_app = sub.is_dir()
                && sub
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase().ends_with(".app"));
            if sub_is_app {
                bundles.push(sub);
            }
        }
    }
    bundles
}

#[cfg(unix)]
fn ensure_macos_bundle_executables_chmod(app_dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let macos_bin_dir = app_dir.join("Contents").join("MacOS");
    let Ok(rd) = std::fs::read_dir(&macos_bin_dir) else {
        return;
    };
    for entry in rd.flatten() {
        let bin_path = entry.path();
        if let Ok(meta) = std::fs::symlink_metadata(&bin_path) {
            if meta.is_file() {
                let mode = meta.permissions().mode();
                if mode & 0o111 == 0 {
                    let perms = std::fs::Permissions::from_mode(mode | 0o755);
                    let _ = std::fs::set_permissions(&bin_path, perms);
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn ensure_macos_bundle_executables_chmod(_app_dir: &Path) {}

fn sanitize_macos_app_bundle(path: &Path, is_macos: bool) {
    if !path.exists() {
        return;
    }
    let bundles = collect_macos_app_bundles(path);
    for app_dir in &bundles {
        ensure_macos_bundle_executables_chmod(app_dir);
    }
    if !is_macos || !cfg!(target_os = "macos") {
        return;
    }
    let root_str = path.to_string_lossy().to_string();
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine", &root_str])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.provenance", &root_str])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    for app_dir in &bundles {
        let app_str = app_dir.to_string_lossy().to_string();
        let _ = Command::new("/usr/bin/xattr")
            .args(["-dr", "com.apple.quarantine", &app_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let verify_ok = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", &app_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !verify_ok {
            let _ = Command::new("/usr/bin/xattr")
                .args(["-cr", &app_str])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = Command::new("/usr/bin/codesign")
                .args(["--force", "--deep", "--sign", "-", &app_str])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn copy_local_dir_preserving_bundle(src_path: &str, dst_path: &str) -> Result<(), String> {
    let dst_pb = Path::new(dst_path);
    let parent_dir = dst_pb.parent().unwrap_or_else(|| Path::new("/"));
    let dst_name = dst_pb
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bundle");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let stage_wrap_name = format!(".manifold-stage-{}-{nanos}", std::process::id());
    let stage_wrap = parent_dir.join(stage_wrap_name);
    if stage_wrap.exists() {
        let _ = std::fs::remove_dir_all(&stage_wrap);
    }
    std::fs::create_dir_all(&stage_wrap)
        .map_err(|e| format!("Failed to create staging dir: {e}"))?;
    let stage_dst = stage_wrap.join(dst_name);
    let stage_dst_str = stage_dst.to_string_lossy().to_string();

    let copied_ok = if cfg!(target_os = "macos") {
        let ditto_ok = Command::new("/usr/bin/ditto")
            .args(["--noqtn", src_path, &stage_dst_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ditto_ok {
            true
        } else {
            Command::new("cp")
                .args(["-RPp", src_path, &stage_dst_str])
                .status()
                .is_ok_and(|s| s.success())
        }
    } else {
        let cp_p_ok = Command::new("cp")
            .args(["-RPp", src_path, &stage_dst_str])
            .status()
            .is_ok_and(|s| s.success());
        if cp_p_ok {
            true
        } else {
            Command::new("cp")
                .args(["-R", src_path, &stage_dst_str])
                .status()
                .is_ok_and(|s| s.success())
        }
    };

    if !copied_ok {
        let _ = std::fs::remove_dir_all(&stage_wrap);
        return Err("Failed to copy directory".to_string());
    }

    sanitize_macos_app_bundle(&stage_dst, cfg!(target_os = "macos"));

    if dst_pb.exists() || std::fs::symlink_metadata(dst_pb).is_ok() {
        let _ = std::fs::remove_dir_all(dst_pb);
        let _ = std::fs::remove_file(dst_pb);
    }
    if let Err(e) = std::fs::rename(&stage_dst, dst_pb) {
        let _ = std::fs::remove_dir_all(&stage_wrap);
        return Err(format!("Failed to finalize directory copy: {e}"));
    }
    let _ = std::fs::remove_dir_all(&stage_wrap);
    sanitize_macos_app_bundle(dst_pb, cfg!(target_os = "macos"));
    Ok(())
}

fn significant_dir_children(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            name != ".DS_Store" && name != "__MACOSX"
        })
        .collect()
}

fn find_single_macos_app_in_extracted_dir(extracted_dir: &Path) -> Option<PathBuf> {
    let top = significant_dir_children(extracted_dir);
    if top.len() != 1 {
        return None;
    }
    let first = &top[0];
    if !first.is_dir() {
        return None;
    }
    let first_name = first.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if first_name.to_ascii_lowercase().ends_with(".app") {
        return Some(first.clone());
    }
    let nested = significant_dir_children(first);
    if nested.len() != 1 || !nested[0].is_dir() {
        return None;
    }
    let nested_name = nested[0].file_name().and_then(|s| s.to_str()).unwrap_or("");
    if nested_name.to_ascii_lowercase().ends_with(".app") {
        return Some(nested[0].clone());
    }
    None
}

fn finalize_extracted_archive_dir(
    dest_dir: &str,
    trimmed_parent: &str,
    is_macos: bool,
) -> Result<String, String> {
    if !is_macos {
        return Ok(dest_dir.to_string());
    }
    let Some(app_path) = find_single_macos_app_in_extracted_dir(Path::new(dest_dir)) else {
        sanitize_macos_app_bundle(Path::new(dest_dir), is_macos);
        return Ok(dest_dir.to_string());
    };
    sanitize_macos_app_bundle(&app_path, is_macos);
    let app_name = app_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("App.app");
    let app_stem = app_name
        .strip_suffix(".app")
        .or_else(|| app_name.strip_suffix(".APP"))
        .unwrap_or(app_name);
    let mut final_app = format!("{trimmed_parent}/{app_name}");
    let mut counter = 2usize;
    while Path::new(&final_app).exists() {
        final_app = format!("{trimmed_parent}/{app_stem} {counter}.app");
        counter += 1;
    }
    if std::fs::rename(&app_path, &final_app).is_ok() {
        let _ = std::fs::remove_dir_all(dest_dir);
        sanitize_macos_app_bundle(Path::new(&final_app), is_macos);
        return Ok(final_app);
    }
    sanitize_macos_app_bundle(Path::new(dest_dir), is_macos);
    Ok(dest_dir.to_string())
}

fn extract_archive_to_folder(meta: &FileMetadata) -> Result<String, String> {
    extract_archive_to_folder_for_os(meta, cfg!(target_os = "macos"))
}

fn extract_archive_to_folder_for_os(meta: &FileMetadata, is_macos: bool) -> Result<String, String> {
    if meta.path.contains("://") {
        return Err("Remote archive extraction requires downloading first".to_string());
    }
    let parent = Path::new(&meta.path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("/tmp");
    let trimmed_parent = parent.trim_end_matches('/');
    let stem = archive_output_stem(&meta.name);
    let mut dest_dir = format!("{trimmed_parent}/{stem}");
    let mut counter = 2usize;
    while Path::new(&dest_dir).exists() {
        dest_dir = format!("{trimmed_parent}/{stem} {counter}");
        counter += 1;
    }
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("Failed to create {dest_dir}: {e}"))?;

    if let Err(e) = extract_archive_into_dir(&meta.path, &meta.name, &dest_dir, &stem) {
        let _ = std::fs::remove_dir_all(&dest_dir);
        return Err(e);
    }
    finalize_extracted_archive_dir(&dest_dir, trimmed_parent, is_macos)
}

fn open_archive_contents_dir(meta: &FileMetadata, is_macos: bool) -> Result<String, String> {
    if meta.path.contains("://") {
        return Err("Remote archive inspection requires downloading first".to_string());
    }
    let stem = archive_output_stem(&meta.name);
    let base_dir = std::env::temp_dir().join("manifold-archive-contents");
    let stage_dir = base_dir.join(&stem);
    if stage_dir.exists() {
        let _ = std::fs::remove_dir_all(&stage_dir);
    }
    let stage_str = stage_dir.to_string_lossy().to_string();
    std::fs::create_dir_all(&stage_dir)
        .map_err(|e| format!("Failed to create {stage_str}: {e}"))?;
    extract_archive_into_dir(&meta.path, &meta.name, &stage_str, &stem)?;
    let _ = std::fs::remove_dir_all(stage_dir.join("__MACOSX"));
    if is_macos {
        unwrap_nested_macos_app_in_stage_dir(&stage_dir);
    }
    sanitize_macos_app_bundle(&stage_dir, is_macos);
    Ok(stage_str)
}

fn unwrap_nested_macos_app_in_stage_dir(stage_dir: &Path) {
    let top = significant_dir_children(stage_dir);
    if top.len() != 1 || !top[0].is_dir() {
        return;
    }
    let top_name = top[0].file_name().and_then(|s| s.to_str()).unwrap_or("");
    if top_name.to_ascii_lowercase().ends_with(".app") {
        return;
    }
    let nested = significant_dir_children(&top[0]);
    if nested.len() != 1 || !nested[0].is_dir() {
        return;
    }
    let n_name = nested[0].file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !n_name.to_ascii_lowercase().ends_with(".app") {
        return;
    }
    let target_app = stage_dir.join(n_name);
    if std::fs::rename(&nested[0], &target_app).is_ok() {
        let _ = std::fs::remove_dir_all(&top[0]);
    }
}

fn current_iso_date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}")
}

fn default_compress_stem(sel_list: &[FileMetadata]) -> String {
    if sel_list.len() == 1 {
        let first = &sel_list[0];
        if !first.is_dir {
            if let Some((stem, _)) = first.name.rsplit_once('.') {
                if !stem.is_empty() {
                    return stem.to_string();
                }
            }
        }
        if !first.name.is_empty() {
            return first.name.clone();
        }
    }
    let today = current_iso_date_string();
    format!("compressed-file-{today}")
}

fn normalize_archive_base_name(raw_name: &str, format: &str) -> String {
    let trimmed = raw_name.trim();
    let lower = trimmed.to_lowercase();
    let suffix = format!(".{}", format.trim_start_matches('.').to_lowercase());
    if lower.ends_with(&suffix) && trimmed.len() > suffix.len() {
        let cut = trimmed.len() - suffix.len();
        trimmed[..cut].to_string()
    } else if format == "tar.gz" && lower.ends_with(".tgz") && trimmed.len() > 4 {
        let cut = trimmed.len() - 4;
        trimmed[..cut].to_string()
    } else {
        trimmed.to_string()
    }
}

fn create_archive_from_selection(
    sel_list: &[FileMetadata],
    raw_base_name: &str,
    format: &str,
) -> Result<String, String> {
    let Some(first) = sel_list.first() else {
        return Err("No items selected for compression".to_string());
    };
    if first.path.contains("://") {
        return Err("Remote archive creation requires local files".to_string());
    }
    let parent = Path::new(&first.path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("/tmp");
    let trimmed_parent = parent.trim_end_matches('/');
    let ext = if format.eq_ignore_ascii_case("zip") {
        "zip"
    } else {
        "tar.gz"
    };
    let clean_stem = normalize_archive_base_name(raw_base_name, ext);
    let stem = if clean_stem.is_empty() {
        default_compress_stem(sel_list)
    } else {
        clean_stem
    };
    let mut archive_path = format!("{trimmed_parent}/{stem}.{ext}");
    let mut counter = 2usize;
    while Path::new(&archive_path).exists() {
        archive_path = format!("{trimmed_parent}/{stem}-{counter}.{ext}");
        counter += 1;
    }

    let names: Vec<String> = sel_list
        .iter()
        .filter_map(|m| {
            Path::new(&m.path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        })
        .collect();
    if names.is_empty() {
        return Err("No valid file names to compress".to_string());
    }

    if ext == "zip" {
        let zip_res = Command::new("zip")
            .current_dir(parent)
            .args(["-r", "-q", &archive_path])
            .args(&names)
            .status();
        let zip_ok = zip_res.is_ok_and(|s| s.success());
        if !zip_ok {
            let py_res = Command::new("python3")
                .current_dir(parent)
                .args(["-m", "zipfile", "-c", &archive_path])
                .args(&names)
                .status();
            let py_ok = py_res.is_ok_and(|s| s.success());
            if !py_ok {
                let tar_res = Command::new("tar")
                    .args(["-caf", &archive_path, "-C", parent])
                    .args(&names)
                    .status()
                    .map_err(|e| format!("Failed to create ZIP archive: {e}"))?;
                if !tar_res.success() {
                    return Err(format!("ZIP archiver exited with status {tar_res}"));
                }
            }
        }
    } else {
        let status = Command::new("tar")
            .args(["-czf", &archive_path, "-C", parent])
            .args(&names)
            .status()
            .map_err(|e| format!("Failed to run tar: {e}"))?;
        if !status.success() {
            return Err(format!("tar exited with status {status}"));
        }
    }
    Ok(archive_path)
}

fn try_unmount_network_share(mount_path: &str) {
    #[cfg(target_os = "linux")]
    {
        let clean = mount_path.strip_prefix("file://").unwrap_or(mount_path);
        let umount_res = Command::new("umount")
            .arg(clean)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if umount_res.is_err() {
            let _ = Command::new("fusermount")
                .args(["-u", clean])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = mount_path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta(name: &str) -> FileMetadata {
        FileMetadata {
            name: name.to_string(),
            path: format!("/tmp/{name}"),
            extension: None,
            kind: "Document".to_string(),
            size: Some(1024),
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
            owner: None,
            group: None,
        }
    }

    #[test]
    fn test_wildcard_and_per_tab_filter_matching() {
        let pdf = sample_meta("report_2026.pdf");
        let po_pdf = sample_meta("PO-0410311168_v1.pdf");
        let img2 = sample_meta("image_01.png");
        let img3 = sample_meta("image_123.png");
        let mp3 = sample_meta("video_test_sample.mp3");
        let txt = sample_meta("notes.txt");
        assert!(matches_search_query(&pdf, "*pdf"));
        assert!(matches_search_query(&pdf, "*.pdf"));
        assert!(matches_search_query(&po_pdf, "po-*pdf"));
        assert!(matches_search_query(&img2, "image_??.png"));
        assert!(!matches_search_query(&img3, "image_??.png"));
        assert!(matches_search_query(&mp3, "test"));
        assert!(!matches_search_query(&txt, "*pdf"));
    }

    #[test]
    fn test_sort_metadata_keeps_directories_separate_from_files() {
        let mut dir_a = sample_meta("alpha_dir");
        dir_a.is_dir = true;
        dir_a.size = None;
        dir_a.modified = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100));

        let mut dir_z = sample_meta("zeta_dir");
        dir_z.is_dir = true;
        dir_z.size = None;
        dir_z.modified = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(500));

        let mut file_small = sample_meta("b_small.txt");
        file_small.size = Some(128);
        file_small.modified = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(900));

        let mut file_large = sample_meta("a_large.bin");
        file_large.size = Some(8192);
        file_large.modified = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(200));

        let mut items = vec![file_small, dir_z, file_large, dir_a];

        // 1. Sort by Date Modified (Descending): folders must STILL be at the top (zeta_dir, alpha_dir),
        // followed by files sorted newest-first (b_small.txt, a_large.bin).
        sort_metadata_slice(&mut items, SortColumn::Modified, SortDirection::Descending);
        assert!(items[0].is_dir && items[0].name == "zeta_dir");
        assert!(items[1].is_dir && items[1].name == "alpha_dir");
        assert!(!items[2].is_dir && items[2].name == "b_small.txt");
        assert!(!items[3].is_dir && items[3].name == "a_large.bin");

        // 2. Sort by Size (Descending): folders remain at top, files sorted largest-first.
        sort_metadata_slice(&mut items, SortColumn::Size, SortDirection::Descending);
        assert!(items[0].is_dir && items[1].is_dir);
        assert!(!items[2].is_dir && items[2].name == "a_large.bin");
        assert!(!items[3].is_dir && items[3].name == "b_small.txt");

        // 3. Sort by Name (Descending): folders remain at top (zeta_dir, alpha_dir),
        // followed by files (b_small.txt, a_large.bin).
        sort_metadata_slice(&mut items, SortColumn::Name, SortDirection::Descending);
        assert_eq!(items[0].name, "zeta_dir");
        assert_eq!(items[1].name, "alpha_dir");
        assert_eq!(items[2].name, "b_small.txt");
        assert_eq!(items[3].name, "a_large.bin");
    }

    #[test]
    fn test_remote_tab_title_includes_connection_name_and_path() {
        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        rt.saved_connections.connections.push(ConnectionProfile {
            id: "conn-prod".to_string(),
            name: "Prod SFTP".to_string(),
            group: None,
            protocol: ProtocolType::Sftp,
            host: Some("deploy.example.com".to_string()),
            port: Some(22),
            endpoint: None,
            bucket: None,
            region: None,
            username: "ubuntu".to_string(),
            auth_type: AuthType::Password,
            key_path: None,
            base_path: "/var/www".to_string(),
            local_path: None,
            base_url: None,
            text_encoding: "Unicode (UTF-8)".to_string(),
            note: String::new(),
            color_label: ColorLabel::Green,
            show_advanced: true,
        });
        let title = rt.format_tab_title("sftp://ubuntu@deploy.example.com:22/var/www/html");
        assert_eq!(title, "Prod SFTP: /var/www/html");
    }

    #[test]
    fn test_batch_transfer_conflict_detection_and_keep_both() {
        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        let src_dir = "/tmp/manifold_conflict_src";
        let dst_dir = "/tmp/manifold_conflict_dst";
        let _ = rt.local_vfs.remove(src_dir, true);
        let _ = rt.local_vfs.remove(dst_dir, true);
        rt.local_vfs.create_dir(src_dir).unwrap();
        rt.local_vfs.create_dir(dst_dir).unwrap();

        let src_file = format!("{src_dir}/cert.pdf");
        let dst_file = format!("{dst_dir}/cert.pdf");
        rt.local_vfs.write_bytes(&src_file, b"new-pdf").unwrap();
        rt.local_vfs.write_bytes(&dst_file, b"old-pdf").unwrap();

        let src_meta = rt.local_vfs.metadata(&src_file).unwrap();
        rt.execute_drop_target(std::slice::from_ref(&src_meta), dst_dir, false);
        assert!(rt.active_conflict().is_some());

        rt.resolve_batch_conflict(ConflictResolution::KeepBoth, false);
        assert!(rt.active_conflict().is_none());

        let kept_copy = format!("{dst_dir}/cert 2.pdf");
        assert!(rt.local_vfs.metadata(&dst_file).is_ok());
        assert!(rt.local_vfs.metadata(&kept_copy).is_ok());

        let _ = rt.local_vfs.remove(src_dir, true);
        let _ = rt.local_vfs.remove(dst_dir, true);
    }

    #[test]
    fn test_executable_classification_ignores_mount_exec_bits_on_documents() {
        let mut pdf = sample_meta("Quadrata_FZCO_salary_certificate_2026.pdf");
        pdf.extension = Some("pdf".to_string());
        pdf.permissions = 0o755;
        assert!(!is_executable_file(&pdf));

        let mut doc = sample_meta("Quadrata_FZCO_Doc_template_2023.doc");
        doc.extension = Some("doc".to_string());
        doc.permissions = 0o777;
        assert!(!is_executable_file(&doc));

        let mut script = sample_meta("deploy.sh");
        script.extension = Some("sh".to_string());
        script.permissions = 0o644;
        assert!(is_executable_file(&script));

        let mut elf_bin = sample_meta("manifold");
        elf_bin.extension = None;
        elf_bin.permissions = 0o755;
        assert!(is_executable_file(&elf_bin));

        let mut fat_no_ext = sample_meta("README");
        fat_no_ext.extension = None;
        fat_no_ext.permissions = 0o777;
        assert!(!is_executable_file(&fat_no_ext));
    }

    #[test]
    fn test_external_directory_fingerprint_detects_new_and_deleted_entries() {
        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        let watch_dir = "/tmp/manifold_live_watch_test";
        let _ = rt.local_vfs.remove(watch_dir, true);
        rt.local_vfs.create_dir(watch_dir).unwrap();
        rt.navigate_to_path(false, watch_dir.to_string());

        assert!(!rt.poll_external_directory_changes());

        let ext_file = format!("{watch_dir}/created_outside.txt");
        std::fs::write(&ext_file, b"hello").unwrap();
        assert!(rt.poll_external_directory_changes());
        let found_after_add = rt
            .left_entries
            .iter()
            .any(|r| r.meta.name == "created_outside.txt");
        assert!(found_after_add);

        std::fs::remove_file(&ext_file).unwrap();
        assert!(rt.poll_external_directory_changes());
        let found_after_del = rt
            .left_entries
            .iter()
            .any(|r| r.meta.name == "created_outside.txt");
        assert!(!found_after_del);

        let _ = rt.local_vfs.remove(watch_dir, true);
    }

    #[test]
    fn test_format_app_version_label_normalizes_leading_dots_and_v_prefix() {
        assert_eq!(format_app_version_label(".0.3.1.2"), "v.0.3.1.2");
        assert_eq!(format_app_version_label("v.0.3.1.2"), "v.0.3.1.2");
        assert_eq!(format_app_version_label("v0.3.1"), "v.0.3.1");
        assert_eq!(format_app_version_label("0.3.1.2"), "v.0.3.1.2");
        assert_eq!(format_app_version_label("V.1.2.0"), "v.1.2.0");
        assert_eq!(format_app_version_label(""), "v.0.1.0");
    }

    #[test]
    fn test_external_drag_out_resolves_at_window_edges() {
        let rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        let items = [sample_meta("report.pdf")];
        let bounds = (205.0, 1360.0, 840.0);

        let l_coords = (8.0, 300.0, 700.0, 0.0, 0.0);
        let left_edge = rt.resolve_drop_target(false, &items, l_coords, bounds);
        assert_eq!(left_edge.dst_dir.as_deref(), Some("__EXTERNAL_OS_DROP__"));

        let r_coords = (1352.0, 300.0, 700.0, 0.0, 0.0);
        let right_edge = rt.resolve_drop_target(false, &items, r_coords, bounds);
        assert_eq!(right_edge.dst_dir.as_deref(), Some("__EXTERNAL_OS_DROP__"));

        let b_coords = (500.0, 832.0, 700.0, 0.0, 0.0);
        let bottom_edge = rt.resolve_drop_target(false, &items, b_coords, bounds);
        assert_eq!(bottom_edge.dst_dir.as_deref(), Some("__EXTERNAL_OS_DROP__"));
    }

    #[test]
    fn test_favorites_membership_and_unpin_persistence() {
        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        let custom_dir = "/tmp/manifold_fav_test_dir";
        assert!(!rt.is_path_in_favorites(custom_dir));

        rt.pin_folder_to_favorites(custom_dir, "Fav Test");
        assert!(rt.is_path_in_favorites(custom_dir));

        rt.unpin_folder_from_favorites("", custom_dir);
        assert!(!rt.is_path_in_favorites(custom_dir));
    }

    #[test]
    fn test_discover_open_with_apps_includes_system_default() {
        let mut pdf = sample_meta("invoice.pdf");
        pdf.extension = Some("pdf".to_string());
        let apps = discover_open_with_apps(&pdf);
        assert!(!apps.is_empty());
        assert_eq!(apps[0].id.as_str(), "__DEFAULT__");
    }

    #[test]
    fn test_archive_detection_and_extract_to_folder() {
        let targz = sample_meta("manifold-main-71634bd-macos-arm64.tar.gz");
        assert!(is_extractable_archive(&targz));
        assert_eq!(
            archive_output_stem(&targz.name),
            "manifold-main-71634bd-macos-arm64"
        );

        let zip = sample_meta("release_bundle.zip");
        assert!(is_extractable_archive(&zip));
        assert_eq!(archive_output_stem(&zip.name), "release_bundle");

        let txt = sample_meta("notes.txt");
        assert!(!is_extractable_archive(&txt));
    }

    #[test]
    fn test_multi_file_compress_stem_formats_and_selection_reset() {
        let f1 = sample_meta("invoice_01.pdf");
        let f2 = sample_meta("invoice_02.pdf");
        let single_stem = default_compress_stem(std::slice::from_ref(&f1));
        assert_eq!(single_stem, "invoice_01");

        let multi_stem = default_compress_stem(&[f1, f2]);
        assert!(multi_stem.starts_with("compressed-file-20"));
        assert_eq!(multi_stem.len(), "compressed-file-YYYY-MM-DD".len());

        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        let work_dir = "/tmp/manifold_multi_compress_test";
        let _ = rt.local_vfs.remove(work_dir, true);
        rt.local_vfs.create_dir(work_dir).unwrap();
        let p1 = format!("{work_dir}/doc_a.txt");
        let p2 = format!("{work_dir}/doc_b.txt");
        rt.local_vfs.write_bytes(&p1, b"alpha").unwrap();
        rt.local_vfs.write_bytes(&p2, b"beta").unwrap();

        rt.navigate_to_path(false, work_dir.to_string());
        rt.apply_row_selection(false, 0, false, false);
        rt.apply_row_selection(false, 1, true, false);
        assert_eq!(rt.left_selected_indices.len(), 2);

        let sel = rt.selected_metadata_list(false);
        let tgz_path = create_archive_from_selection(&sel, &multi_stem, "tar.gz").unwrap();
        assert!(tgz_path.ends_with(".tar.gz"));
        assert!(Path::new(&tgz_path).exists());

        let zip_path = create_archive_from_selection(&sel, &multi_stem, "zip").unwrap();
        assert!(zip_path.ends_with(".zip"));
        assert!(Path::new(&zip_path).exists());

        rt.invalidate_dir_cache();
        rt.reload_pane_entries(false);
        rt.select_single_path_or_first(false, Some(&zip_path));
        assert_eq!(rt.left_selected_indices.len(), 1);
        let sole_meta = rt.selected_metadata().unwrap();
        assert_eq!(sole_meta.path, zip_path);

        let _ = rt.local_vfs.remove(work_dir, true);
    }

    #[test]
    fn test_recursive_downstream_search_finds_nested_files_and_dirs() {
        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        let root_dir = "/tmp/manifold_recursive_search_test";
        let _ = rt.local_vfs.remove(root_dir, true);
        rt.local_vfs.create_dir(root_dir).unwrap();

        let sub_a = format!("{root_dir}/Alpha_Client/2024");
        let sub_b = format!("{root_dir}/Beta_Client/2025/HPEPO539777");
        let sub_hidden = format!("{root_dir}/.hidden_cache");
        rt.local_vfs.create_dir(&sub_a).unwrap();
        rt.local_vfs.create_dir(&sub_b).unwrap();
        rt.local_vfs.create_dir(&sub_hidden).unwrap();

        let nested_pdf = format!("{sub_a}/HPEPO539777_invoice.pdf");
        let inside_matched_dir = format!("{sub_b}/order_details.txt");
        let hidden_match = format!("{sub_hidden}/HPEPO539777_secret.txt");
        rt.local_vfs.write_bytes(&nested_pdf, b"pdf-data").unwrap();
        rt.local_vfs
            .write_bytes(&inside_matched_dir, b"order")
            .unwrap();
        rt.local_vfs.write_bytes(&hidden_match, b"secret").unwrap();

        rt.navigate_to_path(false, root_dir.to_string());
        let l_idx = rt.app_state.left_active_tab;
        rt.app_state.left_tabs[l_idx].inline_filter = "HPEPO539777".to_string();
        rt.reload_pane_entries(false);

        for _ in 0..50 {
            rt.poll_search_events();
            if !rt.left_search_running {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!rt.left_search_running);
        assert_eq!(rt.left_entries.len(), 2);
        assert!(rt.left_entries[0].meta.is_dir);
        assert_eq!(rt.left_entries[0].meta.name, "HPEPO539777");
        assert!(!rt.left_entries[1].meta.is_dir);
        assert_eq!(rt.left_entries[1].meta.name, "HPEPO539777_invoice.pdf");

        let rel_sub = relative_parent_subpath(&rt.left_entries[1].meta.path, root_dir);
        assert_eq!(rel_sub.as_deref(), Some("Alpha_Client/2024"));

        // "Show in Enclosing Folder" navigates to the nested file's parent directory, clears search, and selects it
        rt.show_entry_in_enclosing_folder(false, &nested_pdf);
        assert_eq!(rt.current_pane_path(false), sub_a);
        assert!(rt.app_state.left_tabs[l_idx].inline_filter.is_empty());
        assert_eq!(rt.left_selected_idx, Some(0));
        assert_eq!(rt.left_entries[0].meta.path, nested_pdf);

        // Navigating into the matched folder clears the inline search filter automatically
        rt.navigate_to_path(false, root_dir.to_string());
        rt.app_state.left_tabs[l_idx].inline_filter = "HPEPO539777".to_string();
        rt.reload_pane_entries(false);
        for _ in 0..50 {
            rt.poll_search_events();
            if !rt.left_search_running {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        rt.open_entry_at(false, Some(0));
        assert_eq!(rt.current_pane_path(false), sub_b);
        assert!(rt.app_state.left_tabs[l_idx].inline_filter.is_empty());
        assert_eq!(rt.left_entries.len(), 1);
        assert_eq!(rt.left_entries[0].meta.name, "order_details.txt");

        let _ = rt.local_vfs.remove(root_dir, true);
    }

    #[test]
    fn test_show_in_enclosing_folder_visibility_rules() {
        let mut direct_item = sample_meta("manifold_fig3.png");
        direct_item.path = "/Users/dimitribellini/Downloads/doc_jpg/manifold_fig3.png".to_string();

        let mut nested_item = sample_meta("HPEPO497793.pdf");
        nested_item.path = "/Volumes/ExtraDisk/Clienti/HPE-2023/Ordine/HPEPO497793.pdf".to_string();

        // 1. Not in search: never show "Show in Enclosing Folder"
        assert!(!should_show_in_enclosing_folder(
            "",
            "/Users/dimitribellini/Downloads/doc_jpg",
            std::slice::from_ref(&direct_item)
        ));
        assert!(!should_show_in_enclosing_folder(
            "",
            "/Volumes/ExtraDisk/Clienti",
            std::slice::from_ref(&nested_item)
        ));

        // 2. Active search, but item is directly inside the main folder: do not show
        assert!(!should_show_in_enclosing_folder(
            "fig3",
            "/Users/dimitribellini/Downloads/doc_jpg",
            std::slice::from_ref(&direct_item)
        ));

        // 3. Active search, and item is in a downstream subfolder: show
        assert!(should_show_in_enclosing_folder(
            "HPEPO497793",
            "/Volumes/ExtraDisk/Clienti",
            std::slice::from_ref(&nested_item)
        ));
    }

    #[test]
    fn test_macos_app_bundle_extraction_and_show_archive_contents() {
        assert!(is_macos_app_bundle_for_os("Manifold.app", true, true));
        assert!(!is_macos_app_bundle_for_os("Manifold.app", true, false));
        assert!(!is_macos_app_bundle_for_os("Manifold.app", false, true));

        let (_, mac_kind) = manifold_vfs::infer_file_kind_for_os("Manifold.app", true, true);
        let (_, lin_kind) = manifold_vfs::infer_file_kind_for_os("Manifold.app", true, false);
        assert_eq!(mac_kind, "Application");
        assert_eq!(lin_kind, "Folder");

        let work_dir = "/tmp/manifold_macos_app_extract_test";
        let _ = std::fs::remove_dir_all(work_dir);
        std::fs::create_dir_all(work_dir).unwrap();

        let build_stage = format!("{work_dir}/build_stage");
        let app_macos_dir = format!("{build_stage}/Manifold.app/Contents/MacOS");
        std::fs::create_dir_all(&app_macos_dir).unwrap();
        std::fs::write(format!("{app_macos_dir}/Manifold"), b"mach-o-bin").unwrap();
        std::fs::write(
            format!("{build_stage}/Manifold.app/Contents/Info.plist"),
            b"<plist/>",
        )
        .unwrap();

        let archive_name = "manifold-v0.5.1-macos-arm64.tar.gz";
        let archive_path = format!("{work_dir}/{archive_name}");
        let status = Command::new("tar")
            .args(["-czf", &archive_path, "-C", &build_stage, "Manifold.app"])
            .status()
            .unwrap();
        assert!(status.success());
        let _ = std::fs::remove_dir_all(&build_stage);

        let mut archive_meta = sample_meta(archive_name);
        archive_meta.path = archive_path.clone();

        // 1. On macOS, extracting the archive unwraps the single .app bundle directly into work_dir/Manifold.app
        let mac_extracted = extract_archive_to_folder_for_os(&archive_meta, true).unwrap();
        assert_eq!(mac_extracted, format!("{work_dir}/Manifold.app"));
        assert!(Path::new(&mac_extracted).is_dir());
        assert!(!Path::new(&format!("{work_dir}/manifold-v0.5.1-macos-arm64")).exists());
        let _ = std::fs::remove_dir_all(&mac_extracted);

        // 2. On Linux/Windows, extracting the archive keeps the standard wrapper folder
        let lin_extracted = extract_archive_to_folder_for_os(&archive_meta, false).unwrap();
        assert_eq!(
            lin_extracted,
            format!("{work_dir}/manifold-v0.5.1-macos-arm64")
        );
        assert!(Path::new(&format!("{lin_extracted}/Manifold.app")).is_dir());

        // 3. "Show Archive Contents" stages the archive contents for browsing
        let stage_dir = open_archive_contents_dir(&archive_meta, true).unwrap();
        assert!(Path::new(&format!("{stage_dir}/Manifold.app")).is_dir());

        let _ = std::fs::remove_dir_all(&stage_dir);
        let _ = std::fs::remove_dir_all(work_dir);
    }

    #[test]
    fn test_drag_drop_sidebar_and_list_row_hit_testing_alignment() {
        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        rt.prefs.font_size_delta = 1;
        rt.sidebar_scroll_y = 0.0;

        let drag_items = [sample_meta("Manifold.app")];
        let font_delta = (rt.prefs.font_size_delta as f32).max(0.0);
        let sb_header_stride = 24.0 + font_delta + 2.0;
        let sb_item_h = 28.0 + font_delta * 1.5;
        let sb_item_stride = sb_item_h + 2.0;

        // Compute the exact Y midpoint of each sidebar entry and verify resolve_drop_target
        // returns that exact entry's target directory and hover_sidebar_idx without downward drift.
        let mut cur_y = 56.0;
        let mut sb_idx: i32 = 0;
        for section in &rt.app_state.sidebar_sections {
            cur_y += sb_header_stride;
            sb_idx += 1;
            for item in &section.items {
                let mid_y = cur_y + sb_item_h * 0.5;
                let res = rt.resolve_drop_target(
                    false,
                    &drag_items,
                    (100.0, mid_y, 800.0, 0.0, 0.0),
                    (200.0, 1280.0, 800.0),
                );
                let expected = vfs_url_to_target(&item.vfs_url);
                if !expected.is_empty() && expected != "connect://dialog" {
                    assert_eq!(
                        res.dst_dir.as_deref(),
                        Some(expected.as_str()),
                        "Sidebar item '{}' at y={mid_y} resolved to wrong target",
                        item.label
                    );
                    assert_eq!(res.hover_sidebar_idx, sb_idx);
                }
                cur_y += sb_item_stride;
                sb_idx += 1;
                for child in &item.children {
                    let child_mid_y = cur_y + sb_item_h * 0.5;
                    let child_res = rt.resolve_drop_target(
                        false,
                        &drag_items,
                        (100.0, child_mid_y, 800.0, 0.0, 0.0),
                        (200.0, 1280.0, 800.0),
                    );
                    let child_expected = vfs_url_to_target(&child.vfs_url);
                    if !child_expected.is_empty() && child_expected != "connect://dialog" {
                        assert_eq!(
                            child_res.dst_dir.as_deref(),
                            Some(child_expected.as_str()),
                            "Sidebar child '{}' at y={child_mid_y} resolved to wrong target",
                            child.label
                        );
                        assert_eq!(child_res.hover_sidebar_idx, sb_idx);
                    }
                    cur_y += sb_item_stride;
                    sb_idx += 1;
                }
            }
        }
    }

    #[test]
    fn test_copy_local_dir_preserving_bundle_replaces_existing_app_and_chmods_macos_bin() {
        let work_dir = "/tmp/manifold_app_bundle_copy_test";
        let _ = std::fs::remove_dir_all(work_dir);
        let src_app = format!("{work_dir}/Downloads/Manifold.app");
        let dst_apps = format!("{work_dir}/Applications");
        let dst_app = format!("{dst_apps}/Manifold.app");

        let src_bin_dir = format!("{src_app}/Contents/MacOS");
        let src_res_dir = format!("{src_app}/Contents/Resources");
        std::fs::create_dir_all(&src_bin_dir).unwrap();
        std::fs::create_dir_all(&src_res_dir).unwrap();
        std::fs::create_dir_all(&dst_apps).unwrap();

        let src_bin = format!("{src_bin_dir}/Manifold");
        std::fs::write(&src_bin, b"new-mach-o-binary-v2").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let non_exec = std::fs::Permissions::from_mode(0o644);
            std::fs::set_permissions(&src_bin, non_exec).unwrap();
        }
        std::fs::write(
            format!("{src_app}/Contents/Info.plist"),
            b"<plist>v2</plist>",
        )
        .unwrap();
        std::fs::write(format!("{src_res_dir}/AppIcon.icns"), b"icns-v2").unwrap();

        // Pre-populate an existing /Applications/Manifold.app with stale files
        let old_bin_dir = format!("{dst_app}/Contents/MacOS");
        std::fs::create_dir_all(&old_bin_dir).unwrap();
        std::fs::write(format!("{old_bin_dir}/Manifold"), b"old-binary-v1").unwrap();
        std::fs::write(format!("{dst_app}/Contents/stale.txt"), b"stale").unwrap();

        // Copy src_app over dst_app atomically via copy_local_dir_preserving_bundle
        copy_local_dir_preserving_bundle(&src_app, &dst_app).unwrap();

        // Must NOT nest Manifold.app/Manifold.app inside dst_app
        assert!(!Path::new(&format!("{dst_app}/Manifold.app")).exists());
        // Stale files from the previous bundle must be gone
        assert!(!Path::new(&format!("{dst_app}/Contents/stale.txt")).exists());
        // New binary and Info.plist must match v2
        let copied_bin = format!("{dst_app}/Contents/MacOS/Manifold");
        let copied_bytes = std::fs::read(&copied_bin).unwrap();
        assert_eq!(copied_bytes, b"new-mach-o-binary-v2");

        // sanitize_macos_app_bundle must ensure +x (0o111) on Contents/MacOS/Manifold
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copied_bin).unwrap().permissions().mode();
            assert_ne!(
                mode & 0o111,
                0,
                "Expected Contents/MacOS/Manifold to be executable"
            );
        }

        let _ = std::fs::remove_dir_all(work_dir);
    }

    #[test]
    fn test_tree_view_vs_list_view_flattening_and_mode_mapping() {
        assert_eq!(view_mode_to_int(PaneViewMode::List), 0);
        assert_eq!(view_mode_to_int(PaneViewMode::Icons), 1);
        assert_eq!(view_mode_to_int(PaneViewMode::Columns), 2);
        assert_eq!(view_mode_to_int(PaneViewMode::Tree), 3);

        assert_eq!(int_to_view_mode(0), PaneViewMode::List);
        assert_eq!(int_to_view_mode(1), PaneViewMode::Icons);
        assert_eq!(int_to_view_mode(2), PaneViewMode::Columns);
        assert_eq!(int_to_view_mode(3), PaneViewMode::Tree);

        let work_dir = "/tmp/manifold_tree_vs_list_test";
        let _ = std::fs::remove_dir_all(work_dir);
        let sub_dir = format!("{work_dir}/alpha");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(format!("{sub_dir}/nested.txt"), b"hello").unwrap();
        std::fs::write(format!("{work_dir}/root.txt"), b"world").unwrap();

        let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
        rt.app_state.active_pane = ActivePane::Left;
        rt.app_state.left_active_tab = 0;
        rt.app_state.left_tabs[0].current_path = work_dir.to_string();
        rt.app_state.left_tabs[0]
            .expanded_folders
            .insert(sub_dir.clone());

        // In List mode (0), expanded_folders are ignored so the list stays strictly flat
        rt.app_state.left_tabs[0].view_mode = PaneViewMode::List;
        rt.reload_pane_entries(false);
        assert_eq!(rt.left_entries.len(), 2);
        assert!(rt.left_entries.iter().all(|e| e.depth == 0));

        // In Tree mode (3), expanded_folders are recursively flattened with depth > 0
        rt.app_state.left_tabs[0].view_mode = PaneViewMode::Tree;
        rt.reload_pane_entries(false);
        assert_eq!(rt.left_entries.len(), 3);
        assert!(rt
            .left_entries
            .iter()
            .any(|e| e.meta.name == "nested.txt" && e.depth == 1));
        assert_eq!(rt.app_state.toolbar.active_view_mode, PaneViewMode::Tree);

        let _ = std::fs::remove_dir_all(work_dir);
    }
}
