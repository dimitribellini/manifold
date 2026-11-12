use super::*;

pub(super) fn handle_keyboard_shortcut(
    w: &ManifoldWindow,
    rt: &Rc<RefCell<GuiRuntime>>,
    key: &str,
    mods: (bool, bool, bool),
) -> bool {
    let (cmd, alt, shift) = mods;
    let k_low = key.to_lowercase();

    // Escape closes active popovers, modals, hover preview, path edit mode, or terminal drawer
    if key == "\u{001b}" {
        if w.get_show_batch_rename_modal() && w.get_batch_rename_open_dropdown_rule() >= 0 {
            w.set_batch_rename_open_dropdown_rule(-1);
            w.set_batch_rename_open_dropdown_kind(SharedString::default());
            return true;
        }
        let had_overlay = w.get_show_app_menu()
            || w.get_show_about_modal()
            || w.get_show_context_menu()
            || w.get_show_sidebar_context_menu()
            || w.get_show_view_options_popover()
            || w.get_show_open_with_modal()
            || w.get_show_batch_rename_modal()
            || w.get_show_sync_modal()
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
        w.set_show_view_options_popover(false);
        w.set_view_options_scope_menu_open(false);
        w.set_view_options_sort_menu_open(false);
        w.set_view_options_icon_menu_open(false);
        w.set_view_options_font_menu_open(false);
        w.set_view_options_text_menu_open(false);
        w.set_show_open_with_modal(false);
        w.set_batch_rename_open_dropdown_rule(-1);
        w.set_batch_rename_open_dropdown_kind(SharedString::default());
        w.set_show_batch_rename_modal(false);
        w.set_show_sync_modal(false);
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

    // Enter confirms the Delete Modal when open
    if w.get_show_delete_modal() && (key == "\n" || key == "\r") {
        w.set_show_delete_modal(false);
        w.invoke_confirm_delete_modal();
        return true;
    }

    // Modal focus guard: block background pane/file shortcuts when any modal dialog is active
    if w.get_show_connect_modal()
        || w.get_show_input_modal()
        || w.get_show_delete_modal()
        || w.get_show_conflict_modal()
        || w.get_show_open_with_modal()
        || w.get_show_preferences_modal()
        || w.get_show_about_modal()
        || w.get_show_batch_rename_modal()
        || w.get_show_sync_modal()
    {
        return true;
    }

    // Ctrl+, / Cmd+,: Open Preferences & Themes Modal
    if cmd && !alt && key == "," {
        w.set_show_app_menu(false);
        w.set_show_preferences_modal(true);
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
    if cmd && alt && !shift && k_low == "s" {
        w.invoke_execute_context_action("make_alias".into());
        return true;
    }
    if cmd && !alt && k_low == "n" {
        w.invoke_open_new_folder_modal();
        return true;
    }
    if cmd && !alt && k_low == "r" {
        w.invoke_sync_panes();
        return true;
    }
    if cmd && !alt && shift && k_low == "y" {
        w.invoke_open_sync_modal();
        return true;
    }
    if cmd && !alt && !shift && k_low == "i" {
        w.invoke_execute_context_action("get_info".into());
        return true;
    }
    if cmd && !alt && !shift && k_low == "j" {
        let next = !w.get_show_view_options_popover();
        w.set_view_options_scope_menu_open(false);
        w.set_view_options_sort_menu_open(false);
        w.set_view_options_icon_menu_open(false);
        w.set_view_options_font_menu_open(false);
        w.set_view_options_text_menu_open(false);
        w.set_show_view_options_popover(next);
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
        reset_connect_modal_for_new(w);
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
        if w.get_show_hover_preview() {
            w.set_show_hover_preview(false);
        }
        let mut guard = rt.borrow_mut();
        guard.hover_candidate = None;
        let is_right = guard.is_right_effective();
        let view_mode = if is_right {
            guard
                .app_state
                .right_tabs
                .get(guard.app_state.right_active_tab)
                .map(|t| t.view_mode)
                .unwrap_or(guard.app_state.toolbar.active_view_mode)
        } else {
            guard
                .app_state
                .left_tabs
                .get(guard.app_state.left_active_tab)
                .map(|t| t.view_mode)
                .unwrap_or(guard.app_state.toolbar.active_view_mode)
        };
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
                let local_idx =
                    sync_virtual_window_for_selection(w, &mut guard, is_right, next, view_mode);
                drop(guard);
                w.invoke_ensure_row_visible(is_right, local_idx);
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
                            let local_idx = sync_virtual_window_for_selection(
                                w, &mut guard, is_right, next, view_mode,
                            );
                            drop(guard);
                            w.invoke_ensure_row_visible(is_right, local_idx);
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
                            let local_idx = sync_virtual_window_for_selection(
                                w, &mut guard, is_right, p_idx, view_mode,
                            );
                            drop(guard);
                            w.invoke_ensure_row_visible(is_right, local_idx);
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
        if w.get_show_hover_preview() {
            w.set_show_hover_preview(false);
        }
        let mut guard = rt.borrow_mut();
        guard.hover_candidate = None;
        let is_right = guard.is_right_effective();
        let view_mode = if is_right {
            guard
                .app_state
                .right_tabs
                .get(guard.app_state.right_active_tab)
                .map(|t| t.view_mode)
                .unwrap_or(guard.app_state.toolbar.active_view_mode)
        } else {
            guard
                .app_state
                .left_tabs
                .get(guard.app_state.left_active_tab)
                .map(|t| t.view_mode)
                .unwrap_or(guard.app_state.toolbar.active_view_mode)
        };
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
                "\u{f72c}" => cur.saturating_sub(18),
                "\u{f72d}" => (cur + 18).min(len - 1),
                _ => cur,
            };
            guard.apply_row_selection(is_right, next, shift, false);
            guard.refresh_headers_and_inspector();
            let local_idx =
                sync_virtual_window_for_selection(w, &mut guard, is_right, next, view_mode);
            drop(guard);
            w.invoke_ensure_row_visible(is_right, local_idx);
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

fn sync_virtual_window_for_selection(
    w: &ManifoldWindow,
    guard: &mut GuiRuntime,
    is_right: bool,
    target_idx: usize,
    view_mode: PaneViewMode,
) -> i32 {
    const WINDOW_CAP: usize = 300;
    let total = if is_right {
        guard.right_entries.len()
    } else {
        guard.left_entries.len()
    };
    let cur_top = if is_right {
        guard.right_virtual_top_row
    } else {
        guard.left_virtual_top_row
    };
    let max_top = total.saturating_sub(WINDOW_CAP);
    let mut new_top = if target_idx < cur_top {
        target_idx.saturating_sub(24).min(max_top)
    } else if target_idx >= cur_top + WINDOW_CAP {
        target_idx.saturating_sub(WINDOW_CAP - 36).min(max_top)
    } else {
        cur_top
    };
    if view_mode == PaneViewMode::Icons {
        new_top -= new_top % 3;
    }
    if new_top != cur_top {
        if is_right {
            guard.right_virtual_top_row = new_top;
        } else {
            guard.left_virtual_top_row = new_top;
        }
        sync_window_from_runtime(w, guard);
    } else {
        sync_selection_and_inspector(w, guard);
    }
    target_idx.saturating_sub(new_top) as i32
}

pub(super) fn normalize_fav_target(raw: &str) -> String {
    let trimmed = raw.trim().strip_prefix("file://").unwrap_or(raw.trim());
    if trimmed == "/" {
        "/".to_string()
    } else {
        trimmed.trim_end_matches('/').to_string()
    }
}

pub(super) fn push_unique_open_with_app(
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

pub(super) fn discover_open_with_apps(meta: &FileMetadata) -> Vec<OpenWithAppSlint> {
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

pub(super) fn launch_file_with_app(target_path: &str, app_spec: &str) -> Result<String, String> {
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
