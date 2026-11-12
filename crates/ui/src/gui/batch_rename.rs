use super::*;

pub(super) fn open_rename_or_batch_modal(w: &ManifoldWindow, guard: &mut GuiRuntime) {
    let is_right = guard.is_right_effective();
    let sel_list = guard.selected_metadata_list(is_right);
    if sel_list.len() > 1 {
        if guard.batch_rename_rules.is_empty() {
            guard.batch_rename_rules.push(default_batch_rename_rule());
        }
        w.set_batch_rename_open_dropdown_rule(-1);
        w.set_batch_rename_open_dropdown_kind(SharedString::default());
        sync_batch_rename_modal(w, guard);
        w.set_show_batch_rename_modal(true);
    } else if let Some(first) = sel_list.first() {
        let title = format!("Rename \"{}\"", first.name);
        w.set_input_modal_mode("rename".into());
        w.set_input_modal_title(title.into());
        w.set_input_modal_value(first.name.clone().into());
        w.set_show_input_modal(true);
        w.invoke_focus_input_modal();
    }
}

pub(super) fn default_batch_rename_rule() -> BatchRenameRuleSlint {
    BatchRenameRuleSlint {
        operation: SharedString::from("Replace Text"),
        apply_to: SharedString::from("Name"),
        occurrences: SharedString::from("All"),
        find_text: SharedString::default(),
        replace_text: SharedString::default(),
        case_sensitive: false,
        case_style: SharedString::from("lowercase"),
        position: SharedString::from("After Name"),
        seq_prefix: SharedString::default(),
        seq_start: SharedString::from("1"),
        seq_step: SharedString::from("1"),
        seq_padding: SharedString::from("2"),
        date_format: SharedString::from("YYYY-MM-DD"),
    }
}

pub(super) fn build_batch_renamer_from_slint_rules(
    rules: &[BatchRenameRuleSlint],
    today_date: &str,
) -> BatchRenamer {
    let mut out = Vec::with_capacity(rules.len());
    for r in rules {
        let scope = match r.apply_to.as_str() {
            "Name with Extension" => RenameTargetScope::NameWithExtension,
            "Extension" => RenameTargetScope::Extension,
            _ => RenameTargetScope::Name,
        };
        let position = match r.position.as_str() {
            "Before Name" => TextInsertionPosition::BeforeName,
            "Replace Name" => TextInsertionPosition::ReplaceName,
            _ => TextInsertionPosition::AfterName,
        };
        let rule = match r.operation.as_str() {
            "Change Case" => {
                let style = match r.case_style.as_str() {
                    "UPPERCASE" => CaseConversionStyle::UpperCase,
                    "Title Case" => CaseConversionStyle::TitleCase,
                    "camelCase" => CaseConversionStyle::CamelCase,
                    "snake_case" => CaseConversionStyle::SnakeCase,
                    "kebab-case" => CaseConversionStyle::KebabCase,
                    _ => CaseConversionStyle::LowerCase,
                };
                RenameRule::ScopedChangeCase { scope, style }
            }
            "Add Sequence" => {
                let start = r.seq_start.trim().parse::<usize>().unwrap_or(1);
                let step = r.seq_step.trim().parse::<usize>().unwrap_or(1).max(1);
                let padding = r
                    .seq_padding
                    .trim()
                    .parse::<usize>()
                    .unwrap_or(2)
                    .clamp(1, 12);
                RenameRule::ScopedAddSequence {
                    scope,
                    position,
                    prefix: r.seq_prefix.to_string(),
                    start,
                    step,
                    padding,
                }
            }
            "Add Text" => RenameRule::ScopedAddText {
                scope,
                position,
                text: r.replace_text.to_string(),
            },
            "Add Date" => {
                let compact = today_date.replace('-', "");
                let (_, _, _, hh, mm) = manifold_vfs::current_local_civil_datetime();
                let now_dt = format!("{today_date}_{hh:02}-{mm:02}");
                let date_token = match r.date_format.as_str() {
                    "YYYYMMDD" => compact,
                    "YYYY-MM-DD_HH-mm" => now_dt,
                    "File Modified (YYYY-MM-DD)" => {
                        format!("{}{today_date}", manifold_sync::FILE_MODIFIED_TOKEN_PREFIX)
                    }
                    "File Modified (Date & Time)" | "File Modified" => {
                        format!("{}{now_dt}", manifold_sync::FILE_MODIFIED_TOKEN_PREFIX)
                    }
                    "_YYYY-MM-DD" => format!("_{today_date}"),
                    "YYYY-MM-DD_" => format!("{today_date}_"),
                    _ => today_date.to_string(),
                };
                RenameRule::ScopedAddDate {
                    scope,
                    position,
                    formatted_timestamp: date_token,
                }
            }
            _ => {
                let occurrences = match r.occurrences.as_str() {
                    "First" => ReplaceOccurrences::First,
                    "Last" => ReplaceOccurrences::Last,
                    "Regular Expression" => ReplaceOccurrences::RegularExpression,
                    _ => ReplaceOccurrences::All,
                };
                RenameRule::ScopedReplaceText {
                    scope,
                    occurrences,
                    find: r.find_text.to_string(),
                    replace: r.replace_text.to_string(),
                    case_sensitive: r.case_sensitive,
                }
            }
        };
        out.push(rule);
    }
    BatchRenamer::new(out)
}

