use super::*;

#[derive(Clone, Debug, Default)]
pub(super) struct ResolvedDropTarget {
    pub(super) dst_dir: Option<String>,
    pub(super) dst_label: String,
    pub(super) is_move: bool,
    pub(super) hover_sidebar_idx: i32,
    pub(super) hover_left_tab: i32,
    pub(super) hover_right_tab: i32,
    pub(super) hover_left_row: i32,
    pub(super) hover_right_row: i32,
    pub(super) hover_left_pane: bool,
    pub(super) hover_right_pane: bool,
}

impl GuiRuntime {
    pub(super) fn drag_items_for_row(&self, from_right: bool, row_idx: usize) -> Vec<FileMetadata> {
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

    #[cfg(test)]
    pub(super) fn resolve_drop_target(
        &self,
        from_right: bool,
        items: &[FileMetadata],
        coords: (f32, f32, f32, f32, f32),
        bounds: (f32, f32, f32),
    ) -> ResolvedDropTarget {
        self.resolve_drop_target_for_os(from_right, items, coords, bounds, true)
    }

    pub(super) fn resolve_drop_target_for_os(
        &self,
        from_right: bool,
        items: &[FileMetadata],
        coords: (f32, f32, f32, f32, f32),
        bounds: (f32, f32, f32),
        allow_external_os_drop: bool,
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
        if is_outside_window && allow_external_os_drop {
            res.dst_dir = Some("__EXTERNAL_OS_DROP__".to_string());
            res.dst_label = "→ Drop Outside Manifold (OS Drag & Drop)".to_string();
            return res;
        }
        if is_outside_window {
            return res;
        }

        // 1. Sidebar Hit-Testing (Devices, Connections, Favorites)
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
        let split = self.app_state.split_ratio.clamp(0.2, 0.8);
        let is_split_vertical = self.app_state.dual_pane_enabled
            && self.app_state.panes_layout == PanesLayout::SplitVertical;

        let (target_is_right, rel_x, target_pane_w, pane_top_y) = if is_split_vertical {
            let avail_h = (win_h - 54.0).max(240.0);
            let split_y = 48.0 + (avail_h - 8.0) * split;
            let in_bottom = abs_y >= split_y + 4.0;
            let top_y = if in_bottom { split_y + 8.0 } else { 48.0 };
            (in_bottom, (abs_x - left_pane_x).max(0.0), pane_w, top_y)
        } else {
            let right_pane_x = left_pane_x + pane_w + 12.0;
            let in_right = self.app_state.dual_pane_enabled && abs_x >= right_pane_x - 6.0;
            let rx = if in_right {
                (abs_x - right_pane_x).max(0.0)
            } else {
                (abs_x - left_pane_x).max(0.0)
            };
            let tpw = if in_right {
                (pane_w * (1.0 - split) / split).max(260.0)
            } else {
                pane_w
            };
            (in_right, rx, tpw, 48.0f32)
        };

        let tab_bar_top = pane_top_y;
        let tab_bar_bottom = tab_bar_top + 38.0 + font_delta * 1.5;
        let path_bar_top = tab_bar_bottom + 2.5;
        let path_bar_bottom = path_bar_top + 50.0 + font_delta * 2.0;

        // 2. Tab Bar Hit-Testing
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

        // 3. Breadcrumb / Path Bar Hit-Testing
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

        // 4. File Viewport Hit-Testing (incorporating virtualized window offset)
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
        let (scroll_y, virtual_top) = if target_is_right {
            (right_scroll_y, self.right_virtual_top_row as isize)
        } else {
            (left_scroll_y, self.left_virtual_top_row as isize)
        };
        let slice_idx = if view_mode == 0 || view_mode == 3 {
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
        let hovered_idx = if slice_idx >= 0 {
            virtual_top + slice_idx
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

    pub(super) fn begin_batch_transfer(
        &mut self,
        items: Vec<FileMetadata>,
        dst_dir: String,
        is_move: bool,
        clear_cut_clipboard: bool,
    ) {
        if items.is_empty() || split_archive_uri(&dst_dir).is_some() {
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

    pub(super) fn apply_resolved_batch_item(
        &mut self,
        batch: &mut PendingBatchTransfer,
        src_meta: &FileMetadata,
        ex_meta: Option<&FileMetadata>,
        policy: ConflictResolution,
    ) -> bool {
        let mut final_dst = join_vfs_path(&batch.dst_dir, &src_meta.name);
        let mut replace_existing = false;

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
                    replace_existing = true;
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
                    replace_existing = true;
                }
                ConflictResolution::KeepBoth => {
                    final_dst = self.unique_keep_both_path(&batch.dst_dir, &src_meta.name);
                }
                _ => {}
            }
        }

        let src_slash = format!("{}/", src_meta.path.trim_end_matches('/'));
        if final_dst == src_meta.path || final_dst.starts_with(&src_slash) {
            let msg = format!(
                "[ERROR] Cannot copy or move \"{}\" into itself",
                src_meta.name
            );
            self.logs.push(msg);
            return true;
        }

        let src_remote = split_remote_uri(&src_meta.path).is_some();
        let src_archive = split_archive_uri(&src_meta.path).is_some();
        let dst_remote = split_remote_uri(&batch.dst_dir).is_some();
        let sz = src_meta.size.unwrap_or(4096);
        if src_remote || src_archive || dst_remote || sz >= 4 * 1024 * 1024 {
            batch.bg_pairs.push((src_meta.path.clone(), final_dst, sz));
        } else {
            let res = if replace_existing && final_dst != src_meta.path {
                let stage_path = local_part_sibling(&final_dst);
                let stage_str = stage_path.to_string_lossy().to_string();
                let staged_res = if batch.is_move {
                    self.vfs_move_entry(&src_meta.path, &stage_str)
                } else {
                    self.vfs_copy_entry(&src_meta.path, &stage_str)
                };
                match staged_res {
                    Ok(()) => swap_local_staged_into_place(&stage_path, &final_dst),
                    Err(err) => {
                        let _ = self.vfs_remove(&stage_str, true);
                        Err(err)
                    }
                }
            } else if batch.is_move {
                self.vfs_move_entry(&src_meta.path, &final_dst)
            } else {
                self.vfs_copy_entry(&src_meta.path, &final_dst)
            };
            match res {
                Ok(()) => {
                    batch.sync_bytes += sz;
                    let tag = if batch.is_move { "MOVE" } else { "COPY" };
                    let msg = format!("[{tag}] {} -> {final_dst}", src_meta.name);
                    self.logs.push(msg);
                    batch.sync_count += 1;
                }
                Err(err) => {
                    let msg = format!("[ERROR] Transfer failed for {}: {err}", src_meta.name);
                    self.logs.push(msg);
                    self.show_toast("error", format!("Transfer failed: {err}"));
                }
            }
        }
        true
    }

    pub(super) fn advance_batch_transfer(&mut self) {
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

    pub(super) fn resolve_batch_conflict(&mut self, policy: ConflictResolution, apply_all: bool) {
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

    pub(super) fn finalize_batch_transfer(&mut self, batch: PendingBatchTransfer) {
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

    pub(super) fn prepare_local_paths_for_external_drag(
        &mut self,
        items: &[FileMetadata],
    ) -> Vec<String> {
        let mut out = Vec::with_capacity(items.len());
        let stage_dir = manifold_vfs::secure_runtime_subdir("drag-out")
            .unwrap_or_else(|_| std::env::temp_dir().join("manifold-drag-out"));
        for meta in items {
            if split_remote_uri(&meta.path).is_none() && split_archive_uri(&meta.path).is_none() {
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
                    self.vfs_read_bytes(&meta.path)
                        .is_ok_and(|b| std::fs::write(&dst_file, b).is_ok())
                };
                if staged_ok {
                    out.push(dst_str);
                }
            }
        }
        out
    }

    pub(super) fn execute_drop_target(
        &mut self,
        items: &[FileMetadata],
        dst_dir: &str,
        is_move: bool,
    ) {
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

            let msg = format!(
                "[DRAG-OUT] Exported {} item(s) to OS File Clipboard",
                items.len()
            );
            self.logs.push(msg);
            self.show_toast(
                "copy",
                format!("Copied {} item(s) to OS Clipboard", items.len()),
            );
            return;
        }
        let effective_move = is_move && items.iter().all(|m| split_archive_uri(&m.path).is_none());
        self.begin_batch_transfer(items.to_vec(), dst_dir.to_string(), effective_move, false);
    }

    pub(super) fn paste_clipboard_into(&mut self, dst_dir: &str) {
        let (sources, is_cut) = if !self.clipboard_paths.is_empty() {
            (self.clipboard_paths.clone(), self.clipboard_cut)
        } else {
            let os_files = read_files_from_os_clipboard();
            if os_files.is_empty() {
                return;
            }
            (os_files, false)
        };
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
        let effective_move = is_cut && items.iter().all(|m| split_archive_uri(&m.path).is_none());
        self.begin_batch_transfer(items, dst_dir.to_string(), effective_move, is_cut);
    }
}
