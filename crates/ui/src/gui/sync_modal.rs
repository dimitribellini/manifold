use super::*;

pub(super) enum BgSyncEvent {
    PlanReady(Result<SyncExecutionPlan, String>),
    ExecFinished {
        dest_dir: String,
        result: Result<(usize, usize), String>,
    },
}

pub(super) fn parse_sync_options(
    strategy_str: &str,
    compare_str: &str,
) -> (SyncStrategy, bool, ComparisonMode) {
    let (strategy, mirror_deletes) = match strategy_str {
        "left_to_right" => (SyncStrategy::LeftToRight, false),
        "mirror_left_to_right" | "0" => (SyncStrategy::LeftToRight, true),
        "right_to_left" => (SyncStrategy::RightToLeft, false),
        "mirror_right_to_left" | "1" => (SyncStrategy::RightToLeft, true),
        "bi_directional" | "bidirectional" | "2" => (SyncStrategy::Bidirectional, false),
        _ => (SyncStrategy::LeftToRight, true),
    };
    let comparison = match compare_str {
        "timestamp" | "metadata" | "0" => ComparisonMode::Metadata,
        _ => ComparisonMode::ContentHash,
    };
    (strategy, mirror_deletes, comparison)
}

fn apply_delete_approval(plan: &mut SyncExecutionPlan, mirror_deletes: bool) {
    if !mirror_deletes {
        for entry in &mut plan.entries {
            if matches!(
                entry.operation,
                SyncOperation::DeleteLeft | SyncOperation::DeleteRight
            ) {
                entry.approved = false;
            }
        }
    }
}

impl GuiRuntime {
    pub(super) fn build_sync_plan_between_panes(
        &mut self,
        strategy_str: &str,
        compare_str: &str,
    ) -> Result<SyncExecutionPlan, String> {
        let src = self.current_pane_path(false);
        let dst = self.current_pane_path(true);
        if let Some((arch_path, _)) = split_archive_uri(&src) {
            self.ensure_archive_mounted(&arch_path);
        }
        if split_archive_uri(&dst).is_some() {
            return Err("Cannot synchronize into a read-only archive".to_string());
        }
        if let Some((p, _)) = split_remote_uri(&src) {
            self.ensure_remote_connected(&p);
        }
        if let Some((p, _)) = split_remote_uri(&dst) {
            self.ensure_remote_connected(&p);
        }
        let src_sub = split_archive_uri(&src)
            .map(|(_, s)| s)
            .or_else(|| split_remote_uri(&src).map(|(_, s)| s))
            .unwrap_or_else(|| src.clone());
        let dst_sub = split_remote_uri(&dst)
            .map(|(_, s)| s)
            .unwrap_or_else(|| dst.clone());
        let src_vfs = self.backend_for_path(&src);
        let dst_vfs = self.backend_for_path(&dst);
        let (strategy, mirror_deletes, comparison) = parse_sync_options(strategy_str, compare_str);
        let mut plan =
            SyncExecutionPlan::build(src_vfs, &src_sub, dst_vfs, &dst_sub, strategy, comparison)
                .map_err(|e| e.to_string())?;
        apply_delete_approval(&mut plan, mirror_deletes);
        self.active_sync_plan = Some(plan.clone());
        Ok(plan)
    }

