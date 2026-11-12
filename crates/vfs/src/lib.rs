//! Unified Virtual File System (`manifold-vfs`) for Project Manifold.
//!
//! Provides the core [`FileSystemBackend`] trait, [`FileMetadata`], [`PermissionsMatrix`],
//! [`ColorLabel`], [`VolumeStats`], [`LocalFsBackend`], and [`ArchiveBackend`].

use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unified error type across all local, archive, and remote VFS providers.
#[derive(Debug, Error)]
pub enum VfsError {
    #[error("Item not found: {0}")]
    NotFound(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Item already exists: {0}")]
    AlreadyExists(String),

    #[error("Invalid or unsafe path: {0}")]
    InvalidPath(String),

    #[error("Network or transport error: {0}")]
    NetworkError(String),

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Unsupported VFS operation: {0}")]
    UnsupportedOperation(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

/// 8-swatch Color Label tag system matching the List View dots (`doc/image1.png`)
/// and Connection Dialog swatch selector (`doc/image3.png`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorLabel {
    #[default]
    None,
    Gray,
    Green,
    Purple,
    Blue,
    Yellow,
    Red,
    Orange,
}

impl ColorLabel {
    /// Returns the display hex color used by the UI renderer for the tag dot.
    pub fn hex_color(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Gray => Some("#8E8E93"),
            Self::Green => Some("#5AC85A"),
            Self::Purple => Some("#AF52DE"),
            Self::Blue => Some("#0A84FF"),
            Self::Yellow => Some("#FFD60A"),
            Self::Red => Some("#FF453A"),
            Self::Orange => Some("#FF9F0A"),
        }
    }

    /// Returns all 8 swatches in the exact order rendered in `doc/image3.png`.
    pub const fn all_swatches() -> [Self; 8] {
        [
            Self::None,
            Self::Gray,
            Self::Green,
            Self::Purple,
            Self::Blue,
            Self::Yellow,
            Self::Red,
            Self::Orange,
        ]
    }
}

/// R/W/X triad for a single principal (`Owner`, `Group`, or `Others`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionTriad {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl PermissionTriad {
    pub const fn from_octal_digit(digit: u32) -> Self {
        Self {
            read: (digit & 0b100) != 0,
            write: (digit & 0b010) != 0,
            execute: (digit & 0b001) != 0,
        }
    }

    pub const fn to_octal_digit(self) -> u32 {
        (if self.read { 4 } else { 0 })
            | (if self.write { 2 } else { 0 })
            | (if self.execute { 1 } else { 0 })
    }

    pub const fn to_symbolic(self) -> [char; 3] {
        [
            if self.read { 'r' } else { '-' },
            if self.write { 'w' } else { '-' },
            if self.execute { 'x' } else { '-' },
        ]
    }
}

/// Interactive 3x3 `R / W / X` Permissions matrix + Octal representation
/// displayed in the Right Inspector (`doc/image1.png` and `doc/image2.png`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsMatrix {
    pub owner: PermissionTriad,
    pub group: PermissionTriad,
    pub others: PermissionTriad,
    pub special_bits: u32,
}

impl PermissionsMatrix {
    /// Constructs a matrix from a raw POSIX mode (e.g. `0o100644` or `0o644`).
    pub const fn from_mode(mode: u32) -> Self {
        let perm = mode & 0o777;
        let special = (mode >> 9) & 0o7;
        Self {
            owner: PermissionTriad::from_octal_digit((perm >> 6) & 0o7),
            group: PermissionTriad::from_octal_digit((perm >> 3) & 0o7),
            others: PermissionTriad::from_octal_digit(perm & 0o7),
            special_bits: special,
        }
    }

    /// Parses a 3- or 4-digit octal string (e.g. `"644"` or `"0755"`) from the UI badge.
    pub fn from_octal_str(input: &str) -> Result<Self, VfsError> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.len() > 4 {
            let msg = format!("Invalid octal permission string: {input}");
            return Err(VfsError::InvalidPath(msg));
        }
        let mode = u32::from_str_radix(trimmed, 8)
            .map_err(|_| VfsError::InvalidPath(format!("Non-octal permission string: {input}")))?;
        Ok(Self::from_mode(mode))
    }

    /// Converts the matrix back into a 12-bit POSIX permission mode (`0o000`..=`0o7777`).
    pub const fn to_mode(self) -> u32 {
        (self.special_bits << 9)
            | (self.owner.to_octal_digit() << 6)
            | (self.group.to_octal_digit() << 3)
            | self.others.to_octal_digit()
    }

    /// Preserves upper POSIX mode bits (`SUID` `0o4000`, `SGID` `0o2000`, sticky `0o1000`)
    /// from `original_mode` when updating `R/W/X` triads (`UI-10`).
    pub const fn with_preserved_special_bits(mut self, original_mode: u32) -> Self {
        self.special_bits = (original_mode >> 9) & 0o7;
        self
    }

    /// Formats the 3-digit octal string (e.g. `"644"`).
    pub fn to_octal_string(self) -> String {
        format!(
            "{}{}{}",
            self.owner.to_octal_digit(),
            self.group.to_octal_digit(),
            self.others.to_octal_digit()
        )
    }

    /// Formats the 9-character symbolic string (e.g. `"rw-r--r--"`).
    pub fn to_symbolic_string(self) -> String {
        let mut out = String::with_capacity(9);
        for c in self.owner.to_symbolic() {
            out.push(c);
        }
        for c in self.group.to_symbolic() {
            out.push(c);
        }
        for c in self.others.to_symbolic() {
            out.push(c);
        }
        out
    }
}

/// Disk volume capacity metrics displayed in Sidebar (`Devices`) and the
/// Combined Path & Status header (`"19 items, 354,54 GB available"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStats {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl VolumeStats {
    /// Formats available space using comma-decimal gigabytes matching `doc/image1.png` (`"354,54 GB"`).
    pub fn format_available_gb(self) -> String {
        format_bytes_comma(self.available_bytes)
    }
}

/// Formats a byte count into human-readable units matching the reference screenshots
/// (e.g. `"351 bytes"`, `"4 KB"`, `"9,3 MB"`, `"354,54 GB"`).
pub fn format_bytes_comma(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    if bytes < 1024 {
        format!("{bytes} bytes")
    } else if (bytes as f64) < MB {
        let kb = (bytes as f64 / KB).round() as u64;
        format!("{kb} KB")
    } else if (bytes as f64) < GB {
        let mb = bytes as f64 / MB;
        if (mb - mb.round()).abs() < 0.05 {
            format!("{} MB", mb.round() as u64)
        } else {
            format!("{:.1} MB", mb).replace('.', ",")
        }
    } else {
        let gb = bytes as f64 / GB;
        format!("{:.2} GB", gb).replace('.', ",")
    }
}

/// Standardized metadata structure for any file, directory, or symlink across VFS backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub name: String,
    pub path: String,
    pub extension: Option<String>,
    pub kind: String,
    /// `None` for directories whose recursive size has not yet been calculated (`"--"` in `doc/image2.png`),
    /// or `Some(bytes)` for files and calculated directories (`"9 MB"` in `doc/image1.png`).
    pub size: Option<u64>,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_hidden: bool,
    pub is_locked: bool,
    pub hide_extension: bool,
    pub color_label: ColorLabel,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub added: Option<SystemTime>,
    pub permissions: u32,
    pub owner: Option<String>,
    pub group: Option<String>,
}

impl FileMetadata {
    /// Returns the display name respecting the `hide_extension` inspector setting.
    pub fn display_name(&self) -> String {
        if self.hide_extension && !self.is_dir {
            if let Some(ext) = &self.extension {
                let suffix = format!(".{ext}");
                let stem = self.name.strip_suffix(&suffix).unwrap_or("");
                if !stem.is_empty() {
                    return stem.to_string();
                }
            }
        }
        self.name.clone()
    }

    /// Returns the formatted size column string (`"--"` for uncalculated folders, or `"9,3 MB"`).
    pub fn formatted_size(&self) -> String {
        match self.size {
            Some(bytes) => format_bytes_comma(bytes),
            None => "--".to_string(),
        }
    }

    /// Returns the interactive [`PermissionsMatrix`] for the Inspector pane.
    pub fn permissions_matrix(&self) -> PermissionsMatrix {
        PermissionsMatrix::from_mode(self.permissions)
    }

