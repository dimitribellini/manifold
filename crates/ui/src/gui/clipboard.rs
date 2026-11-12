use super::*;

pub(super) fn open_with_system_handler(path: &str) {
    if cfg!(target_os = "macos") {
        sanitize_macos_app_bundle(Path::new(path), true);
        let _ = Command::new("open").arg(path).spawn();
    } else if cfg!(target_os = "windows") {
        let _ = Command::new("explorer").arg(path).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(path).spawn();
    }
}

pub(super) fn reveal_in_system_file_manager(path: &str) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("open").args(["-R", path]).spawn();
    } else if cfg!(target_os = "windows") {
        let select_arg = format!("/select,{path}");
        let _ = Command::new("explorer").arg(select_arg).spawn();
    } else if let Some(parent) = Path::new(path).parent().and_then(|p| p.to_str()) {
        let _ = Command::new("xdg-open").arg(parent).spawn();
    }
}

pub(super) fn encode_file_uri(path: &str) -> String {
    let mut out = String::from("file://");
    for &b in path.as_bytes() {
        let is_safe = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~');
        if is_safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(super) fn decode_file_uri(raw_line: &str) -> Option<String> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let rest = if let Some(r) = trimmed.strip_prefix("file://localhost/") {
        format!("/{r}")
    } else if let Some(r) = trimmed.strip_prefix("file://") {
        r.to_string()
    } else {
        trimmed.to_string()
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

pub(super) fn copy_string_to_system_clipboard(text: &str) {
    if cfg!(target_os = "macos") {
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
        return;
    }
    if cfg!(target_os = "windows") {
        if let Ok(mut child) = Command::new("clip").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
        return;
    }
    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let wl: (&str, &[&str]) = ("wl-copy", &[]);
    let xc: (&str, &[&str]) = ("xclip", &["-selection", "clipboard"]);
    let xs: (&str, &[&str]) = ("xsel", &["--clipboard", "--input"]);
    let candidates = if is_wayland {
        [wl, xc, xs]
    } else {
        [xc, xs, wl]
    };
    for (bin, args) in candidates {
        if let Ok(mut child) = Command::new(bin).args(args).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return;
            }
        }
    }
}

pub(super) fn read_string_from_system_clipboard() -> Option<String> {
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
    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let wl: (&str, &[&str]) = ("wl-paste", &["--no-newline"]);
    let xc: (&str, &[&str]) = ("xclip", &["-selection", "clipboard", "-o"]);
    let xs: (&str, &[&str]) = ("xsel", &["--clipboard", "--output"]);
    let candidates = if is_wayland {
        [wl, xc, xs]
    } else {
        [xc, xs, wl]
    };
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(args).output() {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

pub(super) fn copy_files_to_os_clipboard(paths: &[String]) {
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
        let joined = local_paths.join("\n");
        let ps_cmd = concat!(
            "$p = $env:MANIFOLD_CLIP_PATHS -split \"`n\" | Where-Object { $_ -ne '' }; ",
            "if ($p) { Set-Clipboard -LiteralPath $p }"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", ps_cmd])
            .env("MANIFOLD_CLIP_PATHS", joined)
            .spawn();
    } else {
        let uri_list: String = local_paths
            .iter()
            .map(|p| format!("{}\r\n", encode_file_uri(p)))
            .collect();
        let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let wl: (&str, &[&str]) = ("wl-copy", &["--type", "text/uri-list"]);
        let xc: (&str, &[&str]) = ("xclip", &["-selection", "clipboard", "-t", "text/uri-list"]);
        let candidates = if is_wayland { [wl, xc] } else { [xc, wl] };
        for (bin, args) in candidates {
            if let Ok(mut child) = Command::new(bin).args(args).stdin(Stdio::piped()).spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(uri_list.as_bytes());
                }
                if child.wait().is_ok_and(|s| s.success()) {
                    return;
                }
            }
        }
    }
}

pub(super) fn read_files_from_os_clipboard() -> Vec<String> {
    if cfg!(target_os = "macos") {
        let script = concat!(
            "try\n",
            "set theFiles to the clipboard as «class furl»\n",
            "return POSIX path of theFiles\n",
            "end try"
        );
        if let Ok(out) = Command::new("osascript").args(["-e", script]).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                let paths: Vec<String> = s
                    .lines()
                    .map(str::trim)
                    .filter(|p| !p.is_empty() && Path::new(p).exists())
                    .map(ToOwned::to_owned)
                    .collect();
                if !paths.is_empty() {
                    return paths;
                }
            }
        }
        return Vec::new();
    }
    if cfg!(target_os = "windows") {
        let ps_cmd = "Get-Clipboard -Format FileDropList | ForEach-Object { $_.FullName }";
        if let Ok(out) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", ps_cmd])
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                return s
                    .lines()
                    .map(str::trim)
                    .filter(|p| !p.is_empty() && Path::new(p).exists())
                    .map(ToOwned::to_owned)
                    .collect();
            }
        }
        return Vec::new();
    }
    let is_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let wl: (&str, &[&str]) = ("wl-paste", &["--type", "text/uri-list"]);
    let xc: (&str, &[&str]) = (
        "xclip",
        &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
    );
    let candidates = if is_wayland { [wl, xc] } else { [xc, wl] };
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(args).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                let paths: Vec<String> = s
                    .lines()
                    .filter_map(decode_file_uri)
                    .filter(|p| Path::new(p).exists())
                    .collect();
                if !paths.is_empty() {
                    return paths;
                }
            }
        }
    }
    Vec::new()
}

pub(super) fn compute_local_dir_fingerprint(dir_path: &str) -> Option<u64> {
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

pub(super) fn window_logical_bounds(w: &ManifoldWindow) -> (f32, f32, f32) {
    let scale = w.window().scale_factor().max(1.0);
    let phys = w.window().size();
    let win_w = ((phys.width as f32) / scale).max(600.0);
    let win_h = ((phys.height as f32) / scale).max(400.0);
    (w.get_sidebar_width(), win_w, win_h)
}

pub(super) fn clear_window_drag_overlay(w: &ManifoldWindow) {
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

pub(super) fn schedule_async_pointer_release(
    win_weak: slint::Weak<ManifoldWindow>,
    abs_x: f32,
    abs_y: f32,
) {
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

pub(super) fn reset_slint_modifiers(w: &ManifoldWindow) {
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
pub(super) fn sanitize_click_modifiers(
    w: &ManifoldWindow,
    is_shift: bool,
    is_cmd: bool,
) -> (bool, bool) {
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
pub(super) fn sanitize_click_modifiers(
    _w: &ManifoldWindow,
    is_shift: bool,
    is_cmd: bool,
) -> (bool, bool) {
    (is_shift, is_cmd)
}

#[cfg(target_os = "macos")]
pub(super) fn is_primary_mouse_button_pressed() -> bool {
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
pub(super) fn is_primary_mouse_button_pressed() -> bool {
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
pub(super) struct MacOsPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct MacOsSize {
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct MacOsRect {
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
pub(super) fn start_native_os_file_drag(paths: &[String]) -> bool {
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
pub(super) fn start_native_os_file_drag(_paths: &[String]) -> bool {
    false
}
