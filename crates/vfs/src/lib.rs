//! Unified Virtual File System (`manifold-vfs`) for Project Manifold.
//!
//! Provides the core [`FileSystemBackend`] trait, [`FileMetadata`], [`PermissionsMatrix`],
//! [`ColorLabel`], [`VolumeStats`], [`LocalFsBackend`], and [`ArchiveBackend`].

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
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
pub fn system_time_to_civil(time: SystemTime) -> (i32, u32, u32, u32, u32) {
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

/// Classifies a human-readable `Kind` label from file extension and directory flag.
pub fn infer_file_kind(name: &str, is_dir: bool) -> (Option<String>, String) {
    let raw_ext = Path::new(name).extension().and_then(|e| e.to_str());
    let ext = raw_ext.map(str::to_lowercase);
    if is_dir {
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
            Ok(normalized)
        } else {
            Ok(normalize_lexical(input_path))
        }
    }

    fn get_attrs(&self, path: &Path) -> Option<ExtendedItemAttributes> {
        let guard = self.attrs_overlay.read().ok()?;
        guard.get(path).cloned()
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
            let guard = self.dir_size_cache.read().ok();
            guard.and_then(|cache| cache.get(path).copied())
        } else {
            Some(target_meta.len())
        };

        let modified = target_meta.modified().ok();
        let created = target_meta.created().ok().or(modified);
        let added = attrs.added.or(created).or(modified);
        let permissions = target_meta.permissions().mode() & 0o7777;

        let uid = target_meta.uid();
        let gid = target_meta.gid();
        let owner_str = if uid == 0 {
            "root".to_string()
        } else {
            std::env::var("USER").unwrap_or_else(|_| uid.to_string())
        };
        let group_str = match gid {
            0 => "wheel".to_string(),
            20 => "staff".to_string(),
            _ => gid.to_string(),
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

    fn set_permissions(&self, path: &str, mode: u32) -> Result<(), VfsError> {
        let resolved = self.resolve_safe_path(path)?;
        let perms = fs::Permissions::from_mode(mode & 0o7777);
        fs::set_permissions(&resolved, perms)?;
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

    fn normalize_virtual_path(path: &str) -> Result<String, VfsError> {
        if path.contains("..") || path.contains('\0') {
            let msg = format!("Zip-slip / path traversal rejected in archive: {path}");
            return Err(VfsError::InvalidPath(msg));
        }
        let trimmed = path.trim().trim_matches('/');
        if trimmed.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(format!("/{trimmed}"))
        }
    }
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
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        let mut entries = self.entries.write().unwrap();
        entries.insert(
            norm.clone(),
            ArchiveEntryNode {
                metadata: FileMetadata {
                    name,
                    path: norm,
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
                    owner: Some("archive".to_string()),
                    group: Some("archive".to_string()),
                },
                data: Vec::new(),
            },
        );
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
        let entries = self.entries.read().unwrap();
        let found = entries.get(&norm).map(|n| n.data.clone());
        found.ok_or(VfsError::NotFound(norm))
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

        // Verify zip-slip / traversal rejection
        assert!(archive.write_bytes("../../etc/passwd", b"x").is_err());
    }
}