    /// Formats `modified` as `"YYYY.MM.DD"` for List View columns (`doc/image1.png`).
    pub fn formatted_modified_date(&self) -> String {
        match self.modified {
            Some(t) => {
                let (y, m, d, _, _) = system_time_to_civil(t);
                format!("{y:04}.{m:02}.{d:02}")
            }
            None => "--".to_string(),
        }
    }

    /// Formats `modified` as `"YYYY.MM.DD, HH:MM"` for the Inspector (`doc/image1.png`).
    pub fn formatted_modified_datetime(&self) -> String {
        match self.modified {
            Some(t) => {
                let (y, m, d, hh, mm) = system_time_to_civil(t);
                format!("{y:04}.{m:02}.{d:02}, {hh:02}:{mm:02}")
            }
            None => "--".to_string(),
        }
    }

    /// Formats `modified` as `"YYYY-MM-DD HH:mm"` for standardized table columns (`specification_1.md`).
    pub fn formatted_iso_datetime(&self) -> String {
        match self.modified {
            Some(t) => {
                let (y, m, d, hh, mm) = system_time_to_civil(t);
                format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
            }
            None => "--".to_string(),
        }
    }

    /// Formats `modified` as `"DD/MM/YYYY, HH:mm"` for the Conflict Resolution Modal.
    pub fn formatted_conflict_datetime(&self) -> String {
        match self.modified {
            Some(t) => {
                let (y, m, d, hh, mm) = system_time_to_civil(t);
                format!("{d:02}/{m:02}/{y:04}, {hh:02}:{mm:02}")
            }
            None => "--".to_string(),
        }
    }
}

/// Converts a [`SystemTime`] into `(year, month, day, hour, minute)` in UTC.
pub fn system_time_to_utc_civil(time: SystemTime) -> (i32, u32, u32, u32, u32) {
    let secs = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m, d, hour, minute)
}

/// Converts a [`SystemTime`] into `(year, month, day, hour, minute)` in the host OS local timezone (`UI-07`).
pub fn system_time_to_civil(time: SystemTime) -> (i32, u32, u32, u32, u32) {
    let secs = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return (1970, 1, 1, 0, 0),
    };
    #[cfg(unix)]
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&t, &mut tm).is_null() {
            let year = tm.tm_year + 1900;
            let month = (tm.tm_mon + 1).clamp(1, 12) as u32;
            let day = tm.tm_mday.clamp(1, 31) as u32;
            let hour = tm.tm_hour.clamp(0, 23) as u32;
            let minute = tm.tm_min.clamp(0, 59) as u32;
            return (year, month, day, hour, minute);
        }
    }
    #[cfg(windows)]
    unsafe {
        #[repr(C)]
        #[derive(Clone, Copy)]
        #[allow(dead_code)]
        struct WinFileTime {
            dw_low_date_time: u32,
            dw_high_date_time: u32,
        }
        #[repr(C)]
        #[allow(dead_code)]
        struct WinSystemTime {
            w_year: u16,
            w_month: u16,
            w_day_of_week: u16,
            w_day: u16,
            w_hour: u16,
            w_minute: u16,
            w_second: u16,
            w_milliseconds: u16,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn FileTimeToLocalFileTime(
                lp_file_time: *const WinFileTime,
                lp_local_file_time: *mut WinFileTime,
            ) -> i32;
            fn FileTimeToSystemTime(
                lp_file_time: *const WinFileTime,
                lp_system_time: *mut WinSystemTime,
            ) -> i32;
        }
        let intervals = (secs as u64)
            .saturating_mul(10_000_000)
            .saturating_add(116_444_736_000_000_000);
        let ft = WinFileTime {
            dw_low_date_time: (intervals & 0xFFFF_FFFF) as u32,
            dw_high_date_time: (intervals >> 32) as u32,
        };
        let mut local_ft = WinFileTime {
            dw_low_date_time: 0,
            dw_high_date_time: 0,
        };
        let mut st: WinSystemTime = std::mem::zeroed();
        if FileTimeToLocalFileTime(&ft, &mut local_ft) != 0
            && FileTimeToSystemTime(&local_ft, &mut st) != 0
        {
            return (
                st.w_year as i32,
                st.w_month as u32,
                st.w_day as u32,
                st.w_hour as u32,
                st.w_minute as u32,
            );
        }
    }
    system_time_to_utc_civil(time)
}

/// Returns the current local `(year, month, day, hour, minute)`.
pub fn current_local_civil_datetime() -> (i32, u32, u32, u32, u32) {
    system_time_to_civil(SystemTime::now())
}

static SECURE_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Resolves and initializes a private per-user runtime/cache directory with `0700` permissions (`SEC-07`),
/// avoiding predictable world-writable shared `/tmp/manifold-*` directories.
pub fn secure_runtime_subdir(subdir: &str) -> Result<PathBuf, VfsError> {
    let clean_sub: String = subdir
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    let sub_name = if clean_sub.is_empty() {
        "runtime"
    } else {
        clean_sub.as_str()
    };

    let non_empty_env = |key: &str| {
        let val = std::env::var(key).ok()?;
        (!val.trim().is_empty()).then_some(val)
    };
    let base_root = if let Some(xdg_rt) = non_empty_env("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg_rt).join("manifold")
    } else if let Some(xdg_cache) = non_empty_env("XDG_CACHE_HOME") {
        PathBuf::from(xdg_cache).join("manifold")
    } else if cfg!(target_os = "macos") && non_empty_env("HOME").is_some() {
        PathBuf::from(non_empty_env("HOME").unwrap_or_default())
            .join("Library")
            .join("Caches")
            .join("manifold")
    } else if cfg!(target_os = "windows") && non_empty_env("LOCALAPPDATA").is_some() {
        PathBuf::from(non_empty_env("LOCALAPPDATA").unwrap_or_default())
            .join("Manifold")
            .join("Cache")
    } else if let Some(home) = non_empty_env("HOME").or_else(|| non_empty_env("USERPROFILE")) {
        PathBuf::from(home).join(".cache").join("manifold")
    } else {
        #[cfg(unix)]
        let uid = unsafe { libc::geteuid() };
        #[cfg(not(unix))]
        let uid = std::process::id();
        std::env::temp_dir().join(format!("manifold-runtime-{uid}"))
    };

    ensure_private_dir(&base_root)?;
    let target = base_root.join(sub_name);
    ensure_private_dir(&target)?;
    Ok(target)
}

fn ensure_private_dir(dir: &Path) -> Result<(), VfsError> {
    if !dir.exists() {
        fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() {
            let msg = format!("Refusing symlinked runtime directory: {}", dir.display());
            return Err(VfsError::InvalidPath(msg));
        }
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            let msg = format!("Runtime directory UID mismatch: {}", dir.display());
            return Err(VfsError::PermissionDenied(msg));
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically creates a new private (`0600`, `O_CREAT | O_EXCL`) temporary file inside
/// `secure_runtime_subdir(subdir)` (`SEC-07`).
pub fn create_secure_temp_file(
    subdir: &str,
    prefix: &str,
    suffix: &str,
) -> Result<(PathBuf, fs::File), VfsError> {
    let dir = secure_runtime_subdir(subdir)?;
    let pid = std::process::id();
    for _ in 0..32 {
        let seq = SECURE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let rand_tok = nanos
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(seq);
        let fname = format!("{prefix}-{pid}-{seq}-{rand_tok:016x}{suffix}");
        let candidate = dir.join(fname);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        match opts.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(VfsError::IoError(e)),
        }
    }
    let msg = format!("Failed to allocate unique temp file in {}", dir.display());
    Err(VfsError::IoError(std::io::Error::other(msg)))
}

/// Purges transient runtime staging subdirectories (e.g. `"drag"`, `"archive-contents"`)
/// on clean application shutdown (`SEC-07`).
pub fn cleanup_secure_runtime_subdirs(subdirs: &[&str]) {
    for &sub in subdirs {
        if let Ok(dir) = secure_runtime_subdir(sub) {
            if let Ok(meta) = fs::symlink_metadata(&dir) {
                if meta.is_dir() && !meta.file_type().is_symlink() {
                    let _ = fs::remove_dir_all(&dir);
                }
            }
        }
    }
}

/// Guards a local path passed to CLI tools so leading `-` is never parsed as an option flag (`SEC-03`).
pub fn safe_cli_path_arg(path: &str) -> String {
    if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_string()
    }
}

