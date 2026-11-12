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

#[test]
fn test_view_options_sorting_and_relative_dates() {
    let ep = std::time::UNIX_EPOCH;
    let mut dir_z = sample_meta("Zulu");
    dir_z.is_dir = true;
    dir_z.modified = Some(ep + Duration::from_secs(100));
    let mut dir_a = sample_meta("Alpha");
    dir_a.is_dir = true;
    dir_a.modified = Some(ep + Duration::from_secs(900));
    let mut file_m = sample_meta("middle.txt");
    file_m.is_dir = false;
    file_m.modified = Some(ep + Duration::from_secs(500));

    let mut list = [dir_z.clone(), file_m.clone(), dir_a.clone()];
    // 1. Folders on top + always sort folders alphabetically even when sorting by Modified Desc
    sort_metadata_slice_with_options(
        &mut list,
        SortColumn::Modified,
        SortDirection::Descending,
        true,
        true,
        false,
    );
    assert_eq!(list[0].name, "Alpha");
    assert_eq!(list[1].name, "Zulu");
    assert_eq!(list[2].name, "middle.txt");

    // 2. When folders_on_top is false, items are sorted purely by Modified Descending
    let mut mixed = [dir_z, file_m, dir_a];
    sort_metadata_slice_with_options(
        &mut mixed,
        SortColumn::Modified,
        SortDirection::Descending,
        false,
        false,
        false,
    );
    assert_eq!(mixed[0].name, "Alpha");
    assert_eq!(mixed[1].name, "middle.txt");
    assert_eq!(mixed[2].name, "Zulu");

    // 3. Relative date formatting produces "Today, HH:mm" for current time
    let now_st = std::time::SystemTime::now();
    let rel = format_datetime_for_view(Some(now_st), true);
    assert!(rel.starts_with("Today, "), "Unexpected rel date: {rel}");
    let abs = format_datetime_for_view(Some(now_st), false);
    assert!(abs.contains('-'), "Unexpected abs date: {abs}");
}

#[test]
fn test_multi_rename_slint_rule_builder_and_preview_pipeline() {
    let mut r1 = default_batch_rename_rule();
    r1.operation = "Replace Text".into();
    r1.apply_to = "Name with Extension".into();
    r1.occurrences = "Regular Expression".into();
    r1.find_text = "100.*".into();
    r1.replace_text = "image".into();

    let renamer = build_batch_rename_from_test(std::slice::from_ref(&r1));
    let names = [
        "manifold_fig3.png".to_string(),
        "100baset hub.jpg".to_string(),
        "100baset hub copy.jpg".to_string(),
        "10700.jpg".to_string(),
    ];
    let existing: HashSet<String> = names.iter().cloned().collect();
    let prev = renamer.preview(&names, &existing);
    assert_eq!(prev[0].proposed_name, "manifold_fig3.png");
    assert!(!prev[0].has_collision);
    assert_eq!(prev[1].proposed_name, "image");
    assert!(prev[1].has_collision);
    assert_eq!(prev[2].proposed_name, "image");
    assert!(prev[2].has_collision);
    assert_eq!(prev[3].proposed_name, "10700.jpg");
    assert!(!prev[3].has_collision);

    // Add a second rule in the pipeline (Add Sequence -> After Name -> Prefix "_")
    let mut r2 = default_batch_rename_rule();
    r2.operation = "Add Sequence".into();
    r2.apply_to = "Name".into();
    r2.position = "After Name".into();
    r2.seq_prefix = "_".into();
    r2.seq_start = "1".into();
    r2.seq_step = "1".into();
    r2.seq_padding = "2".into();

    let renamer2 = build_batch_rename_from_test(&[r1, r2]);
    let prev2 = renamer2.preview(&names, &existing);
    assert_eq!(prev2[0].proposed_name, "manifold_fig3_01.png");
    assert!(!prev2[0].has_collision);
    assert_eq!(prev2[1].proposed_name, "image_02");
    assert!(!prev2[1].has_collision);
    assert_eq!(prev2[2].proposed_name, "image_03");
    assert!(!prev2[2].has_collision);
}