fn build_batch_rename_inputs(
    sel_list: &[FileMetadata],
) -> Vec<manifold_sync::BatchRenameInputItem> {
    sel_list
        .iter()
        .map(|m| {
            let modified_timestamp = m.modified.map(|st| {
                let (y, mo, d, hh, mm) = manifold_vfs::system_time_to_civil(st);
                format!("{y:04}-{mo:02}-{d:02}_{hh:02}-{mm:02}")
            });
            manifold_sync::BatchRenameInputItem {
                name: m.name.clone(),
                is_dir: m.is_dir,
                modified_timestamp,
            }
        })
        .collect()
}

pub(super) fn sync_batch_rename_favorites_to_window(w: &ManifoldWindow, guard: &GuiRuntime) {
    let names: Vec<SharedString> = guard
        .prefs
        .batch_rename_favorites
        .iter()
        .map(|p| SharedString::from(&p.name))
        .collect();
    w.set_batch_rename_favorite_names(ModelRc::new(VecModel::from(names)));
}

pub(super) fn save_batch_rename_favorite_from_modal(w: &ManifoldWindow, guard: &mut GuiRuntime) {
    if guard.batch_rename_rules.is_empty() {
        return;
    }
    let first = &guard.batch_rename_rules[0];
    let add_txt = first.replace_text.as_str();
    let base_label = match first.operation.as_str() {
        "Replace Text" if !first.find_text.is_empty() => {
            format!(
                "Replace \"{}\" → \"{}\"",
                first.find_text, first.replace_text
            )
        }
        "Add Sequence" if !first.seq_prefix.is_empty() => {
            format!("Sequence \"{}#\"", first.seq_prefix)
        }
        "Add Text" if !add_txt.is_empty() => format!("Add \"{add_txt}\""),
        op => format!(
            "{op} ({} step{})",
            guard.batch_rename_rules.len(),
            if guard.batch_rename_rules.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
    };
    let existing_count = guard.prefs.batch_rename_favorites.len() + 1;
    let preset_name = if guard
        .prefs
        .batch_rename_favorites
        .iter()
        .any(|p| p.name == base_label)
    {
        format!("{base_label} #{existing_count}")
    } else {
        base_label
    };
    let rules_cfg: Vec<manifold_config::BatchRenameRuleConfig> = guard
        .batch_rename_rules
        .iter()
        .map(|r| manifold_config::BatchRenameRuleConfig {
            operation: r.operation.to_string(),
            apply_to: r.apply_to.to_string(),
            occurrences: r.occurrences.to_string(),
            find_text: r.find_text.to_string(),
            replace_text: r.replace_text.to_string(),
            case_sensitive: r.case_sensitive,
            case_mode: r.case_style.to_string(),
            text_position: r.position.to_string(),
            add_text: r.replace_text.to_string(),
            seq_prefix: r.seq_prefix.to_string(),
            seq_position: r.position.to_string(),
            seq_start: r.seq_start.trim().parse::<i32>().unwrap_or(1),
            seq_step: r.seq_step.trim().parse::<i32>().unwrap_or(1),
            seq_digits: r.seq_padding.trim().parse().unwrap_or(2),
            date_position: r.position.to_string(),
            date_format: r.date_format.to_string(),
        })
        .collect();
    guard
        .prefs
        .upsert_batch_rename_favorite(manifold_config::BatchRenameFavoritePreset {
            name: preset_name.clone(),
            rules: rules_cfg,
        });
    let _ = guard.prefs.save_default();
    sync_batch_rename_favorites_to_window(w, guard);
    let msg = format!("Saved preset \"{preset_name}\" to Favorites");
    w.set_batch_rename_status_text(msg.into());
}

pub(super) fn apply_batch_rename_favorite_to_modal(
    w: &ManifoldWindow,
    guard: &mut GuiRuntime,
    idx: usize,
) {
    let Some(preset) = guard.prefs.batch_rename_favorites.get(idx).cloned() else {
        return;
    };
    guard.batch_rename_rules = preset
        .rules
        .iter()
        .map(|c| BatchRenameRuleSlint {
            operation: c.operation.clone().into(),
            apply_to: c.apply_to.clone().into(),
            occurrences: c.occurrences.clone().into(),
            find_text: c.find_text.clone().into(),
            replace_text: c.replace_text.clone().into(),
            case_sensitive: c.case_sensitive,
            case_style: c.case_mode.clone().into(),
            position: c.text_position.clone().into(),
            seq_prefix: c.seq_prefix.clone().into(),
            seq_start: c.seq_start.to_string().into(),
            seq_step: c.seq_step.to_string().into(),
            seq_padding: c.seq_digits.to_string().into(),
            date_format: c.date_format.clone().into(),
        })
        .collect();
    sync_batch_rename_modal(w, guard);
}

pub(super) fn delete_batch_rename_favorite_from_modal(
    w: &ManifoldWindow,
    guard: &mut GuiRuntime,
    idx: usize,
) {
    if guard.prefs.remove_batch_rename_favorite(idx) {
        let _ = guard.prefs.save_default();
        sync_batch_rename_favorites_to_window(w, guard);
    }
}

pub(super) fn sync_batch_rename_modal(w: &ManifoldWindow, guard: &mut GuiRuntime) {
    if guard.batch_rename_rules.is_empty() {
        guard.batch_rename_rules.push(default_batch_rename_rule());
    }
    sync_batch_rename_favorites_to_window(w, guard);
    w.set_batch_rename_rules(ModelRc::default());
    w.set_batch_rename_rules(ModelRc::new(VecModel::from(
        guard.batch_rename_rules.clone(),
    )));
    sync_batch_rename_preview_only(w, guard);
}

pub(super) fn sync_batch_rename_rule_at(w: &ManifoldWindow, guard: &mut GuiRuntime, idx: usize) {
    if let Some(rule) = guard.batch_rename_rules.get(idx).cloned() {
        let model = w.get_batch_rename_rules();
        if idx < model.row_count() {
            model.set_row_data(idx, rule);
        } else {
            w.set_batch_rename_rules(ModelRc::new(VecModel::from(
                guard.batch_rename_rules.clone(),
            )));
        }
    }
    sync_batch_rename_preview_only(w, guard);
}

pub(super) fn sync_batch_rename_preview_only(w: &ManifoldWindow, guard: &mut GuiRuntime) {
    let is_right = guard.is_right_effective();
    let sel_list = guard.selected_metadata_list(is_right);
    let cur_dir = guard.current_pane_path(is_right);
    let existing_names: HashSet<String> = guard
        .list_raw_dir_cached(&cur_dir)
        .into_iter()
        .map(|m| m.name)
        .collect();
    let inputs = build_batch_rename_inputs(&sel_list);
    let today_date = current_iso_date_string();
    let renamer = build_batch_renamer_from_slint_rules(&guard.batch_rename_rules, &today_date)
        .with_case_insensitive(cfg!(any(target_os = "macos", target_os = "windows")));
    let previews = renamer.preview_with_metadata(&inputs, &existing_names);

    let mut conflict_count = 0i32;
    let mut changed_count = 0i32;
    let mut rows = Vec::with_capacity(previews.len());
    for (meta, prev) in sel_list.iter().zip(previews.iter()) {
        let is_changed = prev.proposed_name != prev.original_name;
        let has_conflict = prev.has_collision;
        if has_conflict {
            conflict_count += 1;
        } else if is_changed {
            changed_count += 1;
        }
        let status = if has_conflict {
            "conflict"
        } else if is_changed {
            "changed"
        } else {
            "unchanged"
        };
        rows.push(BatchRenamePreviewRowSlint {
            original_name: prev.original_name.clone().into(),
            proposed_name: prev.proposed_name.clone().into(),
            is_dir: is_browsable_folder(meta),
            is_image: is_image_extension(meta.extension.as_deref()),
            status: SharedString::from(status),
        });
    }
    let status_text = if conflict_count == 1 {
        "1 item has a conflicting or invalid name".to_string()
    } else if conflict_count > 1 {
        format!("{conflict_count} items have conflicting or invalid names")
    } else if changed_count == 1 {
        "1 item will be renamed".to_string()
    } else if changed_count > 1 {
        format!("{changed_count} items will be renamed")
    } else {
        format!("{} items selected", sel_list.len())
    };

    let preview_model = w.get_batch_rename_preview_rows();
    if preview_model.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            let needs_update = preview_model.row_data(i).is_none_or(|cur| {
                cur.proposed_name != row.proposed_name
                    || cur.status != row.status
                    || cur.original_name != row.original_name
            });
            if needs_update {
                preview_model.set_row_data(i, row);
            }
        }
    } else {
        w.set_batch_rename_preview_rows(ModelRc::new(VecModel::from(rows)));
    }
    w.set_batch_rename_conflict_count(conflict_count);
    w.set_batch_rename_changed_count(changed_count);
    w.set_batch_rename_status_text(status_text.into());
}