/// Classifies a human-readable `Kind` label from file extension and directory flag.
pub fn infer_file_kind(name: &str, is_dir: bool) -> (Option<String>, String) {
    infer_file_kind_for_os(name, is_dir, cfg!(target_os = "macos"))
}

/// Classifies a human-readable `Kind` label with explicit macOS `.app` bundle detection.
pub fn infer_file_kind_for_os(
    name: &str,
    is_dir: bool,
    is_macos: bool,
) -> (Option<String>, String) {
    let raw_ext = Path::new(name).extension().and_then(|e| e.to_str());
    let ext = raw_ext.map(str::to_lowercase);
    if is_dir {
        if is_macos && ext.as_deref() == Some("app") {
            return (ext, "Application".to_string());
        }
        return (ext, "Folder".to_string());
    }

    let kind = match ext.as_deref() {
        Some("jpg" | "jpeg") => "JPEG image".to_string(),
        Some("png") => "PNG image".to_string(),
        Some("svg") => "SVG image".to_string(),
        Some("webp") => "WebP image".to_string(),
        Some("avif") => "AVIF image".to_string(),
        Some("php") => "PHP Source".to_string(),
        Some("rs") => "Rust Source".to_string(),
        Some("toml") => "TOML Configuration".to_string(),
        Some("html" | "htm") => "HTML Document".to_string(),
        Some("txt" | "md") => "Plain Text Document".to_string(),
        Some("pdf") => "PDF Document".to_string(),
        Some("zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar") => "Archive".to_string(),
        Some("mp3" | "wav" | "flac" | "ogg") => "Audio File".to_string(),
        Some("mp4" | "mkv" | "mov" | "webm") => "Video File".to_string(),
        Some(other) => format!("{} File", other.to_uppercase()),
        None => "Document".to_string(),
    };
    (ext, kind)
}

/// Core non-blocking VFS backend trait implemented by Local, Archive, and Remote providers.
pub trait FileSystemBackend: Send + Sync {
    /// Identifier of the backend protocol (e.g., `"file"`, `"archive"`, `"sftp"`, `"s3"`).
    fn scheme(&self) -> &str;

    /// Lists all immediate entries in `path`.
    fn list_directory(&self, path: &str) -> Result<Vec<FileMetadata>, VfsError>;

    /// Retrieves metadata for a single file or directory.
    fn metadata(&self, path: &str) -> Result<FileMetadata, VfsError>;

    /// Queries total and available bytes on the underlying volume.
    fn volume_stats(&self, path: &str) -> Result<VolumeStats, VfsError>;

    /// Creates a directory (and parents if missing).
    fn create_dir(&self, path: &str) -> Result<(), VfsError>;

    /// Removes a file or directory.
    fn remove(&self, path: &str, recursive: bool) -> Result<(), VfsError>;

    /// Renames or moves an entry within the same backend.
    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError>;

    /// Copies a file within the same backend.
    fn copy_file(&self, from: &str, to: &str) -> Result<(), VfsError>;

    /// Reads the full byte contents of `path` (or a slice via `read_range`).
    fn read_bytes(&self, path: &str) -> Result<Vec<u8>, VfsError>;

    /// Reads a chunk from `path` starting at `offset` up to `length` bytes (used for 4-8 MB chunked I/O and Hex View).
    fn read_range(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>, VfsError> {
        let all = self.read_bytes(path)?;
        let start = (offset as usize).min(all.len());
        let end = start.saturating_add(length).min(all.len());
        Ok(all[start..end].to_vec())
    }

    /// Writes `data` to `path`, creating or truncating the file.
    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError>;

    /// Appends `data` to `path` (creating the file if missing) without loading existing bytes into RAM (`GAP-05`).
    fn append_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let mut existing = match self.read_bytes(path) {
            Ok(bytes) => bytes,
            Err(VfsError::NotFound(_)) => Vec::new(),
            Err(err) => return Err(err),
        };
        existing.extend_from_slice(data);
        self.write_bytes(path, &existing)
    }

    /// Creates a symbolic link at `link_path` pointing to `target_path`, enforcing sandbox safety (`SEC-09`).
    fn create_symlink(&self, target_path: &str, link_path: &str) -> Result<(), VfsError> {
        let _ = (target_path, link_path);
        Err(VfsError::UnsupportedOperation(
            "Symbolic links are not supported on this backend".to_string(),
        ))
    }

    /// Updates POSIX permissions mode (e.g. `0o644`).
    fn set_permissions(&self, path: &str, mode: u32) -> Result<(), VfsError>;

    /// Updates the 8-color label tag (`user.manifold.color_label`), persisted across sessions.
    fn set_color_label(&self, path: &str, label: ColorLabel) -> Result<(), VfsError>;

    /// Updates the `Hide Extension` attribute (`General` inspector section).
    fn set_hide_extension(&self, path: &str, hide: bool) -> Result<(), VfsError>;

    /// Updates the `Locked` attribute (`Permissions` inspector section).
    fn set_locked(&self, path: &str, locked: bool) -> Result<(), VfsError>;

    /// Recursively computes the total byte size of a directory so the UI can transition
    /// folder size display from `"--"` to `"9 MB"`.
    fn calculate_dir_size(&self, path: &str) -> Result<u64, VfsError> {
        let meta = self.metadata(path)?;
        if !meta.is_dir {
            return Ok(meta.size.unwrap_or(0));
        }
        let mut total = 0u64;
        for entry in self.list_directory(path)? {
            if entry.is_dir && !entry.is_symlink {
                total = total.saturating_add(self.calculate_dir_size(&entry.path)?);
            } else {
                total = total.saturating_add(entry.size.unwrap_or(0));
            }
        }
        Ok(total)
    }
}

/// Extended Manifold metadata attributes (`color_label`, `hide_extension`, `is_locked`, `added`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtendedItemAttributes {
    pub color_label: ColorLabel,
    pub hide_extension: bool,
    pub is_locked: bool,
    pub added: Option<SystemTime>,
}

/// Local filesystem implementation of [`FileSystemBackend`] with optional sandbox root boundary.
#[derive(Clone)]
pub struct LocalFsBackend {
    sandbox_root: Option<PathBuf>,
    attrs_overlay: Arc<RwLock<HashMap<PathBuf, ExtendedItemAttributes>>>,
    dir_size_cache: Arc<RwLock<HashMap<PathBuf, u64>>>,
}

