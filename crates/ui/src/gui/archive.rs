use super::*;

pub(super) fn is_extractable_archive(meta: &FileMetadata) -> bool {
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

pub(super) fn archive_output_stem(file_name: &str) -> String {
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

pub(super) fn should_show_in_enclosing_folder(
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

pub(super) fn run_tar_extract(
    archive_path: &str,
    dest_dir: &str,
) -> std::io::Result<std::process::ExitStatus> {
    let safe_arc = manifold_vfs::safe_cli_path_arg(archive_path);
    let safe_dst = manifold_vfs::safe_cli_path_arg(dest_dir);
    if cfg!(target_os = "macos") {
        Command::new("tar")
            .args(["--no-mac-metadata", "-xf", &safe_arc, "-C", &safe_dst])
            .status()
            .or_else(|_| {
                Command::new("tar")
                    .args(["-xf", &safe_arc, "-C", &safe_dst])
                    .status()
            })
    } else {
        let mut cmd = Command::new("tar");
        if cfg!(target_os = "linux") && safe_arc.contains(':') {
            cmd.arg("--force-local");
        }
        cmd.args(["-xf", &safe_arc, "-C", &safe_dst]).status()
    }
}

pub(super) fn extract_archive_into_dir(
    archive_path: &str,
    archive_name: &str,
    dest_dir: &str,
    stem: &str,
) -> Result<(), String> {
    let lower = archive_name.to_lowercase();
    let is_single_stream = lower.ends_with(".gz")
        && !lower.ends_with(".tar.gz")
        && !lower.ends_with(".tgz")
        || (lower.ends_with(".bz2") && !lower.ends_with(".tar.bz2") && !lower.ends_with(".tbz2"))
        || (lower.ends_with(".xz") && !lower.ends_with(".tar.xz") && !lower.ends_with(".txz"));
    if !is_single_stream {
        if let Some(fmt) = ArchiveFormat::detect(archive_path) {
            manifold_vfs::inspect_archive_entries_on_disk(archive_path, fmt)
                .map_err(|e| e.to_string())?;
        }
    } else if ArchiveBackend::normalize_virtual_path(stem).is_err() {
        return Err(format!("Unsafe archive stem: {stem}"));
    }
    let safe_arc = manifold_vfs::safe_cli_path_arg(archive_path);
    let safe_dst = manifold_vfs::safe_cli_path_arg(dest_dir);
    let status = if lower.ends_with(".zip") {
        Command::new("unzip")
            .args(["-q", "-o", &safe_arc, "-d", &safe_dst])
            .status()
            .or_else(|_| run_tar_extract(archive_path, dest_dir))
    } else if lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".tbz2")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
        || lower.ends_with(".tar")
    {
        run_tar_extract(archive_path, dest_dir)
    } else if lower.ends_with(".7z") {
        let out_arg = format!("-o{dest_dir}");
        Command::new("7z")
            .args(["x", "-y", &out_arg, "--", &safe_arc])
            .status()
    } else if lower.ends_with(".rar") {
        Command::new("unrar")
            .args(["x", "-y", "--", &safe_arc, &safe_dst])
            .status()
    } else if lower.ends_with(".gz") {
        let out_file = format!("{dest_dir}/{stem}");
        decompress_stream_to_file("gzip", archive_path, &out_file)
    } else if lower.ends_with(".bz2") {
        let out_file = format!("{dest_dir}/{stem}");
        decompress_stream_to_file("bzip2", archive_path, &out_file)
    } else if lower.ends_with(".xz") {
        let out_file = format!("{dest_dir}/{stem}");
        decompress_stream_to_file("xz", archive_path, &out_file)
    } else {
        run_tar_extract(archive_path, dest_dir)
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

pub(super) fn decompress_stream_to_file(
    tool: &str,
    archive_path: &str,
    out_file: &str,
) -> std::io::Result<std::process::ExitStatus> {
    let safe_arc = manifold_vfs::safe_cli_path_arg(archive_path);
    let target = std::fs::File::create(out_file)?;
    Command::new(tool)
        .args(["-dc", "--", &safe_arc])
        .stdout(Stdio::from(target))
        .status()
}

pub(super) fn collect_macos_app_bundles(root: &Path) -> Vec<PathBuf> {
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
pub(super) fn ensure_macos_bundle_executables_chmod(app_dir: &Path) {
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
pub(super) fn ensure_macos_bundle_executables_chmod(_app_dir: &Path) {}

pub(super) fn sanitize_macos_app_bundle(path: &Path, _is_macos: bool) {
    if !path.exists() {
        return;
    }
    let bundles = collect_macos_app_bundles(path);
    for app_dir in &bundles {
        ensure_macos_bundle_executables_chmod(app_dir);
    }
}

fn copy_dir_recursive_rust(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry_res in std::fs::read_dir(src)? {
        let entry = entry_res?;
        let ft = entry.file_type()?;
        let src_child = entry.path();
        let dst_child = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive_rust(&src_child, &dst_child)?;
        } else if ft.is_symlink() {
            #[cfg(unix)]
            {
                let link_target = std::fs::read_link(&src_child)?;
                std::os::unix::fs::symlink(link_target, &dst_child)?;
            }
            #[cfg(not(unix))]
            {
                std::fs::copy(&src_child, &dst_child)?;
            }
        } else {
            std::fs::copy(&src_child, &dst_child)?;
        }
    }
    Ok(())
}

pub(super) fn copy_local_dir_preserving_bundle(
    src_path: &str,
    dst_path: &str,
) -> Result<(), String> {
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
            .args(["--noqtn", "--", src_path, &stage_dst_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ditto_ok {
            true
        } else {
            Command::new("cp")
                .args(["-RPp", "--", src_path, &stage_dst_str])
                .status()
                .is_ok_and(|s| s.success())
                || copy_dir_recursive_rust(Path::new(src_path), &stage_dst).is_ok()
        }
    } else if cfg!(windows) {
        copy_dir_recursive_rust(Path::new(src_path), &stage_dst).is_ok()
    } else {
        let cp_p_ok = Command::new("cp")
            .args(["-RPp", "--", src_path, &stage_dst_str])
            .status()
            .is_ok_and(|s| s.success());
        if cp_p_ok {
            true
        } else {
            Command::new("cp")
                .args(["-R", "--", src_path, &stage_dst_str])
                .status()
                .is_ok_and(|s| s.success())
                || copy_dir_recursive_rust(Path::new(src_path), &stage_dst).is_ok()
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

pub(super) fn significant_dir_children(dir: &Path) -> Vec<PathBuf> {
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

pub(super) fn find_single_macos_app_in_extracted_dir(extracted_dir: &Path) -> Option<PathBuf> {
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

pub(super) fn finalize_extracted_archive_dir(
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

pub(super) fn extract_archive_to_folder(meta: &FileMetadata) -> Result<String, String> {
    extract_archive_to_folder_for_os(meta, cfg!(target_os = "macos"))
}

pub(super) fn extract_archive_to_folder_for_os(
    meta: &FileMetadata,
    is_macos: bool,
) -> Result<String, String> {
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

pub(super) fn open_archive_contents_dir(
    meta: &FileMetadata,
    is_macos: bool,
) -> Result<String, String> {
    if meta.path.contains("://") {
        return Err("Remote archive inspection requires downloading first".to_string());
    }
    let stem = archive_output_stem(&meta.name);
    let base_dir =
        manifold_vfs::secure_runtime_subdir("archive-contents").map_err(|e| e.to_string())?;
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

pub(super) fn unwrap_nested_macos_app_in_stage_dir(stage_dir: &Path) {
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

pub(super) fn current_iso_date_string() -> String {
    let (y, m, d, _, _) = manifold_vfs::current_local_civil_datetime();
    format!("{y:04}-{m:02}-{d:02}")
}

pub(super) fn default_compress_stem(sel_list: &[FileMetadata]) -> String {
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

pub(super) fn normalize_archive_base_name(raw_name: &str, format: &str) -> String {
    let sanitized: String = raw_name
        .trim()
        .chars()
        .filter(|&c| !matches!(c, '/' | '\\' | ':' | '\0') && !c.is_control())
        .collect();
    let trimmed = sanitized.trim();
    if trimmed == "." || trimmed == ".." || trimmed.contains("..") {
        return String::new();
    }
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

pub(super) fn create_archive_from_selection(
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

    let safe_names: Vec<String> = names.iter().map(|n| format!("./{n}")).collect();

    let safe_arc_out = manifold_vfs::safe_cli_path_arg(&archive_path);
    if ext == "zip" {
        let zip_res = Command::new("zip")
            .current_dir(parent)
            .args(["-r", "-q", &safe_arc_out, "--"])
            .args(&safe_names)
            .status();
        let zip_ok = zip_res.is_ok_and(|s| s.success());
        if !zip_ok {
            let py_res = Command::new("python3")
                .current_dir(parent)
                .args(["-m", "zipfile", "-c", &safe_arc_out])
                .args(&safe_names)
                .status();
            let py_ok = py_res.is_ok_and(|s| s.success());
            if !py_ok {
                let mut tar_cmd = Command::new("tar");
                if cfg!(target_os = "linux") && safe_arc_out.contains(':') {
                    tar_cmd.arg("--force-local");
                }
                let tar_res = tar_cmd
                    .args(["-caf", &safe_arc_out, "-C", parent, "--"])
                    .args(&safe_names)
                    .status()
                    .map_err(|e| format!("Failed to create ZIP archive: {e}"))?;
                if !tar_res.success() {
                    return Err(format!("ZIP archiver exited with status {tar_res}"));
                }
            }
        }
    } else {
        let mut tar_cmd = Command::new("tar");
        if cfg!(target_os = "linux") && safe_arc_out.contains(':') {
            tar_cmd.arg("--force-local");
        }
        let status = tar_cmd
            .args(["-czf", &safe_arc_out, "-C", parent, "--"])
            .args(&safe_names)
            .status()
            .map_err(|e| format!("Failed to run tar: {e}"))?;
        if !status.success() {
            return Err(format!("tar exited with status {status}"));
        }
    }
    Ok(archive_path)
}

pub(super) fn try_unmount_network_share(mount_path: &str) {
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