    pub(super) fn spawn_bg_sync_plan(&mut self, strategy_str: &str, compare_str: &str) {
        let src = self.current_pane_path(false);
        let dst = self.current_pane_path(true);
        if split_archive_uri(&dst).is_some() {
            self.show_toast("error", "Cannot synchronize into a read-only archive");
            return;
        }
        if let Some((p, _)) = split_remote_uri(&src) {
            self.ensure_remote_connected(&p);
        }
        if let Some((p, _)) = split_remote_uri(&dst) {
            self.ensure_remote_connected(&p);
        }
        if split_archive_uri(&src).is_some() {
            let _ = self.build_sync_plan_between_panes(strategy_str, compare_str);
            return;
        }
        let tx = self.sync_tx.clone();
        let local_vfs = self.local_vfs.clone();
        let remotes = self.remote_backends.clone();
        let strat_owned = strategy_str.to_string();
        let cmp_owned = compare_str.to_string();
        self.sync_running = true;
        std::thread::spawn(move || {
            let (strategy, mirror_deletes, comparison) =
                parse_sync_options(&strat_owned, &cmp_owned);
            let src_rem = split_remote_uri(&src);
            let dst_rem = split_remote_uri(&dst);
            let src_sub = src_rem
                .as_ref()
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| src.clone());
            let dst_sub = dst_rem
                .as_ref()
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| dst.clone());
            let src_vfs: Option<&dyn FileSystemBackend> = match &src_rem {
                Some((p, _)) => {
                    let rem = remotes.get(p);
                    rem.map(|r| r as &dyn FileSystemBackend)
                }
                None => Some(&local_vfs as &dyn FileSystemBackend),
            };
            let dst_vfs: Option<&dyn FileSystemBackend> = match &dst_rem {
                Some((p, _)) => {
                    let rem = remotes.get(p);
                    rem.map(|r| r as &dyn FileSystemBackend)
                }
                None => Some(&local_vfs as &dyn FileSystemBackend),
            };
            let res = match (src_vfs, dst_vfs) {
                (Some(sv), Some(dv)) => {
                    SyncExecutionPlan::build(sv, &src_sub, dv, &dst_sub, strategy, comparison)
                        .map(|mut plan| {
                            apply_delete_approval(&mut plan, mirror_deletes);
                            plan
                        })
                        .map_err(|e| e.to_string())
                }
                _ => Err("Remote VFS is not connected".to_string()),
            };
            let _ = tx.send(BgSyncEvent::PlanReady(res));
        });
    }

    pub(super) fn spawn_bg_sync_execute(&mut self) {
        let Some(plan) = self.active_sync_plan.take() else {
            self.show_toast("error", "No active synchronization plan");
            return;
        };
        let src = self.current_pane_path(false);
        let dst = self.current_pane_path(true);
        if split_archive_uri(&src).is_some() {
            self.active_sync_plan = Some(plan);
            match self.execute_active_sync_plan() {
                Ok((total, applied)) => {
                    let msg = format!("[SYNC] Synchronized {total} entries ({applied} updated).");
                    self.logs.push(msg);
                    let label = format!("Synchronized {applied} of {total} entries");
                    self.start_tracked_task(&label, &dst, 8192, label.clone(), "success");
                }
                Err(err) => self.show_toast("error", err),
            }
            return;
        }
        let tx = self.sync_tx.clone();
        let local_vfs = self.local_vfs.clone();
        let remotes = self.remote_backends.clone();
        self.sync_running = true;
        self.bg_tasks_in_flight += 1;
        self.active_task_label = "Synchronizing directories…".to_string();
        self.active_task_progress = 35;
        std::thread::spawn(move || {
            let total = plan.entries.len();
            let src_rem = split_remote_uri(&src);
            let dst_rem = split_remote_uri(&dst);
            let src_vfs: Option<&dyn FileSystemBackend> = match &src_rem {
                Some((p, _)) => {
                    let rem = remotes.get(p);
                    rem.map(|r| r as &dyn FileSystemBackend)
                }
                None => Some(&local_vfs as &dyn FileSystemBackend),
            };
            let dst_vfs: Option<&dyn FileSystemBackend> = match &dst_rem {
                Some((p, _)) => {
                    let rem = remotes.get(p);
                    rem.map(|r| r as &dyn FileSystemBackend)
                }
                None => Some(&local_vfs as &dyn FileSystemBackend),
            };
            let result = match (src_vfs, dst_vfs) {
                (Some(sv), Some(dv)) => plan
                    .execute(sv, dv)
                    .map(|applied| (total, applied))
                    .map_err(|e| e.to_string()),
                _ => Err("Remote VFS is not connected".to_string()),
            };
            let _ = tx.send(BgSyncEvent::ExecFinished {
                dest_dir: dst,
                result,
            });
        });
    }

    #[allow(dead_code)]
    pub(super) fn execute_active_sync_plan(&mut self) -> Result<(usize, usize), String> {
        let Some(plan) = self.active_sync_plan.take() else {
            return Err("No active synchronization plan".to_string());
        };
        let total = plan.entries.len();
        let src = self.current_pane_path(false);
        let dst = self.current_pane_path(true);
        let src_vfs = self.backend_for_path(&src);
        let dst_vfs = self.backend_for_path(&dst);
        let applied = plan.execute(src_vfs, dst_vfs).map_err(|e| e.to_string())?;
        self.invalidate_dir_cache();
        self.reload_pane_entries(false);
        self.reload_pane_entries(true);
        Ok((total, applied))
    }
}