impl Default for LocalFsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalFsBackend {
    /// Creates an unrestricted local filesystem backend.
    pub fn new() -> Self {
        Self {
            sandbox_root: None,
            attrs_overlay: Arc::new(RwLock::new(HashMap::new())),
            dir_size_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Creates a sandboxed local filesystem backend restricted strictly to `root`.
    /// Prevents path traversal attacks (`../` or prefix bypasses).
    pub fn with_sandbox_root(root: impl AsRef<Path>) -> Result<Self, VfsError> {
        let canonical = fs::canonicalize(root.as_ref())?;
        Ok(Self {
            sandbox_root: Some(canonical),
            attrs_overlay: Arc::new(RwLock::new(HashMap::new())),
            dir_size_cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Resolves and validates a user-supplied path against directory traversal and sandbox escape.
    pub fn resolve_safe_path(&self, raw_path: &str) -> Result<PathBuf, VfsError> {
        if raw_path.trim().is_empty() || raw_path.contains('\0') {
            return Err(VfsError::InvalidPath("Empty or null-byte path".to_string()));
        }

        let input_path = Path::new(raw_path);
        // Reject explicit parent-dir traversal components when a sandbox root is active
        if let Some(root) = &self.sandbox_root {
            for comp in input_path.components() {
                if matches!(comp, Component::ParentDir) {
                    let msg = format!("Path traversal component '..' denied for {raw_path}");
                    return Err(VfsError::InvalidPath(msg));
                }
            }
            let joined = if input_path.is_absolute() {
                if input_path.starts_with(root) {
                    input_path.to_path_buf()
                } else {
                    root.join(input_path.strip_prefix("/").unwrap_or(input_path))
                }
            } else {
                root.join(input_path)
            };

            // Verify boundary with strict prefix check
            let normalized = normalize_lexical(&joined);
            if !normalized.starts_with(root) {
                let msg = format!("Path {raw_path} escapes sandbox root");
                return Err(VfsError::InvalidPath(msg));
            }
            // Enforce canonical symlink resolution against sandbox escape (SEC-09)
            let mut probe = normalized.as_path();
            while !probe.exists() && fs::symlink_metadata(probe).is_err() {
                let Some(parent) = probe.parent() else {
                    break;
                };
                if parent == probe {
                    break;
                }
                probe = parent;
            }
            if fs::symlink_metadata(probe)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                let link_target = fs::read_link(probe)?;
                let base_dir = probe.parent().unwrap_or(root);
                let combined = if link_target.is_absolute() {
                    link_target
                } else {
                    base_dir.join(link_target)
                };
                let canon_target =
                    fs::canonicalize(&combined).unwrap_or_else(|_| normalize_lexical(&combined));
                if !canon_target.starts_with(root) {
                    let msg = format!("Symlink {raw_path} escapes sandbox root");
                    return Err(VfsError::InvalidPath(msg));
                }
            }
            if fs::canonicalize(probe)
                .map(|c| !c.starts_with(root))
                .unwrap_or(false)
            {
                let msg = format!("Resolved path {raw_path} escapes sandbox root");
                return Err(VfsError::InvalidPath(msg));
            }
            Ok(normalized)
        } else {
            Ok(normalize_lexical(input_path))
        }
    }

    fn get_attrs(&self, path: &Path) -> Option<ExtendedItemAttributes> {
        let guard = self.attrs_overlay.read().ok()?;
        guard.get(path).cloned()
    }

    /// Seeds persistent file/folder color labels from `preferences.toml` into the overlay.
    pub fn restore_color_labels(&self, labels: &HashMap<String, ColorLabel>) {
        let Ok(mut guard) = self.attrs_overlay.write() else {
            return;
        };
        for (raw_path, color) in labels {
            if *color == ColorLabel::None {
                continue;
            }
            if let Ok(resolved) = self.resolve_safe_path(raw_path) {
                guard.entry(resolved).or_default().color_label = *color;
            }
        }
    }

    fn build_metadata(&self, path: &Path) -> Result<FileMetadata, VfsError> {
        let sym_meta = fs::symlink_metadata(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VfsError::NotFound(path.display().to_string()),
            std::io::ErrorKind::PermissionDenied => {
                VfsError::PermissionDenied(path.display().to_string())
            }
            _ => VfsError::IoError(e),
        })?;

        let is_symlink = sym_meta.file_type().is_symlink();
        let target_meta = if is_symlink {
            fs::metadata(path).unwrap_or(sym_meta)
        } else {
            sym_meta
        };
        let is_dir = target_meta.is_dir();

        let raw_name = path.file_name().and_then(|n| n.to_str());
        let name = raw_name.unwrap_or("/").to_string();
        let is_hidden = name.starts_with('.') && name.len() > 1;
        let (extension, kind) = infer_file_kind(&name, is_dir);

        let attrs = self.get_attrs(path).unwrap_or_default();

        let size = if is_dir {
            let cached = self
                .dir_size_cache
                .read()
                .ok()
                .and_then(|cache| cache.get(path).copied());
            if cached.is_some() {
                cached
            } else if cfg!(target_os = "macos") && extension.as_deref() == Some("app") {
                let total = shallow_dir_bundle_size(path, 3);
                if let Ok(mut guard) = self.dir_size_cache.write() {
                    guard.insert(path.to_path_buf(), total);
                }
                Some(total)
            } else {
                None
            }
        } else {
            Some(target_meta.len())
        };

        let modified = target_meta.modified().ok();
        let created = target_meta.created().ok().or(modified);
        let added = attrs.added.or(created).or(modified);
        #[cfg(unix)]
        let (permissions, owner_str, group_str) = {
            let perm_bits = target_meta.permissions().mode() & 0o7777;
            let uid = target_meta.uid();
            let gid = target_meta.gid();
            let owner = resolve_unix_uid_name(uid);
            let group = resolve_unix_gid_name(gid);
            (perm_bits, owner, group)
        };
        #[cfg(windows)]
        let (permissions, owner_str, group_str) = {
            let readonly = target_meta.permissions().readonly();
            let perm_bits = match (is_dir, readonly) {
                (true, true) => 0o555,
                (true, false) => 0o755,
                (false, true) => 0o444,
                (false, false) => 0o644,
            };
            let owner = std::env::var("USERNAME").unwrap_or_else(|_| "User".to_string());
            (perm_bits, owner, "Users".to_string())
        };

        Ok(FileMetadata {
            name,
            path: path.to_string_lossy().to_string(),
            extension,
            kind,
            size,
            is_dir,
            is_symlink,
            is_hidden,
            is_locked: attrs.is_locked,
            hide_extension: attrs.hide_extension,
            color_label: attrs.color_label,
            modified,
            created,
            added,
            permissions,
            owner: Some(owner_str),
            group: Some(group_str),
        })
    }
}

#[cfg(unix)]
fn resolve_unix_uid_name(uid: u32) -> String {
    use std::sync::OnceLock;
    static UID_CACHE: OnceLock<RwLock<HashMap<u32, String>>> = OnceLock::new();
    let cache = UID_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(guard) = cache.read() {
        if let Some(name) = guard.get(&uid) {
            return name.clone();
        }
    }
    let resolved = unsafe {
        let mut pwd = std::mem::zeroed::<libc::passwd>();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = [0 as libc::c_char; 1024];
        let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
        if rc == 0 && !result.is_null() && !pwd.pw_name.is_null() {
            std::ffi::CStr::from_ptr(pwd.pw_name)
                .to_string_lossy()
                .into_owned()
        } else if uid == 0 {
            "root".to_string()
        } else {
            uid.to_string()
        }
    };
    if let Ok(mut guard) = cache.write() {
        guard.insert(uid, resolved.clone());
    }
    resolved
}

#[cfg(unix)]
fn resolve_unix_gid_name(gid: u32) -> String {
    use std::sync::OnceLock;
    static GID_CACHE: OnceLock<RwLock<HashMap<u32, String>>> = OnceLock::new();
    let cache = GID_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(guard) = cache.read() {
        if let Some(name) = guard.get(&gid) {
            return name.clone();
        }
    }
    let resolved = unsafe {
        let mut grp = std::mem::zeroed::<libc::group>();
        let mut result: *mut libc::group = std::ptr::null_mut();
        let mut buf = [0 as libc::c_char; 1024];
        let rc = libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut result);
        if rc == 0 && !result.is_null() && !grp.gr_name.is_null() {
            std::ffi::CStr::from_ptr(grp.gr_name)
                .to_string_lossy()
                .into_owned()
        } else if gid == 0 {
            if cfg!(target_os = "macos") {
                "wheel".to_string()
            } else {
                "root".to_string()
            }
        } else {
            gid.to_string()
        }
    };
    if let Ok(mut guard) = cache.write() {
        guard.insert(gid, resolved.clone());
    }
    resolved
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn shallow_dir_bundle_size(dir: &Path, max_depth: usize) -> u64 {
    if max_depth == 0 {
        return 0;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in rd.flatten().take(128) {
        let p = entry.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            total = total.saturating_add(shallow_dir_bundle_size(&p, max_depth - 1));
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

impl FileSystemBackend for LocalFsBackend {
    fn scheme(&self) -> &str {
        "file"
    }

    fn list_directory(&self, path: &str) -> Result<Vec<FileMetadata>, VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        let read_dir = fs::read_dir(&resolved).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VfsError::NotFound(path.to_string()),
            std::io::ErrorKind::PermissionDenied => VfsError::PermissionDenied(path.to_string()),
            _ => VfsError::IoError(e),
        })?;

        let mut items = Vec::new();
        for entry in read_dir {
            let entry = entry?;
            if let Ok(meta) = self.build_metadata(&entry.path()) {
                items.push(meta);
            }
        }
        items.sort_by_key(|a| a.name.to_lowercase());
        Ok(items)
    }

    fn metadata(&self, path: &str) -> Result<FileMetadata, VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        self.build_metadata(&resolved)
    }

    #[allow(clippy::unnecessary_cast)]
    fn volume_stats(&self, path: &str) -> Result<VolumeStats, VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        #[cfg(unix)]
        {
            let lossy = resolved.to_string_lossy();
            let Ok(c_path) = CString::new(lossy.as_bytes()) else {
                return Err(VfsError::InvalidPath(path.to_string()));
            };

            // Query POSIX statvfs for accurate volume free space ("354,54 GB available")
            unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                    let block_size = if stat.f_frsize > 0 {
                        stat.f_frsize as u64
                    } else {
                        stat.f_bsize as u64
                    };
                    let total_bytes = (stat.f_blocks as u64).saturating_mul(block_size);
                    let available_bytes = (stat.f_bavail as u64).saturating_mul(block_size);
                    return Ok(VolumeStats {
                        total_bytes,
                        available_bytes,
                    });
                }
            }
        }
        #[cfg(windows)]
        {
            #[link(name = "kernel32")]
            extern "system" {
                fn GetDiskFreeSpaceExW(
                    lp_directory_name: *const u16,
                    lp_free_bytes_available: *mut u64,
                    lp_total_number_of_bytes: *mut u64,
                    lp_total_number_of_free_bytes: *mut u64,
                ) -> i32;
            }
            let mut wide: Vec<u16> = resolved.to_string_lossy().encode_utf16().collect();
            wide.push(0);
            let mut avail: u64 = 0;
            let mut total: u64 = 0;
            let mut free: u64 = 0;
            let ptr = wide.as_ptr();
            let ok = unsafe { GetDiskFreeSpaceExW(ptr, &mut avail, &mut total, &mut free) };
            if ok != 0 {
                return Ok(VolumeStats {
                    total_bytes: total,
                    available_bytes: avail,
                });
            }
        }

