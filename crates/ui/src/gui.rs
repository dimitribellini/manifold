//! Interactive Slint Desktop GUI Controller for Project Manifold.
//!
//! Binds [`ManifoldAppState`], [`LocalFsBackend`], [`RemoteVfsBackend`], [`TransferEngine`],
//! [`SyncExecutionPlan`], [`BatchRenamer`], and [`FileInspectorViewModel`] to the declarative
//! Slint window defined in `crates/ui/ui/manifold.slint`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use manifold_config::{
    AppPreferences, AuthType, ConnectionProfile, ConnectionsConfig, CredentialStore,
    FavoriteFolderEntry, PaneViewMode, ProtocolType, SortColumn, SortDirection,
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

struct InteractivePtyShell {
    stdin_tx: mpsc::Sender<Vec<u8>>,
    stdout_rx: mpsc::Receiver<String>,
}

impl InteractivePtyShell {
    fn spawn(initial_cwd: &str) -> Option<Self> {
        let start_dir = if Path::new(initial_cwd).is_dir() {
            initial_cwd
        } else {
            "/"
        };
        let py_script = concat!(
            "import os, pty, select, signal, sys, termios\n",
            "master, slave = pty.openpty()\n",
            "try:\n",
            "    attrs = termios.tcgetattr(slave)\n",
            "    attrs[3] = attrs[3] & ~termios.ECHO\n",
            "    termios.tcsetattr(slave, termios.TCSANOW, attrs)\n",
            "except Exception:\n",
            "    pass\n",
            "env = dict(os.environ)\n",
            "env['PS1'] = ''\n",
            "env['PROMPT_COMMAND'] = ''\n",
            "env['TERM'] = 'dumb'\n",
            "pid = os.fork()\n",
            "if pid == 0:\n",
            "    os.setsid()\n",
            "    os.dup2(slave, 0)\n",
            "    os.dup2(slave, 1)\n",
            "    os.dup2(slave, 2)\n",
            "    if slave > 2:\n",
            "        os.close(slave)\n",
            "    os.close(master)\n",
            "    os.execve('/bin/sh', ['/bin/sh'], env)\n",
            "os.close(slave)\n",
            "while True:\n",
            "    r, _, _ = select.select([0, master], [], [])\n",
            "    if 0 in r:\n",
            "        data = os.read(0, 4096)\n",
            "        if not data:\n",
            "            break\n",
            "        if b'\\x03' in data:\n",
            "            try:\n",
            "                os.killpg(os.getpgid(pid), signal.SIGINT)\n",
            "            except Exception:\n",
            "                pass\n",
            "        else:\n",
            "            os.write(master, data)\n",
            "    if master in r:\n",
            "        try:\n",
            "            out = os.read(master, 4096)\n",
            "            if not out:\n",
            "                break\n",
            "            os.write(1, out)\n",
            "        except OSError:\n",
            "            break\n",
        );
        let child_res = Command::new("python3")
            .args(["-u", "-c", py_script])
            .current_dir(start_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .or_else(|_| {
                Command::new("sh")
                    .current_dir(start_dir)
                    .env("PS1", "")
                    .env("TERM", "dumb")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            });
        let mut child = child_res.ok()?;
        let mut child_in = child.stdin.take()?;
        let mut child_out = child.stdout.take()?;

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        let (stdout_tx, stdout_rx) = mpsc::channel::<String>();

        std::thread::spawn(move || {
            while let Ok(bytes) = stdin_rx.recv() {
                if child_in.write_all(&bytes).is_err() {
                    break;
                }
                let _ = child_in.flush();
            }
            let _ = child.kill();
        });

        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match child_out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let raw = String::from_utf8_lossy(&buf[..n]);
                        let cleaned = strip_ansi_and_control_codes(&raw);
                        if !cleaned.is_empty() && stdout_tx.send(cleaned).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Some(Self {
            stdin_tx,
            stdout_rx,
        })
    }

    fn send_line(&self, line: &str) -> bool {
        let mut payload = line.as_bytes().to_vec();
        payload.push(b'\n');
        self.stdin_tx.send(payload).is_ok()
    }

    fn send_interrupt(&self) {
        let _ = self.stdin_tx.send(vec![0x03]);
    }

    fn sync_cwd(&self, dir_path: &str) {
        if !dir_path.contains("://") && Path::new(dir_path).is_dir() {
            let escaped = dir_path.replace('\'', "'\\''");
            let cmd = format!("cd -- '{escaped}' 2>/dev/null");
            self.send_line(&cmd);
        }
    }
}

fn strip_ansi_and_control_codes(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let _ = chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if ch != '\r' && (ch == '\n' || ch == '\t' || !ch.is_control()) {
            out.push(ch);
        }
    }
    out
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
    last_hovered_cell: Option<(bool, i32)>,
    clipboard_paths: Vec<String>,
    clipboard_cut: bool,
    sidebar_targets: Vec<(String, String)>,
    sidebar_colors: HashMap<String, ColorLabel>,
    sidebar_context_idx: usize,
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
    terminal_output: String,
    terminal_history: Vec<String>,
    terminal_history_pos: usize,
    pty_shell: Option<InteractivePtyShell>,
    thumbnail_cache: HashMap<String, Option<Image>>,
    dir_cache: RefCell<HashMap<String, Vec<FileMetadata>>>,
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
        for fav in &prefs.custom_favorites {
            if fav.color_label != ColorLabel::None {
                sidebar_colors.insert(fav.id.clone(), fav.color_label);
            }
            if let Some(sec) = app_state
                .sidebar_sections
                .iter_mut()
                .find(|s| s.title == "Favorites")
            {
                if !sec.items.iter().any(|i| i.vfs_url == fav.vfs_url) {
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
        let credential_store = CredentialStore::with_os_backend();
        let (worker_tx, worker_rx) = mpsc::channel();
        let init_log = "[INFO] Manifold VFS initialized (Local POSIX + Remote VFS).";
        let keyring_log = "[INFO] Keyring service bound to 'manifold'.";
        let mut init_sel = HashSet::new();
        init_sel.insert(0);
        let init_cwd = app_state
            .left_tabs
            .first()
            .map(|t| t.current_path.clone())
            .unwrap_or_else(|| "/".to_string());
        let pty_shell = InteractivePtyShell::spawn(&init_cwd);
        let init_term_out =
            format!("Manifold Interactive Shell ready ({init_cwd}). Type commands below.\n");
        let mut runtime = Self {
            app_state,
            local_vfs: LocalFsBackend::new(),
            mounted_vfs,
            remote_backends: HashMap::new(),
            saved_connections,
            prefs,
            credential_store,
            worker_tx,
            worker_rx,
            bg_tasks_in_flight: 0,
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
            last_hovered_cell: None,
            clipboard_paths: Vec::new(),
            clipboard_cut: false,
            sidebar_targets: Vec::new(),
            sidebar_colors,
            sidebar_context_idx: 0,
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
            terminal_cwd: init_cwd,
            terminal_output: init_term_out,
            terminal_history: Vec::new(),
            terminal_history_pos: 0,
            pty_shell,
            thumbnail_cache: HashMap::new(),
            dir_cache: RefCell::new(HashMap::new()),
        };
        runtime.reload_pane_entries(false);
        runtime.reload_pane_entries(true);
        runtime
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

    fn sync_terminal_cwd(&mut self) {
        let active_path = self.current_pane_path(self.is_right_effective());
        if self.terminal_cwd != active_path {
            self.terminal_cwd = active_path.clone();
            if let Some(pty) = &self.pty_shell {
                pty.sync_cwd(&active_path);
            }
        }
    }

    fn poll_pty_output(&mut self) -> bool {
        let Some(pty) = &self.pty_shell else {
            return false;
        };
        let mut appended = false;
        while let Ok(chunk) = pty.stdout_rx.try_recv() {
            if !chunk.is_empty() {
                if !self.terminal_output.is_empty() && !self.terminal_output.ends_with('\n') {
                    self.terminal_output.push('\n');
                }
                self.terminal_output.push_str(&chunk);
                appended = true;
            }
        }
        if appended && self.terminal_output.len() > 12000 {
            let drain_to = self.terminal_output.len() - 10000;
            let _ = self.terminal_output.drain(..drain_to);
        }
        appended
    }

    fn pin_folder_to_favorites(&mut self, folder_path: &str, label: &str) {
        let vfs_url = if folder_path.contains("://") {
            folder_path.to_string()
        } else {
            format!("file://{folder_path}")
        };
        let fav_id = format!("fav-pin-{}", self.prefs.custom_favorites.len() + 1);
        let fav_entry = FavoriteFolderEntry {
            id: fav_id.clone(),
            label: label.to_string(),
            vfs_url: vfs_url.clone(),
            icon_kind: "folder".to_string(),
            color_label: ColorLabel::None,
        };
        self.prefs.upsert_favorite(fav_entry);
        let _ = self.prefs.save_default();

        let fav_sec = self
            .app_state
            .sidebar_sections
            .iter_mut()
            .find(|s| s.title == "Favorites");
        if let Some(sec) = fav_sec {
            let exists = sec.items.iter().any(|i| i.vfs_url == vfs_url);
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
        self.invalidate_dir_cache();
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

        // 2. Tab Bar Hit-Testing (Responsive to Pane Width & Tab Count)
        if (36.0..=76.0).contains(&abs_y) {
            let tab_count = if target_is_right {
                self.app_state.right_tabs.len().max(1) as f32
            } else {
                self.app_state.left_tabs.len().max(1) as f32
            };
            let avail_w = pane_w - 48.0 - (tab_count - 1.0).max(0.0) * 6.0;
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

    fn execute_drop_target(&mut self, items: &[FileMetadata], dst_dir: &str, is_move: bool) {
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

        let mut flattened = Vec::new();
        let opts = (inline_filter.as_str(), sort_col, sort_dir);
        self.collect_flattened_rows(&path, 0, &expanded, opts, &mut flattened);

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
            let need_sample = self.app_state.inspector_open
                && !meta.is_dir
                && split_remote_uri(&meta.path).is_none();
            let sample = if need_sample {
                let vfs = self.backend_for_path(&meta.path);
                vfs.read_range(&meta.path, 0, 512).ok()
            } else {
                None
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

fn matches_search_query_lower(meta: &FileMetadata, q_low: &str) -> bool {
    if q_low.is_empty() {
        return true;
    }
    let name = meta.name.to_lowercase();
    if q_low.contains('*') || q_low.contains('?') {
        let anchored = format!("*{q_low}*");
        wildcard_match(q_low, &name) || wildcard_match(&anchored, &name)
    } else {
        name.contains(q_low)
    }
}

#[cfg(test)]
fn matches_search_query(meta: &FileMetadata, query: &str) -> bool {
    let q_low = query.trim().to_lowercase();
    matches_search_query_lower(meta, &q_low)
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
        return Ok(());
    }
    if Path::new(src_path).is_dir() {
        let status = Command::new("cp").args(["-R", src_path, dst_path]).status();
        if !status.is_ok_and(|s| s.success()) {
            return Err("Failed to copy directory".to_string());
        }
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
            } else {
                "DejaVu Sans"
            }
        }
        "monospace" => {
            if cfg!(target_os = "macos") {
                "Menlo"
            } else {
                "DejaVu Sans Mono"
            }
        }
        "serif" => {
            if cfg!(target_os = "macos") {
                "Georgia"
            } else {
                "DejaVu Serif"
            }
        }
        _ => "",
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
    window.set_terminal_cwd(rt.terminal_cwd.clone().into());
    window.set_terminal_output(rt.terminal_output.clone().into());
    let (term_session_lbl, term_prompt_str) =
        if let Some((_, sub)) = split_remote_uri(&rt.terminal_cwd) {
            let conn_title = rt.format_tab_title(&rt.terminal_cwd);
            let leaf = vfs_display_title(&sub);
            (
                format!("Remote SSH Terminal — {conn_title}"),
                format!("ssh:{leaf}$"),
            )
        } else {
            let leaf = vfs_display_title(&rt.terminal_cwd);
            ("Embedded Local Terminal".to_string(), format!("{leaf}$"))
        };
    window.set_terminal_session_label(term_session_lbl.into());
    window.set_terminal_prompt(term_prompt_str.into());

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
    window.set_left_filter_query(l_filter.into());

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
    window.set_right_filter_query(r_filter.into());

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
    let l_rows = build_slint_rows_from_flattened(
        &rt.left_entries,
        &rt.left_selected_indices,
        &mut rt.thumbnail_cache,
        true,
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
        let r_rows = build_slint_rows_from_flattened(
            &rt.right_entries,
            &rt.right_selected_indices,
            &mut rt.thumbnail_cache,
            true,
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
        let new_sz = if src_meta.is_dir {
            "Folder".to_string()
        } else {
            src_meta.formatted_size()
        };
        let ex_sz = if ex_meta.is_dir {
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
        window.set_inspector_color_index(color_idx);
        window.set_inspector_is_dir(sel.is_dir);
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

        let loaded_img = if is_img && !sel.is_dir && split_remote_uri(&sel.path).is_none() {
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
        let line1 = if w > 0 && h > 0 {
            let mp = (f64::from(w) * f64::from(h)) / 1_000_000.0;
            format!("Resolution: {w}×{h} px ({mp:.2} MP) • Format: {ext_up}")
        } else {
            format!("Format: {ext_up} Image • Size: {sz_str}")
        };
        let color_info = match ext_low.as_str() {
            "png" | "webp" | "ico" => "Color Space: RGBA (8-bit/ch) • Alpha Channel",
            "svg" => "Color Space: Vector SVG / XML Scalable",
            "gif" => "Color Space: Indexed 256-Color Palette",
            _ => "Color Space: sRGB (24-bit TrueColor)",
        };
        let line3 = format!(
            "File Size: {sz_str} • Modified: {}",
            meta.formatted_iso_datetime()
        );
        return (
            "Photo / Image".to_string(),
            line1,
            color_info.to_string(),
            line3,
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
                format!("Codec: PCM WAV ({bps}-bit) • {khz:.1} kHz • {ch_lbl} ({ch} ch)"),
                format!("Duration: {dur_str} • Bitrate: {kbps} kbps"),
                format!("Container: RIFF WAVE • Size: {sz_str}"),
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
                format!("Codec: FLAC Lossless ({bps}-bit) • {khz:.1} kHz • {ch} ch"),
                format!("Est. Duration: {dur_str} • Lossless Audio"),
                format!("Container: FLAC • Size: {sz_str}"),
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
            "ID3v2 Audio Tags Present"
        } else {
            "Standard Audio Stream"
        };
        return (
            "Audio / Music".to_string(),
            format!("Codec: {ext_up} Audio • Sample Rate: 44.1 kHz • Stereo (2 ch)"),
            format!("Est. Duration: {dur_str} • Bitrate: ~{est_kbps} kbps"),
            format!("{tag_lbl} • Size: {sz_str}"),
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
            "mkv" => "Matroska Video + Multi-Audio",
            _ => "Digital Video Stream",
        };
        return (
            "Video / Media".to_string(),
            format!("Container: {ext_up} Video • Codec: {codec_lbl}"),
            format!("Est. Duration: {dur_str} • Stream: ~{est_mbps}.0 Mbps"),
            format!(
                "File Size: {sz_str} • Modified: {}",
                meta.formatted_iso_datetime()
            ),
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
        return "search";
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

fn build_slint_rows_from_flattened(
    entries: &[FlattenedRowEntry],
    selected_indices: &HashSet<usize>,
    thumb_cache: &mut HashMap<String, Option<Image>>,
    load_thumbs: bool,
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
                thumb_cache,
                load_thumbs,
            )
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
    let is_img = is_image_extension(ext_ref);
    let is_exec = !meta.is_dir && (meta.permissions & 0o111 != 0);
    let can_thumb = load_thumbs
        && is_img
        && !meta.is_dir
        && split_remote_uri(&meta.path).is_none()
        && meta.size.unwrap_or(0) <= 16 * 1024 * 1024;
    let thumb_opt = if can_thumb {
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
            guard.sync_terminal_cwd();
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
                if guard.prefs.remove_favorite(&item_id) {
                    let _ = guard.prefs.save_default();
                }
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
                    let is_right = guard.is_right_effective();
                    guard.navigate_to_path(is_right, target_path.clone());
                    w.set_show_terminal_drawer(true);
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
                    if guard.prefs.remove_favorite(&item_id) {
                        let _ = guard.prefs.save_default();
                    }
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
            }
            guard.pin_folder_to_favorites(&vfs_url, &label);
            sync_window_from_runtime(&w, &mut guard);
        });
    }

    // Pin current or selected directory to Sidebar Favorites (with persistence)
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
            guard.pin_folder_to_favorites(&target_path, &label);
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
            let sel_dir = guard
                .selected_metadata()
                .filter(|m| m.is_dir)
                .map(|m| (m.path.clone(), m.name.clone()));
            let (target_path, label) = sel_dir.unwrap_or_else(|| {
                let p = guard.current_pane_path(is_right);
                let l = guard.format_tab_title(&p);
                (p, l)
            });
            guard.pin_folder_to_favorites(&target_path, &label);
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
                w.set_show_tab_preview(true);
                sync_selection_and_inspector(&w, &mut guard);
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
            guard.sync_terminal_cwd();
            guard.refresh_headers_and_inspector();
            let sel_list = guard.selected_metadata_list(use_right);
            let target_name = if sel_list.len() > 1 {
                format!("{} items selected", sel_list.len())
            } else {
                sel_list.first().map(|m| m.name.clone()).unwrap_or_default()
            };
            if let Some(w) = win_weak.upgrade() {
                w.set_has_clipboard_item(!guard.clipboard_paths.is_empty());
                sync_selection_and_inspector(&w, &mut guard);
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
            if let Some(w) = win_weak.upgrade() {
                w.set_has_clipboard_item(!guard.clipboard_paths.is_empty());
                sync_selection_and_inspector(&w, &mut guard);
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
                "new_file" => {
                    if let Some(w) = win_weak.upgrade() {
                        w.set_input_modal_mode("new_file".into());
                        w.set_input_modal_title("New File".into());
                        w.set_input_modal_value("untitled.txt".into());
                        w.set_show_input_modal(true);
                        return;
                    }
                }
                "add_favorite" => {
                    let sel_dir = sel_meta.filter(|m| m.is_dir).map(|m| (m.path, m.name));
                    let (target_path, label) = sel_dir.unwrap_or_else(|| {
                        let p = guard.current_pane_path(is_right);
                        let l = guard.format_tab_title(&p);
                        (p, l)
                    });
                    guard.pin_folder_to_favorites(&target_path, &label);
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
                        guard.invalidate_dir_cache();
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
                        guard.invalidate_dir_cache();
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
                        guard.invalidate_dir_cache();
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
                    guard.paste_clipboard_into(&dst_dir);
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
                w.set_input_modal_mode("new_folder".into());
                w.set_input_modal_title("New Folder".into());
                w.set_input_modal_value("Untitled Folder".into());
                w.set_show_input_modal(true);
            }
        });
    }
    {
        let win_weak = window.as_weak();
        window.on_open_new_file_modal(move || {
            if let Some(w) = win_weak.upgrade() {
                w.set_input_modal_mode("new_file".into());
                w.set_input_modal_title("New File".into());
                w.set_input_modal_value("untitled.txt".into());
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
            } else if mode == "new_file" {
                let cur_dir = guard.current_pane_path(is_right);
                let new_file = join_vfs_path(&cur_dir, trimmed);
                if guard.vfs_write_bytes(&new_file, b"").is_ok() {
                    guard.logs.push(format!("[TOUCH] Created {new_file}"));
                    let toast = format!("Created file \"{trimmed}\"");
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
                guard.invalidate_dir_cache();
                guard.reload_pane_entries(false);
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
            if let Some(w) = win_weak.upgrade() {
                let next = !w.get_show_terminal_drawer();
                w.set_show_terminal_drawer(next);
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_clear_terminal(move || {
            let mut guard = rt.borrow_mut();
            let cwd = guard.terminal_cwd.clone();
            guard.terminal_output = format!("Interactive shell ready in {cwd}");
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_run_terminal_command(move |raw_cmd| {
            let cmd = raw_cmd.trim().to_string();
            if cmd.is_empty() {
                return;
            }
            let mut guard = rt.borrow_mut();
            if guard.terminal_history.last() != Some(&cmd) {
                guard.terminal_history.push(cmd.clone());
            }
            guard.terminal_history_pos = guard.terminal_history.len();

            let is_right = guard.is_right_effective();
            if cmd == "clear" {
                let cwd = guard.terminal_cwd.clone();
                guard.terminal_output = format!("Interactive shell ready in {cwd}");
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
                return;
            }
            if cmd == "cd" || cmd.starts_with("cd ") {
                let arg = cmd.strip_prefix("cd").unwrap_or("").trim();
                let cur_dir = guard.current_pane_path(is_right);
                let next_dir = if arg.is_empty() || arg == "~" {
                    expand_tilde_path("~")
                } else if arg == ".." {
                    vfs_parent_dir(&cur_dir).unwrap_or_else(|| "/".to_string())
                } else if arg.starts_with('/') || arg.contains("://") {
                    arg.to_string()
                } else if arg.starts_with("~/") {
                    expand_tilde_path(arg)
                } else {
                    join_vfs_path(&cur_dir, arg)
                };
                guard.navigate_to_path(is_right, next_dir);
                if let Some(w) = win_weak.upgrade() {
                    sync_window_from_runtime(&w, &mut guard);
                }
                return;
            }

            let cwd = guard.terminal_cwd.clone();
            let header = format!("\n$ {cmd}");
            guard.terminal_output.push_str(&header);
            let effective_cmd = normalize_terminal_command(&cmd);

            if let Some((prefix, rel)) = split_remote_uri(&cwd) {
                guard.ensure_remote_connected(&prefix);
                let output_text = if let Some(remote) = guard.remote_backends.get(&prefix) {
                    let sub = if rel.is_empty() { "/" } else { rel.as_str() };
                    match remote.execute_shell_in_dir(sub, &effective_cmd) {
                        Ok(out) => out,
                        Err(e) => format!("Error: {e}"),
                    }
                } else {
                    format!("Error: Remote host {prefix} is not connected.")
                };
                if !output_text.trim().is_empty() {
                    guard.terminal_output.push('\n');
                    guard.terminal_output.push_str(output_text.trim_end());
                }
            } else {
                let sent_to_pty = if let Some(pty) = &mut guard.pty_shell {
                    pty.sync_cwd(&cwd);
                    pty.send_line(&effective_cmd)
                } else {
                    false
                };
                if sent_to_pty {
                    std::thread::sleep(Duration::from_millis(65));
                    let _ = guard.poll_pty_output();
                } else {
                    let run_dir = if Path::new(&cwd).is_dir() {
                        cwd.as_str()
                    } else {
                        "/"
                    };
                    let timeout = Duration::from_millis(2500);
                    let out = run_local_command_bounded(run_dir, &effective_cmd, timeout);
                    if !out.trim().is_empty() {
                        guard.terminal_output.push('\n');
                        guard.terminal_output.push_str(out.trim_end());
                    }
                }
            }

            if guard.terminal_output.len() > 8192 {
                let start = guard.terminal_output.len() - 8192;
                guard.terminal_output = guard.terminal_output[start..].to_string();
            }
            guard.invalidate_dir_cache();
            guard.reload_pane_entries(is_right);
            if let Some(w) = win_weak.upgrade() {
                sync_window_from_runtime(&w, &mut guard);
            }
        });
    }
    {
        let rt = Rc::clone(&rt);
        let win_weak = window.as_weak();
        window.on_terminal_key_event(move |action, _ctrl| {
            let Some(w) = win_weak.upgrade() else {
                return false;
            };
            let mut guard = rt.borrow_mut();
            match action.as_str() {
                "up" => {
                    if !guard.terminal_history.is_empty() && guard.terminal_history_pos > 0 {
                        guard.terminal_history_pos -= 1;
                        let entry = guard.terminal_history[guard.terminal_history_pos].clone();
                        w.set_terminal_command_input(entry.into());
                    }
                    true
                }
                "down" => {
                    if guard.terminal_history_pos < guard.terminal_history.len() {
                        guard.terminal_history_pos += 1;
                        let entry = if guard.terminal_history_pos < guard.terminal_history.len() {
                            guard.terminal_history[guard.terminal_history_pos].clone()
                        } else {
                            String::new()
                        };
                        w.set_terminal_command_input(entry.into());
                    }
                    true
                }
                "ctrl_c" => {
                    if let Some(pty) = &mut guard.pty_shell {
                        pty.send_interrupt();
                    }
                    guard.terminal_output.push_str("\n^C");
                    w.set_terminal_command_input(SharedString::default());
                    w.set_terminal_output(guard.terminal_output.clone().into());
                    true
                }
                "escape" => {
                    if let Some(pty) = &mut guard.pty_shell {
                        pty.send_interrupt();
                    }
                    if !w.get_terminal_command_input().is_empty() {
                        w.set_terminal_command_input(SharedString::default());
                    } else {
                        w.set_show_terminal_drawer(false);
                        w.invoke_focus_main_key_router();
                    }
                    true
                }
                _ => false,
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
            if guard.terminal_cwd != sel_dir {
                guard.terminal_cwd = sel_dir.clone();
                if let Some(pty) = &mut guard.pty_shell {
                    pty.sync_cwd(&sel_dir);
                }
            }
            let msg = format!("[TERM] Opened integrated bottom terminal at {sel_dir}");
            guard.logs.push(msg);
            if let Some(w) = win_weak.upgrade() {
                w.set_show_terminal_drawer(true);
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
            if guard.poll_pty_output() {
                w.set_terminal_output(guard.terminal_output.clone().into());
            }
            let mut need_transfer_sync = false;
            let mut need_full_sync = false;

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

    // Escape closes active popovers, modals, hover preview, path edit mode, or terminal drawer
    if key == "\u{001b}" {
        let had_overlay = w.get_show_app_menu()
            || w.get_show_context_menu()
            || w.get_show_sidebar_context_menu()
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
        w.set_show_context_menu(false);
        w.set_show_sidebar_context_menu(false);
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
                if let Some(pty) = &mut rt.borrow_mut().pty_shell {
                    pty.send_interrupt();
                }
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
                guard.right_entries.get(cur).is_some_and(|r| r.meta.is_dir)
            } else {
                guard.left_entries.get(cur).is_some_and(|r| r.meta.is_dir)
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

fn normalize_terminal_command(raw_cmd: &str) -> String {
    let trimmed = raw_cmd.trim();
    let is_interactive_top = trimmed == "top"
        || (trimmed.starts_with("top ") && !trimmed.contains("-l") && !trimmed.contains("-b"));
    if is_interactive_top {
        if cfg!(target_os = "macos") {
            return "top -l 1 -n 25".to_string();
        }
        return "top -b -n 1 | head -n 35".to_string();
    }
    if trimmed == "htop" || trimmed == "btop" {
        return "ps aux | head -n 30".to_string();
    }
    if trimmed.starts_with("ping ") && !trimmed.contains(" -c ") {
        let rest = trimmed.strip_prefix("ping ").unwrap_or("").trim();
        return format!("ping -c 4 {rest}");
    }
    if trimmed.starts_with("tail -f ") {
        let rest = trimmed.strip_prefix("tail -f ").unwrap_or("").trim();
        return format!("tail -n 50 {rest}");
    }
    if trimmed == "yes" || trimmed.starts_with("yes ") {
        return "echo '[Interactive infinite stream bounded]'".to_string();
    }
    trimmed.to_string()
}

fn run_local_command_bounded(run_dir: &str, cmd: &str, timeout: Duration) -> String {
    let mut child = match Command::new("sh")
        .args(["-c", cmd])
        .current_dir(run_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return format!("Error executing command: {e}"),
    };

    let start = Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }

    let mut out_bytes = Vec::new();
    let mut err_bytes = Vec::new();
    if let Some(out_pipe) = child.stdout.take() {
        let _ = out_pipe.take(16384).read_to_end(&mut out_bytes);
    }
    if let Some(err_pipe) = child.stderr.take() {
        let _ = err_pipe.take(16384).read_to_end(&mut err_bytes);
    }
    let stdout = String::from_utf8_lossy(&out_bytes);
    let stderr = String::from_utf8_lossy(&err_bytes);
    let mut combined = if stderr.trim().is_empty() {
        stdout.trim_end().to_string()
    } else if stdout.trim().is_empty() {
        stderr.trim_end().to_string()
    } else {
        format!("{}\n{}", stdout.trim_end(), stderr.trim_end())
    };
    if timed_out {
        let note = "\n[Command timed out after 2.5s — interactive process stopped]";
        combined.push_str(note);
    }
    combined
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
}