pub(super) fn execute_batch_rename_from_modal(w: &ManifoldWindow, guard: &mut GuiRuntime) {
    let is_right = guard.is_right_effective();
    let sel_list = guard.selected_metadata_list(is_right);
    if sel_list.is_empty() {
        w.set_show_batch_rename_modal(false);
        return;
    }
    let cur_dir = guard.current_pane_path(is_right);
    let existing_names: HashSet<String> = guard
        .list_raw_dir_cached(&cur_dir)
        .into_iter()
        .map(|m| m.name)
        .collect();
    let inputs = build_batch_rename_inputs(&sel_list);
    let today_date = current_iso_date_string();
    let renamer = build_batch_renamer_from_slint_rules(&guard.batch_rename_rules, &today_date)
        .with_case_insensitive(cfg!(any(target_os = "macos", target_os = "windows")));
    let previews = renamer.preview_with_metadata(&inputs, &existing_names);
    if previews.iter().any(|p| p.has_collision) {
        sync_batch_rename_modal(w, guard);
        return;
    }

    let is_case_insens = cfg!(any(target_os = "macos", target_os = "windows"));
    let norm_name = |s: &str| -> String {
        if is_case_insens {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let batch_orig_set: HashSet<String> = sel_list.iter().map(|m| norm_name(&m.name)).collect();
    let needs_staging = previews.iter().any(|p| {
        !p.has_collision
            && p.proposed_name != p.original_name
            && batch_orig_set.contains(&norm_name(&p.proposed_name))
    });

    let mut staged: Vec<(String, String, String, String, String)> = Vec::new();
    let mut completed_pairs: Vec<(String, String)> = Vec::new();
    let mut renamed_count = 0usize;
    let mut last_dst = None;
    let mut rename_err: Option<String> = None;

    if needs_staging {
        for (i, (meta, prev)) in sel_list.iter().zip(previews.iter()).enumerate() {
            if prev.has_collision || prev.proposed_name == prev.original_name {
                continue;
            }
            let parent = vfs_parent_dir(&meta.path).unwrap_or_else(|| cur_dir.clone());
            let tmp_name = format!(
                ".__manifold_ren_tmp_{}_{i}_{}",
                std::process::id(),
                meta.name
            );
            let tmp_path = join_vfs_path(&parent, &tmp_name);
            let dst_path = join_vfs_path(&parent, &prev.proposed_name);
            match guard.vfs_rename(&meta.path, &tmp_path) {
                Ok(()) => {
                    staged.push((
                        tmp_path,
                        dst_path,
                        meta.path.clone(),
                        meta.name.clone(),
                        prev.proposed_name.clone(),
                    ));
                }
                Err(err) => {
                    rename_err = Some(err);
                    break;
                }
            }
        }
        if rename_err.is_some() {
            for (tmp_path, _, orig_path, _, _) in staged.into_iter().rev() {
                let _ = guard.vfs_rename(&tmp_path, &orig_path);
            }
        } else {
            let mut remaining_tmp: Vec<(String, String)> = staged
                .iter()
                .map(|(tmp, _, orig, _, _)| (tmp.clone(), orig.clone()))
                .collect();
            for (idx, (tmp_path, dst_path, orig_path, orig_name, prop_name)) in
                staged.into_iter().enumerate()
            {
                match guard.vfs_rename(&tmp_path, &dst_path) {
                    Ok(()) => {
                        completed_pairs.push((dst_path.clone(), orig_path));
                        remaining_tmp[idx].0.clear();
                        let log_msg = format!("[BATCH-RENAME] {orig_name} -> {prop_name}");
                        guard.logs.push(log_msg);
                        renamed_count += 1;
                        last_dst = Some(dst_path);
                    }
                    Err(err) => {
                        rename_err = Some(err);
                        for (done_dst, done_orig) in completed_pairs.into_iter().rev() {
                            let _ = guard.vfs_rename(&done_dst, &done_orig);
                        }
                        for (rem_tmp, rem_orig) in remaining_tmp.into_iter().rev() {
                            if !rem_tmp.is_empty() {
                                let _ = guard.vfs_rename(&rem_tmp, &rem_orig);
                            }
                        }
                        renamed_count = 0;
                        break;
                    }
                }
            }
        }
    } else {
        for (meta, prev) in sel_list.iter().zip(previews.iter()) {
            if prev.has_collision || prev.proposed_name == prev.original_name {
                continue;
            }
            let parent = vfs_parent_dir(&meta.path).unwrap_or_else(|| cur_dir.clone());
            let dst = join_vfs_path(&parent, &prev.proposed_name);
            match guard.vfs_rename(&meta.path, &dst) {
                Ok(()) => {
                    completed_pairs.push((dst.clone(), meta.path.clone()));
                    let log_msg = format!("[BATCH-RENAME] {} -> {}", meta.name, prev.proposed_name);
                    guard.logs.push(log_msg);
                    renamed_count += 1;
                    last_dst = Some(dst);
                }
                Err(err) => {
                    rename_err = Some(err);
                    for (done_dst, done_orig) in completed_pairs.into_iter().rev() {
                        let _ = guard.vfs_rename(&done_dst, &done_orig);
                    }
                    renamed_count = 0;
                    break;
                }
            }
        }
    }

    if let Some(err) = rename_err {
        guard.invalidate_dir_cache();
        guard.reload_pane_entries(false);
        guard.reload_pane_entries(true);
        sync_window_from_runtime(w, guard);
        let msg = format!("Rename failed (rolled back): {err}");
        w.set_batch_rename_conflict_count(1);
        w.set_batch_rename_status_text(msg.into());
        return;
    }

    w.set_batch_rename_open_dropdown_rule(-1);
    w.set_batch_rename_open_dropdown_kind(SharedString::default());
    w.set_show_batch_rename_modal(false);
    if renamed_count > 0 {
        guard.invalidate_dir_cache();
        guard.reload_pane_entries(false);
        guard.reload_pane_entries(true);
        guard.select_single_path_or_first(is_right, last_dst.as_deref());
    }
    sync_window_from_runtime(w, guard);
}

pub(super) fn prompt_delete_confirmation(
    w: &ManifoldWindow,
    guard: &mut GuiRuntime,
    permanent: bool,
) {
    let is_right = guard.is_right_effective();
    let sel_list = guard.selected_metadata_list(is_right);
    let Some(first) = sel_list.first() else {
        return;
    };
    let any_remote = sel_list.iter().any(|m| split_remote_uri(&m.path).is_some());
    let effective_perm = permanent || any_remote;
    guard.pending_delete_permanent = effective_perm;
    let count = sel_list.len();
    let (title, msg) = if any_remote {
        if count == 1 {
            (
                format!("Permanently delete \"{}\" on remote server?", first.name),
                "Remote servers do not provide a Trash / Recycle Bin. You cannot undo this action."
                    .to_string(),
            )
        } else {
            (
                format!("Permanently delete {count} items on remote server?"),
                "Remote servers do not provide a Trash / Recycle Bin. You cannot undo this action."
                    .to_string(),
            )
        }
    } else if effective_perm {
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
    w.set_delete_modal_permanent(effective_perm);
    w.set_delete_modal_title(title.into());
    w.set_delete_modal_message(msg.into());
    w.set_show_delete_modal(true);
}

pub(super) fn reset_connect_modal_for_new(w: &ManifoldWindow) {
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

pub(super) fn populate_connect_modal_from_profile(
    w: &ManifoldWindow,
    guard: &GuiRuntime,
    prof: &ConnectionProfile,
) {
    reset_connect_modal_for_new(w);
    let proto_str = match prof.protocol {
        ProtocolType::S3 => "S3",
        ProtocolType::Smb => "SMB",
        ProtocolType::Ftp => "FTP",
        ProtocolType::Ftps => "FTPS",
        ProtocolType::WebDav => "WebDAV",
        _ => "SFTP",
    };
    let host_s = prof.host.clone().unwrap_or_default();
    let default_port = match prof.protocol {
        ProtocolType::S3 | ProtocolType::WebDav => 443,
        ProtocolType::Smb => 445,
        ProtocolType::Ftps => 990,
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

pub(super) fn expand_tilde_path(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/".to_string());
        let trimmed = home.trim_end_matches(&['/', '\\'][..]);
        if raw == "~" {
            return home;
        }
        let suffix = raw.strip_prefix("~/").unwrap_or("");
        return format!("{trimmed}/{suffix}");
    }
    raw.to_string()
}

pub(super) fn complete_directory_prefix(vfs: &LocalFsBackend, input: &str) -> Option<String> {
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