        Err(VfsError::NotFound(path.to_string()))
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        fs::create_dir_all(&resolved)?;
        Ok(())
    }

    fn remove(&self, path: &str, recursive: bool) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        if let Some(attrs) = self.get_attrs(&resolved) {
            if attrs.is_locked {
                let msg = format!("Item is locked: {}", resolved.display());
                return Err(VfsError::PermissionDenied(msg));
            }
        }
        let meta = fs::symlink_metadata(&resolved).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VfsError::NotFound(path.to_string()),
            _ => VfsError::IoError(e),
        })?;
        let is_real_dir = meta.is_dir() && !meta.file_type().is_symlink();
        if is_real_dir && recursive {
            fs::remove_dir_all(&resolved)?;
        } else if is_real_dir {
            fs::remove_dir(&resolved)?;
        } else {
            fs::remove_file(&resolved)?;
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let src = self.resolve_safe_path(from)?;
        let dst = self.resolve_safe_path(to)?;
        if let Some(attrs) = self.get_attrs(&src) {
            if attrs.is_locked {
                let msg = format!("Item is locked: {}", src.display());
                return Err(VfsError::PermissionDenied(msg));
            }
        }
        fs::rename(&src, &dst)?;
        if let Ok(mut map) = self.attrs_overlay.write() {
            let prev = map.remove(&src);
            if let Some(attrs) = prev {
                map.insert(dst, attrs);
            }
        }
        Ok(())
    }

    fn copy_file(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let src = self.resolve_safe_path(from)?;
        let dst = self.resolve_safe_path(to)?;
        fs::copy(&src, &dst)?;
        if let Ok(mut map) = self.attrs_overlay.write() {
            let copied = map.get(&src).cloned();
            if let Some(attrs) = copied {
                map.insert(dst, attrs);
            }
        }
        Ok(())
    }

    fn read_bytes(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        fs::read(&resolved).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VfsError::NotFound(path.to_string()),
            std::io::ErrorKind::PermissionDenied => VfsError::PermissionDenied(path.to_string()),
            _ => VfsError::IoError(e),
        })
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        if let Some(attrs) = self.get_attrs(&resolved) {
            if attrs.is_locked {
                let msg = format!("Item is locked: {}", resolved.display());
                return Err(VfsError::PermissionDenied(msg));
            }
        }
        if let Some(parent) = resolved.parent() {
            let needs_create = !parent.as_os_str().is_empty() && !parent.exists();
            if needs_create {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = fs::File::create(&resolved)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    }

    fn append_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        if let Some(attrs) = self.get_attrs(&resolved) {
            if attrs.is_locked {
                let msg = format!("Item is locked: {}", resolved.display());
                return Err(VfsError::PermissionDenied(msg));
            }
        }
        if let Some(parent) = resolved.parent() {
            let needs_create = !parent.as_os_str().is_empty() && !parent.exists();
            if needs_create {
                fs::create_dir_all(parent)?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)?;
        file.write_all(data)?;
        Ok(())
    }

    fn create_symlink(&self, target_path: &str, link_path: &str) -> Result<(), VfsError> {
        let resolved_target = self.resolve_safe_path(target_path)?;
        let resolved_link = self.resolve_safe_path(link_path)?;
        if resolved_link.exists() || fs::symlink_metadata(&resolved_link).is_ok() {
            let msg = format!("Alias already exists: {}", resolved_link.display());
            return Err(VfsError::AlreadyExists(msg));
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&resolved_target, &resolved_link)?;
        }
        #[cfg(windows)]
        {
            if resolved_target.is_dir() {
                std::os::windows::fs::symlink_dir(&resolved_target, &resolved_link)?;
            } else {
                std::os::windows::fs::symlink_file(&resolved_target, &resolved_link)?;
            }
        }
        Ok(())
    }

    fn set_permissions(&self, path: &str, mode: u32) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(mode & 0o7777);
            fs::set_permissions(&resolved, perms)?;
        }
        #[cfg(windows)]
        {
            let mut perms = fs::metadata(&resolved)?.permissions();
            perms.set_readonly((mode & 0o200) == 0);
            fs::set_permissions(&resolved, perms)?;
        }
        Ok(())
    }

    fn set_color_label(&self, path: &str, label: ColorLabel) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        if !resolved.exists() {
            return Err(VfsError::NotFound(path.to_string()));
        }
        if let Ok(mut map) = self.attrs_overlay.write() {
            map.entry(resolved).or_default().color_label = label;
        }
        Ok(())
    }

    fn set_hide_extension(&self, path: &str, hide: bool) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        if !resolved.exists() {
            return Err(VfsError::NotFound(path.to_string()));
        }
        if let Ok(mut map) = self.attrs_overlay.write() {
            map.entry(resolved).or_default().hide_extension = hide;
        }
        Ok(())
    }

    fn set_locked(&self, path: &str, locked: bool) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        if !resolved.exists() {
            return Err(VfsError::NotFound(path.to_string()));
        }
        if let Ok(mut map) = self.attrs_overlay.write() {
            map.entry(resolved).or_default().is_locked = locked;
        }
        Ok(())
    }

    fn calculate_dir_size(&self, path: &str) -> Result<u64, VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        let meta = self.build_metadata(&resolved)?;
        if !meta.is_dir {
            return Ok(meta.size.unwrap_or(0));
        }
        let mut total = 0u64;
        for entry in self.list_directory(path)? {
            if entry.is_dir && !entry.is_symlink {
                total = total.saturating_add(self.calculate_dir_size(&entry.path)?);
            } else {
                total = total.saturating_add(entry.size.unwrap_or(0));
            }
        }
        if let Ok(mut cache) = self.dir_size_cache.write() {
            cache.insert(resolved, total);
        }
        Ok(total)
    }
}

/// Supported archive formats treated as virtual folders (`Section 3.3` of `BRIEF.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    TarBz2,
    TarXz,
    SevenZ,
    Rar,
}

