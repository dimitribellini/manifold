//! Interactive Slint Desktop GUI Controller for Project Manifold.
//!
//! Binds [`ManifoldAppState`], [`LocalFsBackend`], [`RemoteVfsBackend`], [`TransferEngine`],
//! [`SyncExecutionPlan`], [`BatchRenamer`], and [`FileInspectorViewModel`] to the declarative
//! Slint window defined in `crates/ui/ui/manifold.slint`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use manifold_config::{
    AuthType, ConnectionProfile, ConnectionsConfig, CredentialStore, PaneViewMode, ProtocolType,
    SortColumn, SortDirection,
};
use manifold_preview::{format_hex_dump, FileInspectorViewModel};
use manifold_sync::{ComparisonMode, SyncExecutionPlan, SyncStrategy};
use manifold_transfer::{
    ConflictModalDescriptor, ConflictResolution, TransferEngine, TransferPriority, TransferStatus,
    TransferTask, DEFAULT_TRANSFER_CHUNK_SIZE,
};
use manifold_vfs::{
    ArchiveBackend, ArchiveFormat, ColorLabel, FileMetadata, FileSystemBackend, LocalFsBackend,
    PermissionsMatrix,
};
use manifold_vfs_remote::{RemoteVfsBackend, TerminalIntegration};
use slint::{Color, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::{profile_to_vfs_url, ActivePane, ManifoldAppState, PanesLayout, SidebarItem};

slint::slint! {
    export {
        ManifoldWindow,
        SidebarEntrySlint,
        TabEntrySlint,
        BreadcrumbSegmentSlint,
        FileRowSlint,
        IconGridTripleSlint,
        TransferRowSlint
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

struct GuiRuntime {
    app_state: ManifoldAppState,
    local_vfs: LocalFsBackend,
    mounted_vfs: ArchiveBackend,
    remote_backends: HashMap<String, RemoteVfsBackend>,
    saved_connections: ConnectionsConfig,
    credential_store: CredentialStore,
    transfer_engine: TransferEngine,
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
    last_click: Option<(bool, String, Instant)>,
    clipboard_paths: Vec<String>,
    clipboard_cut: bool,
    sidebar_targets: Vec<(String, String)>,
    sidebar_colors: HashMap<String, ColorLabel>,
    sidebar_context_idx: usize,
    transfers: Vec<TransferTask>,
    logs: Vec<String>,
    pending_conflict: Option<(FileMetadata, FileMetadata, bool)>,
    pending_delete_permanent: bool,
    spinner_frame: i32,
    active_task_ticks: u32,
    active_task_label: String,
    active_task_progress: i32,
    toast_ticks: u32,
    toast_text: String,
    toast_kind: String,
}

impl GuiRuntime {
    fn new(app_state: ManifoldAppState) -> Self {
        let mounted_vfs = ArchiveBackend::new("remote-mounts.tar", ArchiveFormat::Tar);
        let saved_connections = ConnectionsConfig::load_default();
        let mut sidebar_colors = HashMap::new();
        for prof in &saved_connections.connections {
            if prof.color_label != ColorLabel::None {
                sidebar_colors.insert(prof.id.clone(), prof.color_label);
            }
        }
        let credential_store = CredentialStore::with_os_backend();
        let init_log = "[INFO] Manifold VFS initialized (Local POSIX + Remote VFS).";
        let keyring_log = "[INFO] Keyring service bound to 'manifold'.";
        let mut init_sel = HashSet::new();
        init_sel.insert(0);
        let mut runtime = Self {
            app_state,
            local_vfs: LocalFsBackend::new(),
            mounted_vfs,
            remote_backends: HashMap::new(),
            saved_connections,
            credential_store,
            transfer_engine: TransferEngine::new(None),
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
            last_click: None,
            clipboard_paths: Vec::new(),
            clipboard_cut: false,
            sidebar_targets: Vec::new(),
            sidebar_colors,
            sidebar_context_idx: 0,
            transfers: Vec::new(),
            logs: vec![init_log.to_string(), keyring_log.to_string()],
            pending_conflict: None,
            pending_delete_permanent: false,
            spinner_frame: 0,
            active_task_ticks: 0,
            active_task_label: String::new(),
            active_task_progress: 0,
            toast_ticks: 0,
            toast_text: String::new(),
            toast_kind: "info".to_string(),
        };
        runtime.reload_pane_entries(false);
        runtime.reload_pane_entries(true);
        runtime
    }

    fn show_toast(&mut self, kind: &str, message: String) {
        self.toast_kind = kind.to_string();
        self.toast_text = message;
        self.toast_ticks = 55;
        play_system_sound(kind);
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
            {
                let status = Command::new("cp").args(["-R", src_path, dst_path]).status();
                if status.is_ok_and(|s| s.success()) {
                    return Ok(());
                }
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
        let src_slash = format!("{}/", src_path.trim_end_matches('/'));
        if dst_path == src_path || dst_path.starts_with(&src_slash) {
            return Err("Cannot move a folder into itself".to_string());
        }
        if self.vfs_rename(src_path, dst_path).is_ok() {
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

        // 1. Sidebar Hit-Testing (Devices, Connections, Favorites)
        if (6.0..=208.0).contains(&abs_x) && abs_y >= 48.0 {
            let mut cur_y = 50.0f32;
            let mut sb_idx = 0usize;
            for section in &self.app_state.sidebar_sections {
                cur_y += 24.0;
                sb_idx += 1;
                for item in &section.items {
                    if abs_y >= cur_y && abs_y < cur_y + 28.0 {
                        let target = vfs_url_to_target(&item.vfs_url);
                        if !target.is_empty() && target != "connect://dialog" {
                            res.dst_dir = Some(target);
                            res.dst_label = format!("→ {}", item.label);
                            res.hover_sidebar_idx = sb_idx as i32;
                            return res;
                        }
                    }
                    cur_y += 28.0;
                    sb_idx += 1;
                    for child in &item.children {
                        if abs_y >= cur_y && abs_y < cur_y + 28.0 {
                            let target = vfs_url_to_target(&child.vfs_url);
                            if !target.is_empty() && target != "connect://dialog" {
                                res.dst_dir = Some(target);
                                res.dst_label = format!("→ {}", child.label);
                                res.hover_sidebar_idx = sb_idx as i32;
                                return res;
                            }
                        }
                        cur_y += 28.0;
                        sb_idx += 1;
                    }
                }
            }
            return res;
        }

        if abs_x < 210.0 {
            return res;
        }

        let pane_w = left_pane_w.max(260.0);
        let right_pane_x = 212.0 + pane_w + 6.0;
        let target_is_right = self.app_state.dual_pane_enabled && abs_x >= right_pane_x;
        let rel_x = if target_is_right {
            abs_x - right_pane_x
        } else {
            abs_x - 212.0
        };

        // 2. Tab Bar Hit-Testing
        if (36.0..=76.0).contains(&abs_y) {
            let tab_idx = ((rel_x - 6.0) / 154.0).floor() as isize;
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
        if (76.0..=108.0).contains(&abs_y) {
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

        if abs_y <= 108.0 {
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
        let hovered_idx = if view_mode == 0 {
            if abs_y >= 136.0 {
                ((abs_y - 140.0 - scroll_y) / 27.0).floor() as isize
            } else {
                -1
            }
        } else if view_mode == 1 {
            let row_i = ((abs_y - 120.0 - scroll_y) / 134.0).floor() as isize;
            let cell_w = (pane_w - 24.0).max(180.0) / 3.0;
            let col_i = ((rel_x - 12.0) / cell_w).floor().clamp(0.0, 2.0) as isize;
            if row_i >= 0 {
                row_i * 3 + col_i
            } else {
                -1
            }
        } else if rel_x >= 196.0 {
            ((abs_y - 108.0 - scroll_y) / 26.0).floor() as isize
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
            if let Some(entry) = entry_opt.filter(|e| e.meta.is_dir) {
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

    fn execute_drop_target(&mut self, items: &[FileMetadata], dst_dir: &str, is_move: bool) {
        if items.is_empty() {
            return;
        }
        if let Some((prefix, _)) = split_remote_uri(dst_dir) {
            self.ensure_remote_connected(&prefix);
        }
        let mut count = 0usize;
        let mut total_bytes = 0u64;
        let first_name = items[0].name.clone();
        for src_meta in items {
            if let Some((prefix, _)) = split_remote_uri(&src_meta.path) {
                self.ensure_remote_connected(&prefix);
            }
            let dst_path = join_vfs_path(dst_dir, &src_meta.name);
            if dst_path == src_meta.path {
                continue;
            }
            let ok = if is_move {
                self.vfs_move_entry(&src_meta.path, &dst_path).is_ok()
            } else {
                self.vfs_copy_entry(&src_meta.path, &dst_path).is_ok()
            };
            if ok {
                total_bytes += src_meta.size.unwrap_or(4096);
                let tag = if is_move { "DRAG-MOVE" } else { "DRAG-COPY" };
                let msg = format!("[{tag}] {} -> {dst_path}", src_meta.name);
                self.logs.push(msg);
                count += 1;
            }
        }
        if count > 0 {
            let dst_title = vfs_display_title(dst_dir);
            let verb = if is_move { "Moved" } else { "Copied" };
            let label = if count == 1 {
                format!("{verb} {first_name}")
            } else {
                format!("{verb} {count} items")
            };
            let toast = format!("{label} to {dst_title}");
            self.start_tracked_task(&label, dst_dir, total_bytes, toast, "success");
            self.reload_pane_entries(false);
            self.reload_pane_entries(true);
        }
    }

    fn paste_clipboard_into(&mut self, dst_dir: &str) -> usize {
        if self.clipboard_paths.is_empty() {
            return 0;
        }
        let sources = self.clipboard_paths.clone();
        let is_cut = self.clipboard_cut;
        let mut count = 0usize;
        let mut total_bytes = 0u64;
        let mut first_name = String::new();

        for src_path in &sources {
            let name = vfs_file_name(src_path);
            if first_name.is_empty() {
                first_name = name.clone();
            }
            if let Ok(m) = self.backend_for_path(src_path).metadata(src_path) {
                total_bytes += m.size.unwrap_or(4096);
            } else {
                total_bytes += 4096;
            }
            let mut dst_path = join_vfs_path(dst_dir, &name);

            if is_cut {
                if src_path == &dst_path {
                    continue;
                }
                let moved = if self.vfs_rename(src_path, &dst_path).is_ok() {
                    true
                } else if self.vfs_copy_entry(src_path, &dst_path).is_ok() {
                    let _ = self.vfs_remove(src_path, true);
                    true
                } else {
                    false
                };
                if moved {
                    let msg = format!("[MOVE] {src_path} -> {dst_path}");
                    self.logs.push(msg);
                    count += 1;
                }
            } else {
                if src_path == &dst_path
                    || (split_remote_uri(&dst_path).is_none() && Path::new(&dst_path).exists())
                {
                    let (stem, ext_sfx) = split_name_stem_ext(&name);
                    dst_path = join_vfs_path(dst_dir, &format!("{stem} copy{ext_sfx}"));
                    let mut idx = 2u32;
                    while split_remote_uri(&dst_path).is_none() && Path::new(&dst_path).exists() {
                        let cand = format!("{stem} copy {idx}{ext_sfx}");
                        dst_path = join_vfs_path(dst_dir, &cand);
                        idx += 1;
                    }
                }
                if self.vfs_copy_entry(src_path, &dst_path).is_ok() {
                    let msg = format!("[PASTE] {src_path} -> {dst_path}");
                    self.logs.push(msg);
                    count += 1;
                }
            }
        }

        if is_cut && count > 0 {
            self.clipboard_paths.clear();
            self.clipboard_cut = false;
        }
        if count > 0 {
            let dst_title = vfs_display_title(dst_dir);
            let verb = if is_cut { "Moved" } else { "Pasted" };
            let label = if count == 1 {
                format!("{verb} {first_name}")
            } else {
                format!("{verb} {count} items")
            };
            let toast = format!("{label} to {dst_title}");
            self.start_tracked_task(&label, dst_dir, total_bytes, toast, "success");
        }
        count
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
        let title = vfs_display_title(&new_path);
        let vfs_url = if new_path.contains("://") {
            new_path.clone()
        } else {
            format!("file://{new_path}")
        };

        if is_right {
            let idx = self.app_state.right_active_tab;
            if let Some(tab) = self.app_state.right_tabs.get_mut(idx) {
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
        self.app_state.toolbar.active_folder_title = title;
        self.reload_pane_entries(is_right);
    }

    fn list_sorted_dir(
        &self,
        dir_path: &str,
        show_hidden: bool,
        query: &str,
        sort_col: SortColumn,
        sort_dir: SortDirection,
    ) -> Vec<FileMetadata> {
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

        let mut filtered: Vec<FileMetadata> = raw_items
            .into_iter()
            .filter(|m| show_hidden || !m.is_hidden)
            .filter(|m| matches_search_query(m, query))
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
            let is_exp = meta.is_dir && expanded_set.contains(&meta.path);
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
        let (sort_col, sort_dir, inline_filter, expanded) = match active_tab {
            Some(t) => (
                t.sort_column,
                t.sort_direction,
                t.inline_filter.clone(),
                t.expanded_folders.clone(),
            ),
            None => (
                SortColumn::Name,
                SortDirection::Ascending,
                String::new(),
                HashSet::new(),
            ),
        };

        let mut flattened = Vec::new();
        let opts = (inline_filter.as_str(), sort_col, sort_dir);
        self.collect_flattened_rows(&path, 0, &expanded, opts, &mut flattened);

        let parent_items = vfs_parent_dir(&path)
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
            .unwrap_or_default();

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
        let right_vol = if split_remote_uri(&right_path).is_some() {
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
        self.app_state.left_header = crate::PathAndStatusHeader::build(
            Some(root_vol_name),
            &left_path,
            self.left_entries.len(),
            &left_sel,
            left_vol,
        );

        let right_sel: Vec<&FileMetadata> = self
            .right_selected_indices
            .iter()
            .filter_map(|&i| self.right_entries.get(i).map(|r| &r.meta))
            .collect();
        self.app_state.right_header = crate::PathAndStatusHeader::build(
            Some(root_vol_name),
            &right_path,
            self.right_entries.len(),
            &right_sel,
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
        } else {
            vfs_display_title(active_path)
        };
        self.app_state.toolbar.active_folder_title = active_title;
        self.app_state.toolbar.search_query = active_filter;

        let active_sel_count = if is_right_active {
            right_sel.len()
        } else {
            left_sel.len()
        };

        if let Some(meta) = self.selected_metadata().cloned() {
            let sample = if meta.is_dir || split_remote_uri(&meta.path).is_some() {
                None
            } else {
                let vfs = self.backend_for_path(&meta.path);
                vfs.read_range(&meta.path, 0, 512).ok()
            };
            let modified = meta.formatted_modified_datetime();
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

        if meta.is_dir {
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

fn play_system_sound(kind: &str) {
    if cfg!(target_os = "macos") {
        let sound_file = match kind {
            "delete" => "/System/Library/Sounds/Funk.aiff",
            "copy" | "cut" => "/System/Library/Sounds/Tink.aiff",
            _ => "/System/Library/Sounds/Glass.aiff",
        };
        let _ = Command::new("afplay")
            .arg(sound_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    } else {
        let event_id = match kind {
            "delete" => "trash-empty",
            _ => "message-new-instant",
        };
        let _ = Command::new("canberra-gtk-play")
            .args(["-i", event_id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

fn open_with_system_handler(path: &str) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg(path).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(path).spawn();
    }
}

fn reveal_in_system_file_manager(path: &str) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("open").args(["-R", path]).spawn();
    } else if let Some(parent) = Path::new(path).parent().and_then(|p| p.to_str()) {
        let _ = Command::new("xdg-open").arg(parent).spawn();
    }
}

fn copy_string_to_system_clipboard(text: &str) {
    let cmd_name = if cfg!(target_os = "macos") {
        "pbcopy"
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
    let (stem, ext_suffix) = match &meta.extension {
        Some(ext) if !ext.is_empty() && !meta.is_dir => {
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
        let status = Command::new("cp")
            .args(["-R", &meta.path, &dst_path])
            .status();
        if status.is_err() {
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

fn matches_search_query(meta: &FileMetadata, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let name = meta.name.to_lowercase();
    if q.contains('*') || q.contains('?') {
        let anchored = format!("*{q}*");
        wildcard_match(&q, &name) || wildcard_match(&anchored, &name)
    } else {
        name.contains(&q)
    }
}

#[allow(clippy::too_many_arguments)]
fn sort_metadata_slice(items: &mut [FileMetadata], column: SortColumn, direction: SortDirection) {
    let desc = direction == SortDirection::Descending;
    match (column, desc) {
        (SortColumn::Size, false) => {
            items.sort_by_key(|a| {
                let sz = a.size.unwrap_or(0);
                (!a.is_dir, sz, a.name.to_lowercase())
            });
        }
        (SortColumn::Size, true) => {
            items.sort_by_key(|a| {
                let rev_sz = std::cmp::Reverse(a.size.unwrap_or(0));
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!a.is_dir, rev_sz, rev_name)
            });
        }
        (SortColumn::Modified | SortColumn::Created, false) => {
            items.sort_by_key(|a| {
                let name = a.name.to_lowercase();
                (!a.is_dir, a.modified, name)
            });
        }
        (SortColumn::Modified | SortColumn::Created, true) => {
            items.sort_by_key(|a| {
                let rev_mod = std::cmp::Reverse(a.modified);
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!a.is_dir, rev_mod, rev_name)
            });
        }
        (SortColumn::Kind, false) => {
            items.sort_by_key(|a| {
                let kind = a.kind.to_lowercase();
                (!a.is_dir, kind, a.name.to_lowercase())
            });
        }
        (SortColumn::Kind, true) => {
            items.sort_by_key(|a| {
                let rev_kind = std::cmp::Reverse(a.kind.to_lowercase());
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!a.is_dir, rev_kind, rev_name)
            });
        }
        (_, false) => {
            items.sort_by_key(|a| (!a.is_dir, a.name.to_lowercase()));
        }
        (_, true) => {
            items.sort_by_key(|a| {
                let rev_name = std::cmp::Reverse(a.name.to_lowercase());
                (!a.is_dir, rev_name)
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
    }
}

fn int_to_view_mode(v: i32) -> PaneViewMode {
    match v {
        1 => PaneViewMode::Icons,
        2 => PaneViewMode::Columns,
        _ => PaneViewMode::List,
    }
}

fn sync_window_from_runtime(window: &ManifoldWindow, rt: &mut GuiRuntime) {
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
    window.set_task_running(rt.active_task_ticks > 0);
    window.set_spinner_frame(rt.spinner_frame);
    window.set_active_task_label(rt.active_task_label.clone().into());
    window.set_active_task_progress(rt.active_task_progress);
    window.set_show_toast(rt.toast_ticks > 0);
    window.set_toast_text(rt.toast_text.clone().into());
    window.set_toast_kind(rt.toast_kind.clone().into());

    let active_dir = rt.current_pane_path(is_right_active);

    // Sidebar model
    let mut sb_entries = Vec::new();
    let mut sb_targets = Vec::new();

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
            let target_path = vfs_url_to_target(&item.vfs_url);
            let badge_str = item.capacity_badge.as_deref().unwrap_or("");
            let is_sel = !target_path.is_empty() && target_path == active_dir;
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
                let c_sel = !child_target.is_empty() && child_target == active_dir;
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
    window.set_sidebar_items(ModelRc::new(VecModel::from(sb_entries)));

    // Left & Right headers, interactive breadcrumbs, and tabs
    let root_label = crate::resolve_dynamic_root_label();
    let l_cur_path = rt.current_pane_path(false);
    let r_cur_path = rt.current_pane_path(true);

    let l_crumbs = rt.app_state.left_header.formatted_breadcrumbs.clone();
    let l_sub = rt.app_state.left_header.status_subtitle.clone();
    let l_raw_segs = crate::PathAndStatusHeader::clickable_segments(root_label, &l_cur_path);
    let l_seg_len = l_raw_segs.len();
    let l_segs: Vec<BreadcrumbSegmentSlint> = l_raw_segs
        .into_iter()
        .enumerate()
        .map(|(idx, (lbl, target))| BreadcrumbSegmentSlint {
            label: SharedString::from(lbl),
            target_path: SharedString::from(target),
            is_last: idx + 1 == l_seg_len,
        })
        .collect();
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
    window.set_left_filter_query(l_filter.into());

    let l_tabs: Vec<TabEntrySlint> = rt
        .app_state
        .left_tabs
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let q = t.inline_filter.trim();
            let is_search = !q.is_empty();
            let tab_title = if is_search {
                format!("Search: \"{q}\"")
            } else {
                t.title.clone()
            };
            TabEntrySlint {
                title: SharedString::from(tab_title),
                is_active: idx == rt.app_state.left_active_tab,
                is_search,
            }
        })
        .collect();
    window.set_left_tabs(ModelRc::new(VecModel::from(l_tabs)));

    let r_crumbs = rt.app_state.right_header.formatted_breadcrumbs.clone();
    let r_sub = rt.app_state.right_header.status_subtitle.clone();
    let r_raw_segs = crate::PathAndStatusHeader::clickable_segments(root_label, &r_cur_path);
    let r_seg_len = r_raw_segs.len();
    let r_segs: Vec<BreadcrumbSegmentSlint> = r_raw_segs
        .into_iter()
        .enumerate()
        .map(|(idx, (lbl, target))| BreadcrumbSegmentSlint {
            label: SharedString::from(lbl),
            target_path: SharedString::from(target),
            is_last: idx + 1 == r_seg_len,
        })
        .collect();
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
    window.set_right_filter_query(r_filter.into());

    let r_tabs: Vec<TabEntrySlint> = rt
        .app_state
        .right_tabs
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            let q = t.inline_filter.trim();
            let is_search = !q.is_empty();
            let tab_title = if is_search {
                format!("Search: \"{q}\"")
            } else {
                t.title.clone()
            };
            TabEntrySlint {
                title: SharedString::from(tab_title),
                is_active: idx == rt.app_state.right_active_tab,
                is_search,
            }
        })
        .collect();
    window.set_right_tabs(ModelRc::new(VecModel::from(r_tabs)));

    // Rows, 3-Column Polaroid Icon Triples, and Miller Parent Columns
    let l_rows = build_slint_rows_from_flattened(&rt.left_entries, &rt.left_selected_indices);
    let l_triples = build_icon_triples(&l_rows);
    let l_parent_sel = rt
        .left_parent_entries
        .iter()
        .position(|m| m.path == l_cur_path);
    let l_parent_rows = build_slint_rows_from_meta(&rt.left_parent_entries, l_parent_sel);
    window.set_left_rows(ModelRc::new(VecModel::from(l_rows)));
    window.set_left_icon_triples(ModelRc::new(VecModel::from(l_triples)));
    window.set_left_parent_rows(ModelRc::new(VecModel::from(l_parent_rows)));

    let r_rows = build_slint_rows_from_flattened(&rt.right_entries, &rt.right_selected_indices);
    let r_triples = build_icon_triples(&r_rows);
    let r_parent_sel = rt
        .right_parent_entries
        .iter()
        .position(|m| m.path == r_cur_path);
    let r_parent_rows = build_slint_rows_from_meta(&rt.right_parent_entries, r_parent_sel);
    window.set_right_rows(ModelRc::new(VecModel::from(r_rows)));
    window.set_right_icon_triples(ModelRc::new(VecModel::from(r_triples)));
    window.set_right_parent_rows(ModelRc::new(VecModel::from(r_parent_rows)));

    // Inspector
    window.set_show_inspector(rt.app_state.inspector_open);
    if let Some(vm) = &rt.app_state.inspector_vm {
        let title_s = vm.title.clone();
        let sub_s = vm.subtitle.clone();
        let path_s = vm.general.path.clone();
        let size_s = vm.general.size.clone();
        let mod_s = vm.general.modified.clone();
        let cre_s = vm.general.created.clone();
        let add_s = vm.general.added.clone();
        let ext_s = vm.general.extension.clone();
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
    }

    if let Some(sel) = rt.selected_metadata() {
        let color_idx = color_label_to_index(sel.color_label);
        let is_img = is_image_extension(sel.extension.as_deref());
        window.set_inspector_color_index(color_idx);
        window.set_inspector_is_dir(sel.is_dir);
        window.set_inspector_is_image(is_img);

        let preview_text = build_inspector_preview(rt, sel);
        window.set_inspector_preview_text(preview_text.into());
    }

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
}

fn build_inspector_preview(rt: &GuiRuntime, sel: &FileMetadata) -> String {
    if sel.is_dir {
        return format!("Folder\n{}", sel.path);
    }
    let vfs = rt.backend_for_path(&sel.path);
    if let Ok(bytes) = vfs.read_range(&sel.path, 0, 256) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let snippet: String = text.lines().take(5).collect::<Vec<_>>().join("\n");
            if !snippet.trim().is_empty() {
                return snippet;
            }
        }
        if !bytes.is_empty() {
            return format_hex_dump(&bytes, 4).join("\n");
        }
    }
    format!(
        "{}\n{} ({})",
        sel.kind,
        sel.permissions_matrix().to_symbolic_string(),
        sel.permissions_matrix().to_octal_string()
    )
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

fn build_slint_rows_from_flattened(
    entries: &[FlattenedRowEntry],
    selected_indices: &HashSet<usize>,
) -> Vec<FileRowSlint> {
    entries
        .iter()
        .enumerate()
        .take(320)
        .map(|(idx, entry)| {
            meta_to_slint_row(
                &entry.meta,
                entry.depth,
                entry.is_expanded,
                selected_indices.contains(&idx),
            )
        })
        .collect()
}

fn build_slint_rows_from_meta(
    entries: &[FileMetadata],
    selected_idx: Option<usize>,
) -> Vec<FileRowSlint> {
    entries
        .iter()
        .enumerate()
        .take(320)
        .map(|(idx, meta)| meta_to_slint_row(meta, 0, false, selected_idx == Some(idx)))
        .collect()
}

fn meta_to_slint_row(
    meta: &FileMetadata,
    depth: i32,
    is_expanded: bool,
    is_selected: bool,
) -> FileRowSlint {
    let hex = meta.color_label.hex_color().unwrap_or("");
    let tag_name = color_label_to_name(meta.color_label);
    let brush = color_label_to_slint_color(meta.color_label);
    let tint = color_label_to_tint_color(meta.color_label);
    let ext_ref = meta.extension.as_deref();
    let is_exec = !meta.is_dir && (meta.permissions & 0o111 != 0);
    FileRowSlint {
        name: SharedString::from(meta.display_name()),
        path: SharedString::from(&meta.path),
        ext_badge: SharedString::from(format_ext_badge(ext_ref)),
        size_text: SharedString::from(meta.formatted_size()),
        modified_text: SharedString::from(meta.formatted_iso_datetime()),
        kind_text: SharedString::from(&meta.kind),
        color_tag_hex: SharedString::from(hex),
        color_tag_name: SharedString::from(tag_name),
        color_tag_brush: brush.into(),
        color_tag_tint: tint.into(),
        has_color_tag: meta.color_label != ColorLabel::None,
        is_dir: meta.is_dir,
        is_image: is_image_extension(ext_ref),
        is_executable: is_exec,
        is_expanded,
        is_selected,
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
    let rt = Rc::new(RefCell::new(GuiRuntime::new(initial_state)));

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
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
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
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Draggable 4px Dual-Pane Splitter Handle (Section 2.2)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_adjust_split_ratio(move |delta_px| {
            let mut guard = rt.borrow_mut();
            let delta = delta_px / 960.0;
            let next_ratio = (guard.app_state.split_ratio + delta).clamp(0.25, 0.75);
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
            let target = guard
                .sidebar_targets
                .get(idx as usize)
                .map(|(_, p)| p.clone())
                .unwrap_or_default();
            if target == "connect://dialog" {
                if let Some(w) = win_weak.upgrade() {
                    w.set_show_connect_modal(true);
                }
                return;
            }
            if !target.is_empty() {
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
                }
                for sec in &mut guard.app_state.sidebar_sections {
                    sec.items.retain(|i| i.id != item_id);
                }
                if guard.saved_connections.remove_profile(&item_id) {
                    let _ = guard.saved_connections.save_default();
                }
                let msg = format!("[DISCONNECT] Disconnected {item_id}");
                guard.logs.push(msg);
                let toast = format!("Disconnected {item_id}");
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
            if let Some(w) = win_weak.upgrade() {
                w.set_sidebar_context_idx(idx);
                w.set_sidebar_context_x(x);
                w.set_sidebar_context_y(y);
                w.set_sidebar_context_label(found_label.into());
                w.set_sidebar_context_section(found_sec.into());
                w.set_sidebar_context_icon(found_icon.into());
                w.set_sidebar_context_can_eject(found_eject);
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
                    let title = vfs_display_title(&target_path);
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
                    let title = vfs_display_title(&target_path);
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
                    } else if sec_name == "Connections" {
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
                    if let Some((prefix, rel)) = split_remote_uri(&target_path) {
                        let prof_opt = guard
                            .saved_connections
                            .connections
                            .iter()
                            .find(|p| p.id == item_id || profile_to_vfs_url(p).starts_with(&prefix))
                            .cloned();
                        if let Some(prof) = prof_opt {
                            if let Ok(spec) =
                                TerminalIntegration::build_remote_ssh_terminal(&prof, &rel)
                            {
                                let _ = Command::new(&spec.program).args(&spec.args).spawn();
                            }
                        }
                        let toast = format!("Launched SSH terminal for {item_label}");
                        guard.logs.push(format!("[SSH] {toast}"));
                        guard.show_toast("info", toast);
                    } else {
                        if cfg!(target_os = "macos") {
                            let target = if Path::new(&target_path).exists() {
                                &target_path
                            } else {
                                "/"
                            };
                            let args = ["-a", "Terminal", target];
                            let _ = Command::new("open").args(args).spawn();
                        } else if let Ok(spec) =
                            TerminalIntegration::build_local_terminal(&target_path)
                        {
                            let _ = Command::new(&spec.program).args(&spec.args).spawn();
                        }
                        let toast = format!("Opened terminal at {target_path}");
                        guard.logs.push(format!("[TERM] {toast}"));
                        guard.show_toast("info", toast);
                    }
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
                    let vfs_url = if target_path.contains("://") {
                        target_path.clone()
                    } else {
                        format!("file://{target_path}")
                    };
                    let fav_sec = guard
                        .app_state
                        .sidebar_sections
                        .iter_mut()
                        .find(|s| s.title == "Favorites");
                    if let Some(sec) = fav_sec {
                        let exists = sec.items.iter().any(|i| i.vfs_url == vfs_url);
                        if !exists {
                            let new_id = format!("fav-pin-{}", sec.items.len() + 1);
                            sec.items.push(SidebarItem {
                                id: new_id,
                                label: item_label.clone(),
                                vfs_url,
                                icon_kind: "folder".to_string(),
                                capacity_badge: None,
                                can_eject: false,
                                children: vec![],
                                expanded: false,
                            });
                        }
                    }
                    let toast = format!("Added \"{item_label}\" to Favorites");
                    guard.show_toast("success", toast);
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
                    }
                    for sec in &mut guard.app_state.sidebar_sections {
                        sec.items.retain(|i| i.id != item_id);
                    }
                    if guard.saved_connections.remove_profile(&item_id) {
                        let _ = guard.saved_connections.save_default();
                    }
                    let toast = format!("Removed \"{item_label}\"");
                    guard.logs.push(format!("[EJECT] {toast}"));
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
        window.on_set_sidebar_item_color(move |c_idx| {
            let mut guard = rt.borrow_mut();
            let u_idx = guard.sidebar_context_idx;
            let Some((item_id, _)) = guard.sidebar_targets.get(u_idx).cloned() else {
                return;
            };
            let color = index_to_color_label(c_idx);
            if color == ColorLabel::None {
                guard.sidebar_colors.remove(&item_id);
            } else {
                guard.sidebar_colors.insert(item_id.clone(), color);
            }
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
            }
            let fav_sec = guard
                .app_state
                .sidebar_sections
                .iter_mut()
                .find(|s| s.title == "Favorites");
            if let Some(sec) = fav_sec {
                if !sec.items.iter().any(|i| i.vfs_url == vfs_url) {
                    sec.items.push(SidebarItem {
                        id: fav_id,
                        label: label.clone(),
                        vfs_url,
                        icon_kind: "globe".to_string(),
                        capacity_badge: None,
                        can_eject: true,
                        children: vec![],
                        expanded: false,
                    });
                }
            }
            let toast = format!("Added \"{label}\" to Favorites");
            guard.show_toast("success", toast);
            sync_window_from_runtime(&w, &mut guard);
        });
    }

    // Pin current directory to Sidebar Favorites
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_add_current_to_favorites(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let cur_path = guard.current_pane_path(is_right);
            let label = vfs_display_title(&cur_path);
            let vfs_url = if cur_path.contains("://") {
                cur_path
            } else {
                format!("file://{cur_path}")
            };
            let fav_sec = guard
                .app_state
                .sidebar_sections
                .iter_mut()
                .find(|s| s.title == "Favorites");
            if let Some(sec) = fav_sec {
                let exists = sec.items.iter().any(|i| i.vfs_url == vfs_url);
                if !exists {
                    sec.items.push(SidebarItem {
                        id: format!("fav-pin-{}", sec.items.len() + 1),
                        label,
                        vfs_url,
                        icon_kind: "folder".to_string(),
                        capacity_badge: None,
                        can_eject: false,
                        children: vec![],
                        expanded: false,
                    });
                }
            }
            if let Some(w) = win_weak.upgrade() {
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
            } else if !is_right && idx < guard.app_state.left_tabs.len() {
                guard.app_state.left_active_tab = idx;
            }
            guard.reload_pane_entries(is_right);
            if let Some(w) = win_weak.upgrade() {
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
            let title = vfs_display_title(&path);
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
            if let Some(w) = win_weak.upgrade() {
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
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Select file row (with Shift/Cmd multi-selection and 500ms double-click detection)
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_file_row(move |is_right, row_idx| {
            let mut guard = rt.borrow_mut();
            let idx = row_idx as usize;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.apply_row_selection(use_right, idx, false, false);
            guard.refresh_headers_and_inspector();
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_select_file_row_mod(move |is_right, row_idx, is_shift, is_cmd| {
            let mut guard = rt.borrow_mut();
            let idx = row_idx as usize;
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            guard.apply_row_selection(use_right, idx, is_shift, is_cmd);

            if is_shift || is_cmd {
                guard.last_click = None;
                guard.refresh_headers_and_inspector();
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
                return;
            }

            let clicked_path = if use_right {
                guard.right_entries.get(idx).map(|r| r.meta.path.clone())
            } else {
                guard.left_entries.get(idx).map(|r| r.meta.path.clone())
            };

            let now = Instant::now();
            let is_double_click = match (&guard.last_click, &clicked_path) {
                (Some((prev_pane, prev_path, prev_time)), Some(cur_path)) => {
                    let elapsed = now.duration_since(*prev_time);
                    let within_window = elapsed <= Duration::from_millis(500);
                    *prev_pane == use_right && prev_path == cur_path && within_window
                }
                _ => false,
            };

            if is_double_click {
                guard.last_click = None;
                guard.open_entry_at(use_right, Some(idx));
            } else {
                guard.last_click = clicked_path.map(|p| (use_right, p, now));
                guard.refresh_headers_and_inspector();
            }

            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_shift_drag_select(move |is_right, target_idx| {
            let mut guard = rt.borrow_mut();
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
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Explicit open file row callback
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_file_row(move |is_right, row_idx| {
            let mut guard = rt.borrow_mut();
            let use_right = is_right && guard.app_state.dual_pane_enabled;
            let opt_idx = (row_idx >= 0).then_some(row_idx as usize);
            guard.open_entry_at(use_right, opt_idx);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
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
                    .filter(|r| r.meta.is_dir)
                    .map(|r| r.meta.path.clone())
            } else {
                guard
                    .left_entries
                    .get(idx)
                    .filter(|r| r.meta.is_dir)
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
            guard.refresh_headers_and_inspector();
            let sel_list = guard.selected_metadata_list(use_right);
            let target_name = if sel_list.len() > 1 {
                format!("{} items selected", sel_list.len())
            } else {
                sel_list.first().map(|m| m.name.clone()).unwrap_or_default()
            };
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
                w.set_context_target_name(target_name.into());
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
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let sel_list = guard.selected_metadata_list(is_right);
            let sel_meta = sel_list.first().cloned();

            match action.as_str() {
                "open" => {
                    guard.open_entry_at(is_right, None);
                }
                "open_new_tab" => {
                    if let Some(meta) = sel_meta {
                        let target_dir = if meta.is_dir {
                            meta.path
                        } else {
                            vfs_parent_dir(&meta.path).unwrap_or_else(|| "/".to_string())
                        };
                        let title = vfs_display_title(&target_dir);
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
                    }
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
                "get_info" | "quick_look" => {
                    guard.app_state.inspector_open = true;
                    if let Some(w) = win_weak.upgrade() {
                        w.set_inspector_tab_index(0);
                    }
                }
                "rename" => {
                    if let (Some(meta), Some(w)) = (sel_meta, win_weak.upgrade()) {
                        let title = format!("Rename \"{}\"", meta.name);
                        w.set_input_modal_mode("rename".into());
                        w.set_input_modal_title(title.into());
                        w.set_input_modal_value(meta.name.into());
                        w.set_show_input_modal(true);
                        return;
                    }
                }
                "new_folder" => {
                    if let Some(w) = win_weak.upgrade() {
                        w.set_input_modal_mode("new_folder".into());
                        w.set_input_modal_title("New Folder".into());
                        w.set_input_modal_value("Untitled Folder".into());
                        w.set_show_input_modal(true);
                        return;
                    }
                }
                "compress" => {
                    if let Some(meta) = sel_meta {
                        let parent = Path::new(&meta.path)
                            .parent()
                            .and_then(|p| p.to_str())
                            .unwrap_or("/");
                        let trimmed_parent = parent.trim_end_matches('/');
                        let archive_path = format!("{trimmed_parent}/{}.tar.gz", meta.name);
                        let _ = Command::new("tar")
                            .args(["-czf", &archive_path, "-C", parent, &meta.name])
                            .status();
                        let label = format!("Compressed {}.tar.gz", meta.name);
                        let sz = meta.size.unwrap_or(4096);
                        guard
                            .logs
                            .push(format!("[COMPRESS] Created {archive_path}"));
                        guard.start_tracked_task(&label, &archive_path, sz, label.clone(), "info");
                        guard.reload_pane_entries(is_right);
                    }
                }
                "duplicate" => {
                    let mut dup_count = 0usize;
                    let mut total_bytes = 0u64;
                    let mut first_name = String::new();
                    for meta in &sel_list {
                        if let Ok(dup_path) = duplicate_path_on_disk(&guard.local_vfs, meta) {
                            if first_name.is_empty() {
                                first_name = meta.name.clone();
                            }
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
                        guard.reload_pane_entries(is_right);
                    }
                }
                "make_alias" => {
                    if let Some(meta) = sel_meta {
                        let alias_path = format!("{} alias", meta.path);
                        let _ = std::os::unix::fs::symlink(&meta.path, &alias_path);
                        guard.logs.push(format!("[ALIAS] -> {alias_path}"));
                        let toast = format!("Created symlink for {}", meta.name);
                        guard.show_toast("success", toast);
                        guard.reload_pane_entries(is_right);
                    }
                }
                "reveal_in_finder" => {
                    if let Some(meta) = sel_meta {
                        reveal_in_system_file_manager(&meta.path);
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
                    if guard.paste_clipboard_into(&dst_dir) > 0 {
                        guard.reload_pane_entries(false);
                        guard.reload_pane_entries(true);
                    }
                }
                "copy_path" if !sel_list.is_empty() => {
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
                _ => {}
            }

            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Toolbar New Folder & Rename Modal triggers
    {
        let win_weak = window.as_weak();
        window.on_open_new_folder_modal(move || {
            if let Some(w) = win_weak.upgrade() {
                w.set_input_modal_mode("new_folder".into());
                w.set_input_modal_title("New Folder".into());
                w.set_input_modal_value("Untitled Folder".into());
                w.set_show_input_modal(true);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_rename_modal(move || {
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
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_confirm_input_modal(move |val| {
            let Some(w) = win_weak.upgrade() else {
                return;
            };
            let trimmed = val.trim();
            if trimmed.is_empty() {
                return;
            }
            let mut guard = rt.borrow_mut();
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
                guard.reload_pane_entries(false);
                guard.reload_pane_entries(true);
            }
            if let Some(w) = win_weak.upgrade() {
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
            let guard = rt.borrow();
            let items = guard.drag_items_for_row(from_right, row_idx.max(0) as usize);
            if items.is_empty() {
                return;
            }
            let ghost_label = if items.len() == 1 {
                items[0].name.clone()
            } else {
                format!("{} items", items.len())
            };
            let coords = (
                abs_x,
                abs_y,
                w.get_drag_left_pane_w(),
                w.get_drag_left_scroll_y(),
                w.get_drag_right_scroll_y(),
            );
            let res = guard.resolve_drop_target(from_right, &items, coords);
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

            let mut guard = rt.borrow_mut();
            let items = guard.drag_items_for_row(from_right, row_idx.max(0) as usize);
            if items.is_empty() {
                return;
            }
            let coords = (
                abs_x,
                abs_y,
                w.get_drag_left_pane_w(),
                w.get_drag_left_scroll_y(),
                w.get_drag_right_scroll_y(),
            );
            let res = guard.resolve_drop_target(from_right, &items, coords);
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
            let idx = row_idx as usize;
            let in_sel = if from_right {
                guard.right_selected_indices.contains(&idx)
            } else {
                guard.left_selected_indices.contains(&idx)
            };
            let items = if in_sel {
                guard.selected_metadata_list(from_right)
            } else {
                let single = if from_right {
                    guard.right_entries.get(idx).map(|r| r.meta.clone())
                } else {
                    guard.left_entries.get(idx).map(|r| r.meta.clone())
                };
                single.into_iter().collect()
            };
            if items.is_empty() {
                return;
            }
            let dst_dir = guard.current_pane_path(!from_right);
            let mut copied = 0usize;
            let mut total_bytes = 0u64;
            let first_name = items[0].name.clone();
            for src_meta in &items {
                let dst_path = join_vfs_path(&dst_dir, &src_meta.name);
                if dst_path != src_meta.path
                    && guard.vfs_copy_entry(&src_meta.path, &dst_path).is_ok()
                {
                    total_bytes += src_meta.size.unwrap_or(4096);
                    let msg = format!("[DRAG-DROP] {} -> {dst_path}", src_meta.name);
                    guard.logs.push(msg);
                    copied += 1;
                }
            }
            if copied > 0 {
                let dst_title = vfs_display_title(&dst_dir);
                let label = if copied == 1 {
                    format!("Copied {first_name}")
                } else {
                    format!("Copied {copied} items")
                };
                let toast = format!("{label} to {dst_title}");
                guard.start_tracked_task(&label, &dst_dir, total_bytes, toast, "success");
                guard.reload_pane_entries(!from_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_drop_row_on_tab(
            move |from_right, row_idx, target_is_right, target_tab_idx| {
                let mut guard = rt.borrow_mut();
                let idx = row_idx as usize;
                let in_sel = if from_right {
                    guard.right_selected_indices.contains(&idx)
                } else {
                    guard.left_selected_indices.contains(&idx)
                };
                let items = if in_sel {
                    guard.selected_metadata_list(from_right)
                } else {
                    let single = if from_right {
                        guard.right_entries.get(idx).map(|r| r.meta.clone())
                    } else {
                        guard.left_entries.get(idx).map(|r| r.meta.clone())
                    };
                    single.into_iter().collect()
                };
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
                let mut copied = 0usize;
                let mut total_bytes = 0u64;
                let first_name = items[0].name.clone();
                for src_meta in &items {
                    let dst_path = join_vfs_path(&dst_dir, &src_meta.name);
                    if dst_path != src_meta.path
                        && guard.vfs_copy_entry(&src_meta.path, &dst_path).is_ok()
                    {
                        total_bytes += src_meta.size.unwrap_or(4096);
                        let msg = format!("[TAB-DROP] {} -> {dst_path}", src_meta.name);
                        guard.logs.push(msg);
                        copied += 1;
                    }
                }
                if copied > 0 {
                    let dst_title = vfs_display_title(&dst_dir);
                    let label = if copied == 1 {
                        format!("Copied {first_name}")
                    } else {
                        format!("Copied {copied} items")
                    };
                    let toast = format!("{label} to tab {dst_title}");
                    guard.start_tracked_task(&label, &dst_dir, total_bytes, toast, "success");
                    guard.reload_pane_entries(false);
                    guard.reload_pane_entries(true);
                    if let Some(w) = win_weak.upgrade() {
                        sync_window_from_runtime(&w, &mut guard);
                    }
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
            if guard.is_right_effective() {
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
            if let Some(w) = win_weak.upgrade() {
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
            if let Some(w) = win_weak.upgrade() {
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
                for meta in &selected_list {
                    let _ = guard.mounted_vfs.set_color_label(&meta.path, label);
                    let _ = guard.local_vfs.set_color_label(&meta.path, label);
                }
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
                    guard.reload_pane_entries(false);
                    if let Some(w) = win_weak.upgrade() {
                        sync_window_from_runtime(&w, &mut guard);
                    }
                }
                return;
            }

            let dst_dir = guard.current_pane_path(!from_right);
            let trimmed_dst = dst_dir.trim_end_matches('/');
            if sel_list.len() == 1 {
                let dst_path = format!("{trimmed_dst}/{}", src_meta.name);
                let existing = guard
                    .mounted_vfs
                    .metadata(&dst_path)
                    .ok()
                    .or_else(|| guard.local_vfs.metadata(&dst_path).ok());

                if let Some(ex_meta) = existing {
                    let src_dt = src_meta.formatted_modified_datetime();
                    let ex_dt = ex_meta.formatted_modified_datetime();
                    let modal = ConflictModalDescriptor::from_collision(
                        &src_meta, &ex_meta, &src_dt, &ex_dt,
                    );
                    guard.pending_conflict = Some((src_meta, ex_meta, !from_right));
                    if let Some(w) = win_weak.upgrade() {
                        let new_dt = modal.new_item.formatted_modified;
                        let new_sz = modal.new_item.formatted_size;
                        let ex_dt = modal.existing_item.formatted_modified;
                        let ex_sz = modal.existing_item.formatted_size;
                        w.set_conflict_headline(modal.headline.into());
                        w.set_conflict_subheadline(modal.subheadline.into());
                        w.set_conflict_new_date(new_dt.into());
                        w.set_conflict_new_size(new_sz.into());
                        w.set_conflict_existing_date(ex_dt.into());
                        w.set_conflict_existing_size(ex_sz.into());
                        w.set_show_conflict_modal(true);
                    }
                    return;
                }
            }

            let mut copied = 0usize;
            let mut total_bytes = 0u64;
            for item in &sel_list {
                let dst_path = join_vfs_path(&dst_dir, &item.name);
                if guard.vfs_copy_entry(&item.path, &dst_path).is_ok() {
                    total_bytes += item.size.unwrap_or(4096);
                    let msg = format!("[TRANSFER] {} -> {dst_path}", item.name);
                    guard.logs.push(msg);
                    copied += 1;
                }
            }
            if copied > 0 {
                let dst_title = vfs_display_title(&dst_dir);
                let label = if copied == 1 {
                    format!("Copied {}", src_meta.name)
                } else {
                    format!("Copied {copied} items")
                };
                let toast = format!("{label} to {dst_title}");
                guard.start_tracked_task(&label, &dst_dir, total_bytes, toast, "success");
                guard.reload_pane_entries(!from_right);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
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
            let mut moved = 0usize;
            let mut total_bytes = 0u64;
            let first_name = sel_list[0].name.clone();
            for item in &sel_list {
                let dst_path = join_vfs_path(&dst_dir, &item.name);
                if guard.vfs_rename(&item.path, &dst_path).is_ok() {
                    total_bytes += item.size.unwrap_or(4096);
                    let msg = format!("[MOVE] {} -> {dst_path}", item.name);
                    guard.logs.push(msg);
                    moved += 1;
                }
            }
            if moved > 0 {
                let dst_title = vfs_display_title(&dst_dir);
                let label = if moved == 1 {
                    format!("Moved {first_name}")
                } else {
                    format!("Moved {moved} items")
                };
                let toast = format!("{label} to {dst_title}");
                guard.start_tracked_task(&label, &dst_dir, total_bytes, toast, "success");
                guard.reload_pane_entries(false);
                guard.reload_pane_entries(true);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
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
            let pending = guard.pending_conflict.take();
            if let Some((src_meta, ex_meta, dst_is_right)) = pending {
                let task = TransferTask {
                    id: format!("tx-{}", guard.transfers.len() + 1),
                    priority: TransferPriority::Foreground,
                    source_path: src_meta.path.clone(),
                    destination_path: ex_meta.path.clone(),
                    total_bytes: src_meta.size.unwrap_or(0),
                    transferred_bytes: 0,
                    chunk_size: DEFAULT_TRANSFER_CHUNK_SIZE,
                    max_retries: 3,
                    status: TransferStatus::Queued,
                    error_message: None,
                };
                let res = guard.transfer_engine.execute_task(
                    task,
                    &guard.local_vfs,
                    &guard.local_vfs,
                    policy,
                    false,
                );
                if let Ok(done) = res {
                    let dst = &done.destination_path;
                    let msg = format!("[CONFLICT] {:?} -> {dst}", policy);
                    guard.logs.push(msg);
                    guard.transfers.push(done);
                }
                guard.reload_pane_entries(dst_is_right);
            }
            if let Some(w) = win_weak.upgrade() {
                w.set_show_conflict_modal(false);
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
                let title = vfs_display_title(&cur_dir);
                let label = format!("Refreshed {title}");
                guard.start_tracked_task(&label, &cur_dir, 4096, label.clone(), "info");
            }
            guard.reload_pane_entries(false);
            guard.reload_pane_entries(true);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }

    // Open Terminal Here
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_open_terminal_here(move || {
            let mut guard = rt.borrow_mut();
            let is_right = guard.is_right_effective();
            let dir = guard.current_pane_path(is_right);
            if cfg!(target_os = "macos") {
                let target = if Path::new(&dir).exists() { &dir } else { "/" };
                let args = ["-a", "Terminal", target];
                let _ = Command::new("open").args(args).spawn();
            } else if let Ok(spec) = TerminalIntegration::build_local_terminal(".") {
                let _ = Command::new(&spec.program).args(&spec.args).spawn();
            }
            let msg = format!("[TERM] Opened terminal at {dir}");
            guard.logs.push(msg);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
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
                    guard.app_state.inspector_open = true;
                    w.set_inspector_tab_index(2);
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
            let mut need_transfer_sync = false;

            if guard.active_task_ticks > 0 {
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
            }

            if guard.toast_ticks > 0 {
                guard.toast_ticks -= 1;
                if guard.toast_ticks == 0 {
                    w.set_show_toast(false);
                }
            }

            if need_transfer_sync {
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

    window.run().map_err(|e| e.to_string())
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

    // Escape closes active popovers, modals, or path edit mode
    if key == "\u{001b}" {
        w.set_show_context_menu(false);
        w.set_show_sidebar_context_menu(false);
        w.set_show_sibling_dropdown(false);
        w.set_show_tasks_popover(false);
        w.set_show_input_modal(false);
        w.set_show_delete_modal(false);
        w.set_show_connect_modal(false);
        w.set_left_path_edit_mode(false);
        w.set_right_path_edit_mode(false);
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

    // Ctrl+F / Cmd+F: Clear/focus inline filter
    if cmd && k_low == "f" {
        return true;
    }

    // Space: Toggle Quick Look / Inspector Panel
    if key == " " && !cmd && !alt {
        w.invoke_toggle_inspector();
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

    // UpArrow (`\u{f700}`) / DownArrow (`\u{f701}`): Move or Shift-extend selection up/down
    if key == "\u{f700}" || key == "\u{f701}" {
        let mut guard = rt.borrow_mut();
        let is_right = guard.is_right_effective();
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
            let next = if key == "\u{f700}" {
                cur.saturating_sub(1)
            } else {
                (cur + 1).min(len - 1)
            };
            guard.apply_row_selection(is_right, next, shift, false);
            guard.refresh_headers_and_inspector();
            sync_window_from_runtime(w, &mut guard);
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
}