pub(super) fn sync_sync_modal_to_window(window: &ManifoldWindow, rt: &GuiRuntime) {
    let left_path = rt.current_pane_path(false);
    let right_path = rt.current_pane_path(true);
    window.set_sync_left_path(left_path.into());
    window.set_sync_right_path(right_path.into());
    window.set_sync_running(rt.sync_running);

    if rt.sync_running && rt.active_sync_plan.is_none() {
        window.set_sync_preview_rows(ModelRc::new(VecModel::from(Vec::new())));
        window.set_sync_summary_text("Comparing directory trees…".into());
        return;
    }

    let Some(plan) = &rt.active_sync_plan else {
        window.set_sync_preview_rows(ModelRc::new(VecModel::from(Vec::new())));
        window.set_sync_summary_text("No differences computed.".into());
        return;
    };
    let mut approved_count = 0usize;
    let mut create_cnt = 0usize;
    let mut update_cnt = 0usize;
    let mut delete_cnt = 0usize;
    let mut equal_cnt = 0usize;
    let mut total_bytes = 0u64;
    let mut rows = Vec::with_capacity(plan.entries.len().min(600));
    for entry in &plan.entries {
        let src_sz = entry.left_meta.as_ref().and_then(|m| m.size).unwrap_or(0);
        let dst_sz = entry.right_meta.as_ref().and_then(|m| m.size).unwrap_or(0);
        let is_dir = entry.left_meta.as_ref().is_some_and(|m| m.is_dir)
            || entry.right_meta.as_ref().is_some_and(|m| m.is_dir);
        match entry.operation {
            SyncOperation::CopyLeftToRight => {
                if entry.right_meta.is_none() {
                    create_cnt += 1;
                } else {
                    update_cnt += 1;
                }
            }
            SyncOperation::CopyRightToLeft => {
                if entry.left_meta.is_none() {
                    create_cnt += 1;
                } else {
                    update_cnt += 1;
                }
            }
            SyncOperation::DeleteLeft | SyncOperation::DeleteRight => delete_cnt += 1,
            SyncOperation::Identical => equal_cnt += 1,
            SyncOperation::Conflict => {}
        }
        if entry.approved && entry.operation != SyncOperation::Identical {
            approved_count += 1;
            total_bytes = total_bytes.saturating_add(src_sz.max(dst_sz));
        }
        if rows.len() >= 600 {
            continue;
        }
        let action_str = match entry.operation {
            SyncOperation::CopyLeftToRight => "Copy →",
            SyncOperation::CopyRightToLeft => "← Copy",
            SyncOperation::DeleteLeft => "Delete L",
            SyncOperation::DeleteRight => "Delete R",
            SyncOperation::Conflict => "Conflict",
            SyncOperation::Identical => "Equal",
        };
        let src_text = match &entry.left_meta {
            Some(m) if m.is_dir => "Folder".to_string(),
            Some(m) => manifold_vfs::format_bytes_comma(m.size.unwrap_or(0)),
            None => "—".to_string(),
        };
        let dst_text = match &entry.right_meta {
            Some(m) if m.is_dir => "Folder".to_string(),
            Some(m) => manifold_vfs::format_bytes_comma(m.size.unwrap_or(0)),
            None => "—".to_string(),
        };
        rows.push(SyncPreviewRowSlint {
            relative_path: SharedString::from(&entry.relative_path),
            action: SharedString::from(action_str),
            source_size: SharedString::from(src_text),
            dest_size: SharedString::from(dst_text),
            approved: entry.approved,
            is_dir,
        });
    }
    let total_str = manifold_vfs::format_bytes_comma(total_bytes);
    let counts = format!(
        "+{create_cnt} Create  ↻{update_cnt} Update  −{delete_cnt} Delete  ={equal_cnt} Equal"
    );
    let summary = format!("{approved_count} selected ({total_str}) • {counts}");
    window.set_sync_preview_rows(ModelRc::new(VecModel::from(rows)));
    window.set_sync_summary_text(summary.into());
}