impl ArchiveFormat {
    /// Detects if a filename or path points to a supported virtual archive.
    pub fn detect(path: &str) -> Option<Self> {
        let lower = path.to_lowercase();
        if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
            Some(Self::TarBz2)
        } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
            Some(Self::TarXz)
        } else if lower.ends_with(".tar") {
            Some(Self::Tar)
        } else if lower.ends_with(".zip") {
            Some(Self::Zip)
        } else if lower.ends_with(".7z") {
            Some(Self::SevenZ)
        } else if lower.ends_with(".rar") {
            Some(Self::Rar)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
struct ArchiveEntryNode {
    metadata: FileMetadata,
    data: Vec<u8>,
}

/// Archive Virtualizer treating `.zip`, `.tar.*`, `.7z`, and `.rar` as instances of [`FileSystemBackend`].
/// Navigates the central directory index in memory and supports transparent extraction and on-the-fly packing.
pub struct ArchiveBackend {
    archive_path: String,
    format: ArchiveFormat,
    entries: Arc<RwLock<HashMap<String, ArchiveEntryNode>>>,
}

impl ArchiveBackend {
    pub fn new(archive_path: impl Into<String>, format: ArchiveFormat) -> Self {
        let archive_path = archive_path.into();
        let mut map = HashMap::new();
        map.insert(
            "/".to_string(),
            ArchiveEntryNode {
                metadata: FileMetadata {
                    name: "/".to_string(),
                    path: "/".to_string(),
                    extension: None,
                    kind: "Folder".to_string(),
                    size: Some(0),
                    is_dir: true,
                    is_symlink: false,
                    is_hidden: false,
                    is_locked: false,
                    hide_extension: false,
                    color_label: ColorLabel::None,
                    modified: Some(SystemTime::now()),
                    created: Some(SystemTime::now()),
                    added: Some(SystemTime::now()),
                    permissions: 0o755,
                    owner: Some("root".to_string()),
                    group: Some("root".to_string()),
                },
                data: Vec::new(),
            },
        );

        Self {
            archive_path,
            format,
            entries: Arc::new(RwLock::new(map)),
        }
    }

    pub fn archive_path(&self) -> &str {
        &self.archive_path
    }

    pub fn format(&self) -> ArchiveFormat {
        self.format
    }

    pub fn insert_sized_placeholder(&self, path: &str, size_bytes: u64) -> Result<(), VfsError> {
        self.write_bytes(path, b"JFIF")?;
        let norm = Self::normalize_virtual_path(path)?;
        let mut entries = self.entries.write().unwrap();
        if let Some(node) = entries.get_mut(&norm) {
            node.metadata.size = Some(size_bytes);
        }
        Ok(())
    }

    pub fn insert_metadata_only(
        &self,
        path: &str,
        size_bytes: u64,
        is_dir: bool,
    ) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        if norm == "/" {
            return Ok(());
        }
        if let Some((parent, _)) = norm.rsplit_once('/') {
            let parent_norm = if parent.is_empty() { "/" } else { parent };
            let has_parent = self.entries.read().unwrap().contains_key(parent_norm);
            if !has_parent {
                self.insert_metadata_only(parent_norm, 0, true)?;
            }
        }
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        let (extension, kind) = infer_file_kind(&name, is_dir);
        let mut entries = self.entries.write().unwrap();
        entries
            .entry(norm.clone())
            .or_insert_with(|| ArchiveEntryNode {
                metadata: FileMetadata {
                    name,
                    path: norm,
                    extension,
                    kind,
                    size: if is_dir { Some(0) } else { Some(size_bytes) },
                    is_dir,
                    is_symlink: false,
                    is_hidden: false,
                    is_locked: false,
                    hide_extension: false,
                    color_label: ColorLabel::None,
                    modified: Some(SystemTime::now()),
                    created: Some(SystemTime::now()),
                    added: Some(SystemTime::now()),
                    permissions: if is_dir { 0o755 } else { 0o644 },
                    owner: Some("archive".to_string()),
                    group: Some("archive".to_string()),
                },
                data: Vec::new(),
            });
        Ok(())
    }

    /// Opens a real `.zip`, `.tar`, `.tar.gz`, `.tar.bz2`, `.tar.xz`, or `.7z` archive from disk (`GAP-06`),
    /// validating all member paths against Zip-Slip and symlink escape (`SEC-03`).
    pub fn open_from_disk(archive_path: &str) -> Result<Self, VfsError> {
        let fmt = ArchiveFormat::detect(archive_path).ok_or_else(|| {
            VfsError::UnsupportedOperation(format!("Unsupported archive format: {archive_path}"))
        })?;
        let p = Path::new(archive_path);
        if !p.is_file() {
            return Err(VfsError::NotFound(archive_path.to_string()));
        }
        let backend = Self::new(archive_path, fmt);
        let raw_entries = inspect_archive_entries_on_disk(archive_path, fmt)?;
        for (member_path, size_bytes, is_dir) in raw_entries {
            backend.insert_metadata_only(&member_path, size_bytes, is_dir)?;
        }
        Ok(backend)
    }

    pub fn normalize_virtual_path(path: &str) -> Result<String, VfsError> {
        let unified = path.replace('\\', "/");
        let has_drive = unified.len() >= 2 && unified.as_bytes()[1] == b':';
        if unified.contains('\0') || has_drive || unified.split('/').any(|seg| seg == "..") {
            let msg = format!("Zip-slip / path traversal rejected in archive: {path}");
            return Err(VfsError::InvalidPath(msg));
        }
        let mut clean_segs = Vec::new();
        for seg in unified.trim().split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            clean_segs.push(seg);
        }
        if clean_segs.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(format!("/{}", clean_segs.join("/")))
        }
    }
}

/// Validates an archive on disk against Zip-Slip (`..`, leading `/`, drive letters, or external symlinks)
/// before extraction (`SEC-03`), returning `(virtual_path, size_bytes, is_dir)` for every entry.
pub fn inspect_archive_entries_on_disk(
    archive_path: &str,
    fmt: ArchiveFormat,
) -> Result<Vec<(String, u64, bool)>, VfsError> {
    let py_script = r#"
import os, sys, json, posixpath

path = os.environ["MANIFOLD_ARCHIVE_PATH"]
fmt = os.environ["MANIFOLD_ARCHIVE_FMT"]
out = []

def check_safe(name, linkname=""):
    u = name.replace("\\", "/")
    if "\0" in u or u.startswith("/") or (len(u) >= 2 and u[1] == ":"):
        raise ValueError("Unsafe absolute archive path: " + name)
    parts = [p for p in u.split("/") if p not in ("", ".")]
    if any(p == ".." for p in parts):
        raise ValueError("Zip-slip traversal in archive: " + name)
    norm = "/" + "/".join(parts) if parts else "/"
    if linkname:
        lu = linkname.replace("\\", "/")
        if lu.startswith("/") or (len(lu) >= 2 and lu[1] == ":"):
            raise ValueError("Unsafe absolute symlink in archive: " + linkname)
        parent = posixpath.dirname(norm.lstrip("/"))
        joined = posixpath.normpath(posixpath.join(parent, lu))
        if joined.startswith("..") or joined.startswith("/"):
            raise ValueError("Symlink escapes archive root: " + linkname)
    return norm

if fmt == "zip":
    import zipfile
    with zipfile.ZipFile(path, "r") as zf:
        for info in zf.infolist():
            is_dir = info.filename.endswith("/")
            is_sym = ((info.external_attr >> 16) & 0o170000) == 0o120000
            link_target = ""
            if is_sym and info.file_size < 4096:
                link_target = zf.read(info.filename).decode("utf-8", "replace")
            norm = check_safe(info.filename, link_target)
            if norm != "/":
                out.append([norm, int(info.file_size), bool(is_dir)])
else:
    import tarfile
    with tarfile.open(path, "r:*") as tf:
        for m in tf.getmembers():
            link_target = m.linkname if (m.issym() or m.islnk()) else ""
            norm = check_safe(m.name, link_target)
            if norm != "/":
                out.append([norm, int(m.size), bool(m.isdir())])

sys.stdout.write(json.dumps(out))
"#;
    if matches!(
        fmt,
        ArchiveFormat::Zip
            | ArchiveFormat::Tar
            | ArchiveFormat::TarGz
            | ArchiveFormat::TarBz2
            | ArchiveFormat::TarXz
    ) {
        let fmt_tag = if fmt == ArchiveFormat::Zip {
            "zip"
        } else {
            "tar"
        };
        let py_out = Command::new("python3")
            .args(["-c", py_script])
            .env("MANIFOLD_ARCHIVE_PATH", archive_path)
            .env("MANIFOLD_ARCHIVE_FMT", fmt_tag)
            .output();
        if let Ok(output) = py_out {
            if !output.status.success() {
                let err_txt = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(VfsError::InvalidPath(err_txt));
            }
            let stdout_str = String::from_utf8_lossy(&output.stdout);
            if let Ok(parsed) = parse_archive_json_listing(&stdout_str) {
                return Ok(parsed);
            }
        }
    }

    // Fallback CLI inspection for 7z/rar or systems without python3
    let safe_arg = safe_cli_path_arg(archive_path);
    let output = match fmt {
        ArchiveFormat::Zip => Command::new("zipinfo")
            .args(["-1", "--", &safe_arg])
            .output(),
        ArchiveFormat::Tar
        | ArchiveFormat::TarGz
        | ArchiveFormat::TarBz2
        | ArchiveFormat::TarXz => {
            let mut cmd = Command::new("tar");
            if cfg!(target_os = "linux") && safe_arg.contains(':') {
                cmd.arg("--force-local");
            }
            cmd.args(["-tf", &safe_arg]);
            cmd.output()
        }
        ArchiveFormat::SevenZ => Command::new("7z")
            .args(["l", "-ba", "--", &safe_arg])
            .output(),
        ArchiveFormat::Rar => {
            let mut cmd = Command::new("unrar");
            cmd.args(["lb", "--", &safe_arg]);
            cmd.output()
        }
    }?;
    if !output.status.success() {
        let err_txt = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(VfsError::InvalidPath(err_txt));
    }
    let mut list = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.starts_with('/') || (raw.len() >= 2 && raw.as_bytes()[1] == b':') {
            let msg = format!("Unsafe absolute path in archive: {raw}");
            return Err(VfsError::InvalidPath(msg));
        }
        let is_dir = raw.ends_with('/');
        let norm = ArchiveBackend::normalize_virtual_path(raw)?;
        if norm != "/" {
            list.push((norm, 0, is_dir));
        }
    }
    Ok(list)
}