fn build_batch_rename_from_test(rules: &[BatchRenameRuleSlint]) -> BatchRenamer {
    build_batch_renamer_from_slint_rules(rules, "2026-11-10")
}

#[test]
fn test_v070_trash_freedesktop_trashinfo_and_no_permanent_delete_fallback() {
    let work_dir = format!("/tmp/manifold_trash_v070_{}", std::process::id());
    let _ = std::fs::remove_dir_all(&work_dir);
    let xdg_data = format!("{work_dir}/xdg_data");
    let src_dir = format!("{work_dir}/workspace");
    std::fs::create_dir_all(&src_dir).unwrap();
    let victim = format!("{src_dir}/important note.txt");
    std::fs::write(&victim, b"never-permanently-unlink").unwrap();

    let vfs = LocalFsBackend::new();
    #[cfg(target_os = "linux")]
    {
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &xdg_data);
        let trashed_path = move_path_to_trash(&vfs, &victim).unwrap();
        if let Some(old) = prev_xdg {
            std::env::set_var("XDG_DATA_HOME", old);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
        assert!(!Path::new(&victim).exists());
        assert_eq!(
            std::fs::read(&trashed_path).unwrap(),
            b"never-permanently-unlink"
        );
        let info_file = format!("{xdg_data}/Trash/info/important note.txt.trashinfo");
        let info_text = std::fs::read_to_string(&info_file).unwrap();
        assert!(info_text.starts_with("[Trash Info]\nPath="));
        assert!(info_text.contains("important%20note.txt"));
        assert!(info_text.contains("\nDeletionDate=20"));
    }

    // Missing source path returns Err without crashing or inventing destructive fallbacks
    let missing = format!("{src_dir}/does_not_exist.txt");
    assert!(move_path_to_trash(&vfs, &missing).is_err());

    let _ = std::fs::remove_dir_all(&work_dir);
}

#[test]
fn test_v070_archive_zip_slip_rejected_and_virtual_archive_mount() {
    let work_dir = format!("/tmp/manifold_arc_v070_{}", std::process::id());
    let _ = std::fs::remove_dir_all(&work_dir);
    let out_dir = format!("{work_dir}/extracted");
    std::fs::create_dir_all(&out_dir).unwrap();

    let evil_zip = format!("{work_dir}/evil.zip");
    let safe_zip = format!("{work_dir}/safe.zip");
    let py_script = concat!(
        "import sys, zipfile\n",
        "with zipfile.ZipFile(sys.argv[1], 'w') as z: z.writestr('../escaped.txt', 'pwned')\n",
        "with zipfile.ZipFile(sys.argv[2], 'w') as z: z.writestr('docs/readme.txt', 'hello')\n",
    );
    let py_status = Command::new("python3")
        .args(["-c", py_script, &evil_zip, &safe_zip])
        .status()
        .unwrap();
    assert!(py_status.success());

    // 1. SEC-03: Zip-slip archive is rejected BEFORE extraction runs
    let err_res = extract_archive_into_dir(&evil_zip, "evil.zip", &out_dir, "evil");
    assert!(err_res.is_err());
    assert!(!Path::new(&format!("{work_dir}/escaped.txt")).exists());

    // 2. GAP-06: Virtual archive mounting browses safe.zip directly via archive:// URI
    let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
    let arch_uri = format!("archive://{safe_zip}#/docs");
    rt.navigate_to_path(false, arch_uri.clone());
    assert_eq!(rt.current_pane_path(false), arch_uri);
    assert_eq!(rt.left_entries.len(), 1);
    assert_eq!(rt.left_entries[0].meta.name, "readme.txt");

    let _ = std::fs::remove_dir_all(&work_dir);
}

