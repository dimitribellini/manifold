use super::*;

pub(super) fn is_executable_file(meta: &FileMetadata) -> bool {
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

#[cfg(windows)]
pub(super) fn move_path_to_trash_windows(path: &str) -> Result<String, String> {
    #[repr(C)]
    #[allow(dead_code)]
    struct ShFileOpStructW {
        hwnd: isize,
        w_func: u32,
        p_from: *const u16,
        p_to: *const u16,
        f_flags: u16,
        f_any_operations_aborted: i32,
        h_name_mappings: *mut std::ffi::c_void,
        lpsz_progress_title: *const u16,
    }
    #[link(name = "shell32")]
    extern "system" {
        fn SHFileOperationW(lp_file_op: *mut ShFileOpStructW) -> i32;
    }
    const FO_DELETE: u32 = 3;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOERRORUI: u16 = 0x0400;

    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    wide.push(0);
    let mut op = ShFileOpStructW {
        hwnd: 0,
        w_func: FO_DELETE,
        p_from: wide.as_ptr(),
        p_to: std::ptr::null(),
        f_flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT,
        f_any_operations_aborted: 0,
        h_name_mappings: std::ptr::null_mut(),
        lpsz_progress_title: std::ptr::null(),
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc == 0 && op.f_any_operations_aborted == 0 {
        Ok("Recycle Bin".to_string())
    } else {
        Err(format!("Failed to move item to Recycle Bin (code {rc})"))
    }
}

#[cfg(not(windows))]
fn encode_trashinfo_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for &b in raw.as_bytes() {
        let is_unreserved =
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(not(windows))]
fn ensure_dir_mode_0700(dir: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub(super) fn move_path_to_trash(vfs: &LocalFsBackend, path: &str) -> Result<String, String> {
    #[cfg(windows)]
    {
        let _ = vfs;
        move_path_to_trash_windows(path)
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .ok_or_else(|| "Cannot locate HOME directory for Trash".to_string())?;
        let trimmed_home = home.trim_end_matches('/');
        let (trash_files_dir, trash_info_dir) = if cfg!(target_os = "macos") {
            let mac_trash = format!("{trimmed_home}/.Trash");
            ensure_dir_mode_0700(&mac_trash)?;
            (mac_trash, None)
        } else {
            let data_home = std::env::var("XDG_DATA_HOME")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("{trimmed_home}/.local/share"));
            let base = format!("{}/Trash", data_home.trim_end_matches('/'));
            ensure_dir_mode_0700(&base)?;
            let files_d = format!("{base}/files");
            let info_d = format!("{base}/info");
            ensure_dir_mode_0700(&files_d)?;
            ensure_dir_mode_0700(&info_d)?;
            (files_d, Some(info_d))
        };
        let name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("item");

        let mut trash_name = name.to_string();
        let mut candidate = format!("{trash_files_dir}/{trash_name}");
        let mut reserved_info_path: Option<String> = None;
        let mut counter = 2u32;

        loop {
            let cand_exists =
                Path::new(&candidate).exists() || std::fs::symlink_metadata(&candidate).is_ok();
            if !cand_exists {
                if let Some(info_dir) = &trash_info_dir {
                    let info_path = format!("{info_dir}/{trash_name}.trashinfo");
                    let open_res = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&info_path);
                    if let Ok(mut f) = open_res {
                        let (yr, mo, dy, hr, mn) = manifold_vfs::current_local_civil_datetime();
                        let del_ts = format!("{yr:04}-{mo:02}-{dy:02}T{hr:02}:{mn:02}:00");
                        let enc_path = encode_trashinfo_path(path);
                        let body =
                            format!("[Trash Info]\nPath={enc_path}\nDeletionDate={del_ts}\n");
                        let _ = f.write_all(body.as_bytes());
                        reserved_info_path = Some(info_path);
                        break;
                    }
                } else {
                    break;
                }
            }
            trash_name = format!("{name} {counter}");
            candidate = format!("{trash_files_dir}/{trash_name}");
            counter += 1;
        }

        if std::fs::rename(path, &candidate).is_ok() {
            return Ok(candidate);
        }

        let copy_res = if Path::new(path).is_dir() {
            copy_local_dir_preserving_bundle(path, &candidate)
        } else {
            vfs.copy_file(path, &candidate).map_err(|e| e.to_string())
        };
        if let Err(err) = copy_res {
            if let Some(info_p) = &reserved_info_path {
                let _ = std::fs::remove_file(info_p);
            }
            return Err(err);
        }
        if let Err(err) = vfs.remove(path, true) {
            let _ = vfs.remove(&candidate, true);
            if let Some(info_p) = &reserved_info_path {
                let _ = std::fs::remove_file(info_p);
            }
            return Err(err.to_string());
        }
        Ok(candidate)
    }
}

pub(super) fn duplicate_path_on_disk(
    vfs: &LocalFsBackend,
    meta: &FileMetadata,
) -> Result<String, String> {
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
        copy_local_dir_preserving_bundle(&meta.path, &dst_path)?;
    } else {
        vfs.copy_file(&meta.path, &dst_path)
            .map_err(|e| e.to_string())?;
    }
    Ok(dst_path)
}