fn parse_archive_json_listing(raw: &str) -> Result<Vec<(String, u64, bool)>, VfsError> {
    // Lightweight parser for `[["/path", 123, false], ...]` without extra crate deps
    let trimmed = raw.trim();
    if trimmed == "[]" {
        return Ok(Vec::new());
    }
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| VfsError::InvalidPath("Malformed archive JSON".to_string()))?;
    let mut entries = Vec::new();
    for item in inner.split("],") {
        let clean = item.trim().trim_start_matches('[').trim_end_matches(']');
        if clean.is_empty() {
            continue;
        }
        let first_quote = clean
            .find('"')
            .ok_or_else(|| VfsError::InvalidPath("Malformed archive JSON path".to_string()))?;
        let rest_after_first = &clean[first_quote + 1..];
        let mut path_buf = String::new();
        let mut escaped = false;
        let mut end_idx = 0;
        for (i, ch) in rest_after_first.char_indices() {
            if escaped {
                path_buf.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                end_idx = i;
                break;
            } else {
                path_buf.push(ch);
            }
        }
        let tail = rest_after_first[end_idx + 1..]
            .trim()
            .trim_start_matches(',');
        let mut parts = tail.split(',');
        let size_str = parts.next().unwrap_or("0").trim();
        let dir_str = parts.next().unwrap_or("false").trim();
        let size_bytes = size_str.parse::<u64>().unwrap_or(0);
        let is_dir = dir_str == "true";
        let norm = ArchiveBackend::normalize_virtual_path(&path_buf)?;
        entries.push((norm, size_bytes, is_dir));
    }
    Ok(entries)
}

fn extract_single_archive_member(
    archive_path: &str,
    fmt: ArchiveFormat,
    virtual_path: &str,
) -> Result<Vec<u8>, VfsError> {
    let rel_member = virtual_path.trim_start_matches('/');
    let py_script = r#"
import os, sys
path = os.environ["MANIFOLD_ARCHIVE_PATH"]
fmt = os.environ["MANIFOLD_ARCHIVE_FMT"]
member = os.environ["MANIFOLD_ARCHIVE_MEMBER"]
if fmt == "zip":
    import zipfile
    with zipfile.ZipFile(path, "r") as zf:
        for info in zf.infolist():
            clean = info.filename.replace("\\", "/").strip("/")
            if clean == member:
                sys.stdout.buffer.write(zf.read(info.filename))
                sys.exit(0)
else:
    import tarfile
    with tarfile.open(path, "r:*") as tf:
        for m in tf.getmembers():
            clean = m.name.replace("\\", "/").lstrip("./").strip("/")
            if clean == member and m.isfile():
                f = tf.extractfile(m)
                if f:
                    sys.stdout.buffer.write(f.read())
                    sys.exit(0)
sys.exit(2)
"#;
    if matches!(
        fmt,
        ArchiveFormat::Zip
            | ArchiveFormat::Tar
            | ArchiveFormat::TarGz
            | ArchiveFormat::TarBz2
            | ArchiveFormat::TarXz
    ) {
        let fmt_tag = if fmt == ArchiveFormat::Zip {
            "zip"
        } else {
            "tar"
        };
        let out = Command::new("python3")
            .args(["-c", py_script])
            .env("MANIFOLD_ARCHIVE_PATH", archive_path)
            .env("MANIFOLD_ARCHIVE_FMT", fmt_tag)
            .env("MANIFOLD_ARCHIVE_MEMBER", rel_member)
            .output();
        if let Ok(res) = out {
            if res.status.success() {
                return Ok(res.stdout);
            }
        }
    }
    Err(VfsError::NotFound(virtual_path.to_string()))
}

impl FileSystemBackend for ArchiveBackend {
    fn scheme(&self) -> &str {
        "archive"
    }

    fn list_directory(&self, path: &str) -> Result<Vec<FileMetadata>, VfsError> {
        let dir_path = Self::normalize_virtual_path(path)?;
        let entries = self.entries.read().unwrap();
        if !entries.contains_key(&dir_path) {
            return Err(VfsError::NotFound(dir_path));
        }
        let prefix = if dir_path == "/" {
            "/".to_string()
        } else {
            format!("{dir_path}/")
        };

        let mut results = Vec::new();
        for (k, node) in entries.iter() {
            if k == &dir_path {
                continue;
            }
            if let Some(rest) = k.strip_prefix(&prefix) {
                let is_direct_child = !rest.is_empty() && !rest.contains('/');
                if is_direct_child {
                    results.push(node.metadata.clone());
                }
            }
        }
        results.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(results)
    }