#[test]
fn test_v070_per_tab_history_split_vertical_drop_and_500k_window_slice() {
    let mut rt = GuiRuntime::new(ManifoldAppState::new_reference_workspace());
    let base_dir = format!("/tmp/manifold_hist_v070_{}", std::process::id());
    let dir_a = format!("{base_dir}/a");
    let dir_b = format!("{base_dir}/b");
    let dir_c = format!("{base_dir}/c");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::create_dir_all(&dir_c).unwrap();

    // 1. UI-08: Per-tab Back/Forward navigation stacks stay strictly isolated across tabs
    rt.app_state.active_pane = ActivePane::Left;
    rt.app_state.left_active_tab = 0;
    rt.app_state.left_tabs[0].back_stack.clear();
    rt.app_state.left_tabs[0].forward_stack.clear();
    rt.app_state.left_tabs[0].current_path = dir_a.clone();
    rt.navigate_to_path(false, dir_b.clone());
    assert_eq!(rt.app_state.left_tabs[0].back_stack, vec![dir_a.clone()]);

    rt.app_state.left_active_tab = 1;
    rt.app_state.left_tabs[1].back_stack.clear();
    rt.app_state.left_tabs[1].current_path = dir_c.clone();
    assert!(rt.app_state.left_tabs[1].back_stack.is_empty());

    rt.app_state.left_active_tab = 0;
    rt.go_back();
    assert_eq!(rt.current_pane_path(false), dir_a);
    assert_eq!(rt.app_state.left_tabs[0].forward_stack, vec![dir_b.clone()]);
    rt.go_forward();
    assert_eq!(rt.current_pane_path(false), dir_b);

    // 2. BUG-04 & GAP-07: Non-macOS edge drag does NOT trigger external drop, and SplitVertical
    // resolves drops in the bottom pane to the Right/Bottom pane directory.
    let items = [sample_meta("payload.bin")];
    let edge_non_mac = rt.resolve_drop_target_for_os(
        false,
        &items,
        (4.0, 300.0, 700.0, 0.0, 0.0),
        (200.0, 1280.0, 800.0),
        false,
    );
    assert!(edge_non_mac.dst_dir.is_none());

    rt.app_state.dual_pane_enabled = true;
    rt.app_state.panes_layout = PanesLayout::SplitVertical;
    rt.app_state.split_ratio = 0.5;
    rt.app_state.right_active_tab = 0;
    rt.app_state.right_tabs[0].current_path = dir_c.clone();
    let bottom_drop = rt.resolve_drop_target_for_os(
        false,
        &items,
        (500.0, 650.0, 900.0, 0.0, 0.0),
        (200.0, 1280.0, 800.0),
        false,
    );
    assert_eq!(bottom_drop.dst_dir.as_deref(), Some(dir_c.as_str()));
    assert!(bottom_drop.hover_right_pane);

    // 3. GAP-02: 500,000-entry virtualized window slice extracts middle 300 rows without truncation
    let entries_500k: Vec<FlattenedRowEntry> = (0..500_000usize)
        .map(|i| FlattenedRowEntry {
            meta: FileMetadata {
                name: format!("item_{i:06}.log"),
                path: format!("/logs/item_{i:06}.log"),
                extension: Some("log".to_string()),
                kind: "Log".to_string(),
                size: Some(64),
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
            },
            depth: 0,
            is_expanded: false,
        })
        .collect();
    let mut sel = HashSet::new();
    sel.insert(400_005);
    let thumbs = HashMap::new();
    let mut missing = Vec::new();
    let vo = FolderViewOptions::default();
    let win_rows = build_slint_rows_from_flattened(
        &entries_500k,
        &sel,
        &thumbs,
        &mut missing,
        &vo,
        (400_000, None),
    );
    assert_eq!(win_rows.len(), VIRTUAL_WINDOW_CAP);
    assert_eq!(win_rows[0].name.as_str(), "item_400000.log");
    assert!(win_rows[5].is_selected);

    let _ = std::fs::remove_dir_all(&base_dir);
}