    fn metadata(&self, path: &str) -> Result<FileMetadata, VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let entries = self.entries.read().unwrap();
        let found = entries.get(&norm).map(|n| n.metadata.clone());
        found.ok_or(VfsError::NotFound(norm))
    }

    fn volume_stats(&self, _path: &str) -> Result<VolumeStats, VfsError> {
        let entries = self.entries.read().unwrap();
        let used: u64 = entries.values().filter_map(|e| e.metadata.size).sum();
        Ok(VolumeStats {
            total_bytes: used,
            available_bytes: 0,
        })
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let mut accum = String::new();
        let mut entries = self.entries.write().unwrap();
        for seg in norm.trim_matches('/').split('/').filter(|s| !s.is_empty()) {
            accum.push('/');
            accum.push_str(seg);
            let cur = accum.clone();
            entries
                .entry(cur.clone())
                .or_insert_with(|| ArchiveEntryNode {
                    metadata: FileMetadata {
                        name: seg.to_string(),
                        path: cur,
                        extension: None,
                        kind: "Folder".to_string(),
                        size: Some(0),
                        is_dir: true,
                        is_symlink: false,
                        is_hidden: seg.starts_with('.'),
                        is_locked: false,
                        hide_extension: false,
                        color_label: ColorLabel::None,
                        modified: Some(SystemTime::now()),
                        created: Some(SystemTime::now()),
                        added: Some(SystemTime::now()),
                        permissions: 0o755,
                        owner: Some("archive".to_string()),
                        group: Some("archive".to_string()),
                    },
                    data: Vec::new(),
                });
        }
        Ok(())
    }

    fn remove(&self, path: &str, recursive: bool) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let mut entries = self.entries.write().unwrap();
        if !entries.contains_key(&norm) {
            return Err(VfsError::NotFound(norm));
        }
        if recursive {
            let prefix = format!("{norm}/");
            entries.retain(|k, _| k != &norm && !k.starts_with(&prefix));
        } else {
            entries.remove(&norm);
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let src = Self::normalize_virtual_path(from)?;
        let dst = Self::normalize_virtual_path(to)?;
        let mut entries = self.entries.write().unwrap();
        let mut node = entries.remove(&src).ok_or(VfsError::NotFound(src))?;
        node.metadata.name = dst.rsplit('/').next().unwrap_or(&dst).to_string();
        node.metadata.path = dst.clone();
        entries.insert(dst, node);
        Ok(())
    }

    fn copy_file(&self, from: &str, to: &str) -> Result<(), VfsError> {
        let src = Self::normalize_virtual_path(from)?;
        let dst = Self::normalize_virtual_path(to)?;
        let mut entries = self.entries.write().unwrap();
        let mut node = entries.get(&src).cloned().ok_or(VfsError::NotFound(src))?;
        node.metadata.name = dst.rsplit('/').next().unwrap_or(&dst).to_string();
        node.metadata.path = dst.clone();
        entries.insert(dst, node);
        Ok(())
    }

    fn read_bytes(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        {
            let entries = self.entries.read().unwrap();
            let node = entries
                .get(&norm)
                .ok_or_else(|| VfsError::NotFound(norm.clone()))?;
            if !node.data.is_empty() || node.metadata.is_dir || node.metadata.size == Some(0) {
                return Ok(node.data.clone());
            }
        }
        let bytes = extract_single_archive_member(&self.archive_path, self.format, &norm)?;
        if bytes.len() <= 16 * 1024 * 1024 {
            let mut entries = self.entries.write().unwrap();
            if let Some(node) = entries.get_mut(&norm) {
                node.data = bytes.clone();
            }
        }
        Ok(bytes)
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        // Ensure parent directories exist in the virtual index
        if let Some((parent, _)) = norm.rsplit_once('/') {
            let parent_norm = if parent.is_empty() { "/" } else { parent };
            if !self.entries.read().unwrap().contains_key(parent_norm) {
                self.create_dir(parent_norm)?;
            }
        }
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        let (extension, kind) = infer_file_kind(&name, false);
        let mut entries = self.entries.write().unwrap();
        entries.insert(
            norm.clone(),
            ArchiveEntryNode {
                metadata: FileMetadata {
                    name,
                    path: norm,
                    extension,
                    kind,
                    size: Some(data.len() as u64),
                    is_dir: false,
                    is_symlink: false,
                    is_hidden: false,
                    is_locked: false,
                    hide_extension: false,
                    color_label: ColorLabel::None,
                    modified: Some(SystemTime::now()),
                    created: Some(SystemTime::now()),
                    added: Some(SystemTime::now()),
                    permissions: 0o644,
                    owner: Some("archive".to_string()),
                    group: Some("archive".to_string()),
                },
                data: data.to_vec(),
            },
        );
        Ok(())
    }

    fn set_permissions(&self, path: &str, mode: u32) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let mut entries = self.entries.write().unwrap();
        let node = entries.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.permissions = mode & 0o7777;
        Ok(())
    }

    fn set_color_label(&self, path: &str, label: ColorLabel) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let mut entries = self.entries.write().unwrap();
        let node = entries.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.color_label = label;
        Ok(())
    }

    fn set_hide_extension(&self, path: &str, hide: bool) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let mut entries = self.entries.write().unwrap();
        let node = entries.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.hide_extension = hide;
        Ok(())
    }

    fn set_locked(&self, path: &str, locked: bool) -> Result<(), VfsError> {
        let norm = Self::normalize_virtual_path(path)?;
        let mut entries = self.entries.write().unwrap();
        let node = entries.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.is_locked = locked;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permissions_matrix_roundtrip_and_image2_644() {
        // Verify exact match with doc/image2.png: Owner R+W, Group R, Others R -> rw-r--r-- (644)
        let matrix = PermissionsMatrix::from_octal_str("644").unwrap();
        assert!(matrix.owner.read);
        assert!(matrix.owner.write);
        assert!(!matrix.owner.execute);
        assert!(matrix.group.read);
        assert!(!matrix.group.write);
        assert!(!matrix.group.execute);
        assert!(matrix.others.read);
        assert!(!matrix.others.write);
        assert!(!matrix.others.execute);
        assert_eq!(matrix.to_symbolic_string(), "rw-r--r--");
        assert_eq!(matrix.to_octal_string(), "644");
        assert_eq!(matrix.to_mode(), 0o644);

        // Property test across all 512 permission bit combinations (0o000..=0o777)
        for mode in 0..=0o777u32 {
            let m = PermissionsMatrix::from_mode(mode);
            let oct = m.to_octal_string();
            let parsed = PermissionsMatrix::from_octal_str(&oct).unwrap();
            assert_eq!(parsed.to_mode(), mode);
        }
    }

    #[test]
    fn test_local_fs_property_based_concurrent_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let vfs = Arc::new(LocalFsBackend::with_sandbox_root(tmp.path()).unwrap());

        // Reject sandbox traversal attempts
        assert!(vfs.write_bytes("../escape.txt", b"bad").is_err());

        // Spawn 8 concurrent worker threads performing randomized writes, reads, permissions & color label updates
        std::thread::scope(|s| {
            for worker_id in 0..8 {
                let vfs = Arc::clone(&vfs);
                s.spawn(move || {
                    let mut rng = fastrand::Rng::with_seed(worker_id as u64 + 100);
                    let dir_name = format!("worker_{worker_id}");
                    vfs.create_dir(&dir_name).unwrap();

                    for i in 0..25 {
                        let file_path = format!("{dir_name}/file_{i}.php");
                        let payload_len = rng.usize(16..512);
                        let payload: Vec<u8> = (0..payload_len).map(|_| rng.u8(..)).collect();
                        vfs.write_bytes(&file_path, &payload).unwrap();

                        let read_back = vfs.read_bytes(&file_path).unwrap();
                        assert_eq!(read_back, payload);

                        let swatch = ColorLabel::all_swatches()[rng.usize(..8)];
                        vfs.set_color_label(&file_path, swatch).unwrap();

                        let meta = vfs.metadata(&file_path).unwrap();
                        assert_eq!(meta.color_label, swatch);
                        assert_eq!(meta.size, Some(payload_len as u64));
                        assert_eq!(meta.kind, "PHP Source");
                    }
                });
            }
        });

        let root_items = vfs.list_directory(".").unwrap();
        assert_eq!(root_items.len(), 8);
        for item in &root_items {
            assert!(item.is_dir);
            // Before calculation, directory size is None ("--")
            assert_eq!(item.formatted_size(), "--");
            let calculated = vfs.calculate_dir_size(&item.path).unwrap();
            assert!(calculated > 0);
            let updated_meta = vfs.metadata(&item.path).unwrap();
            assert_eq!(updated_meta.size, Some(calculated));
        }

        let stats = vfs.volume_stats(".").unwrap();
        assert!(stats.total_bytes > 0);
        let avail_str = stats.format_available_gb();
        assert!(avail_str.ends_with("GB") || avail_str.ends_with("MB"));
    }

    #[test]
    fn test_archive_virtualizer_navigation_and_zip_slip_guard() {
        let archive = ArchiveBackend::new("/tmp/backup.tar.gz", ArchiveFormat::TarGz);
        archive.write_bytes("/admin/index.php", b"<?php").unwrap();
        archive.write_bytes("/admin/settings.php", b"1").unwrap();

        let root_list = archive.list_directory("/").unwrap();
        assert_eq!(root_list.len(), 1);
        assert_eq!(root_list[0].name, "admin");
        assert!(root_list[0].is_dir);

        let admin_list = archive.list_directory("/admin").unwrap();
        assert_eq!(admin_list.len(), 2);
        assert_eq!(admin_list[0].name, "index.php");

        // Verify zip-slip / traversal rejection while allowing filenames with '..' (UI-06)
        assert!(archive.write_bytes("../../etc/passwd", b"x").is_err());
        assert!(archive.write_bytes("/admin/v1..v2.patch", b"diff").is_ok());

        // Verify special bits preservation (UI-10)
        let suid_mode = 0o4755u32;
        let updated = PermissionsMatrix::from_octal_str("750")
            .unwrap()
            .with_preserved_special_bits(suid_mode);
        assert_eq!(updated.to_mode(), 0o4750);

        // Verify secure runtime subdir and 0600 temp file creation (SEC-07)
        let (tmp_path, _file) = create_secure_temp_file("unit_test", "probe", ".tmp").unwrap();
        assert!(tmp_path.exists());
        #[cfg(unix)]
        {
            let tmp_meta = fs::metadata(&tmp_path).unwrap();
            let m = tmp_meta.permissions().mode() & 0o777;
            assert_eq!(m, 0o600);
        }
        let _ = fs::remove_file(&tmp_path);
    }

    #[cfg(unix)]
    #[test]
    fn test_local_fs_symlink_sandbox_escape_rejected() {
        let outer = tempfile::tempdir().unwrap();
        let outside_file = outer.path().join("outside_secret.txt");
        fs::write(&outside_file, b"secret").unwrap();

        let sandbox_dir = outer.path().join("sandbox");
        fs::create_dir_all(&sandbox_dir).unwrap();
        let vfs = LocalFsBackend::with_sandbox_root(&sandbox_dir).unwrap();

        let evil_link = sandbox_dir.join("escape_link");
        std::os::unix::fs::symlink(&outside_file, &evil_link).unwrap();
        assert!(vfs.read_bytes("escape_link").is_err());
    }
}
