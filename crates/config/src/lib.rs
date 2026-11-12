//! Configuration serialization (`connections.toml`, `session.toml`) and OS Keyring
//! credential storage (`service = "manifold"`) for Project Manifold.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};

use manifold_vfs::ColorLabel;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Canonical keyring service name for Manifold credentials.
pub const KEYRING_SERVICE_NAME: &str = "manifold";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("TOML deserialization error: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("TOML serialization error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Keyring error: {0}")]
    Keyring(String),

    #[error("Invalid connection profile: {0}")]
    Validation(String),
}

/// Supported remote & cloud protocols (`doc/image3.png` and `Section 3.2` of `BRIEF.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    #[default]
    Sftp,
    Ftp,
    Ftps,
    S3,
    #[serde(rename = "gdrive")]
    GoogleDrive,
    Gcs,
    #[serde(rename = "azure")]
    AzureBlob,
    WebDav,
    Smb,
}

impl ProtocolType {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Sftp => "SFTP",
            Self::Ftp => "FTP",
            Self::Ftps => "FTPS",
            Self::S3 => "Amazon S3",
            Self::GoogleDrive => "Google Drive",
            Self::Gcs => "Google Cloud Storage",
            Self::AzureBlob => "Azure Blob",
            Self::WebDav => "WebDAV",
            Self::Smb => "SMB",
        }
    }

    pub const fn default_port(self) -> u16 {
        match self {
            Self::Sftp => 22,
            Self::Ftp => 21,
            Self::Ftps => 990,
            Self::S3 | Self::GoogleDrive | Self::Gcs | Self::AzureBlob | Self::WebDav => 443,
            Self::Smb => 445,
        }
    }
}

/// Authentication mechanism for a connection profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    #[default]
    Password,
    PublicKey,
    AccessKey,
    Agent,
    Interactive,
}

/// Connection profile schema (`connections.toml`) matching both `Section 4.1` of `BRIEF.md`
/// and every row of the SFTP Connect Dialog in `doc/image3.png`.
///
/// Security Invariant: Secrets and passphrases are NEVER serialized to TOML; they are
/// persisted exclusively via [`CredentialStore`] under `service = "manifold"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    pub protocol: ProtocolType,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub auth_type: AuthType,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default)]
    pub base_path: String,
    /// Associated local directory (`Local Path` row in `doc/image3.png`).
    #[serde(default)]
    pub local_path: Option<String>,
    /// Public HTTP/HTTPS mapping (`Base URL` row in `doc/image3.png`, e.g. `"https://johnappleseed.com/"`).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Character encoding (`Text Encoding` row in `doc/image3.png`, e.g. `"Unicode (UTF-8)"`).
    #[serde(default = "default_text_encoding")]
    pub text_encoding: String,
    /// User note (`Note` row in `doc/image3.png`).
    #[serde(default)]
    pub note: String,
    /// 8-color swatch tag (`Color Label` row in `doc/image3.png`).
    #[serde(default)]
    pub color_label: ColorLabel,
    /// Whether the advanced section is expanded (`Show Advanced` toggle in `doc/image3.png`).
    #[serde(default = "default_true")]
    pub show_advanced: bool,
}

fn default_text_encoding() -> String {
    "Unicode (UTF-8)".to_string()
}

const fn default_true() -> bool {
    true
}

impl ConnectionProfile {
    /// Creates a pre-populated SFTP profile matching the defaults shown in `doc/image3.png`.
    pub fn new_sftp_default(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            group: Some("Favorites".to_string()),
            protocol: ProtocolType::Sftp,
            host: Some("johnappleseed.com".to_string()),
            port: Some(22),
            endpoint: None,
            bucket: None,
            region: None,
            username: "admin".to_string(),
            auth_type: AuthType::Password,
            key_path: None,
            base_path: "/User/Shared/Website".to_string(),
            local_path: None,
            base_url: Some("https://johnappleseed.com/".to_string()),
            text_encoding: default_text_encoding(),
            note: String::new(),
            color_label: ColorLabel::Purple,
            show_advanced: true,
        }
    }

    /// Resolves a remote file path into a clickable HTTP/HTTPS URL using `base_url` (`doc/image3.png`).
    pub fn web_url_for_remote_path(&self, remote_path: &str) -> Option<String> {
        let base = self.base_url.as_ref()?.trim_end_matches('/');
        let rel = remote_path
            .strip_prefix(&self.base_path)
            .unwrap_or(remote_path)
            .trim_start_matches('/');
        if rel.is_empty() {
            Some(format!("{base}/"))
        } else {
            Some(format!("{base}/{rel}"))
        }
    }

    /// Validates that port, host, username, and path fields contain no control characters
    /// or option-injection prefixes (`SEC-01`, `SEC-11`).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.port == Some(0) {
            return Err(ConfigError::Validation(
                "Port must be between 1 and 65535".to_string(),
            ));
        }
        let has_ctrl = |s: &str| s.chars().any(|c| c.is_control());
        for (label, val) in [
            ("id", self.id.as_str()),
            ("name", self.name.as_str()),
            ("username", self.username.as_str()),
            ("base_path", self.base_path.as_str()),
        ] {
            if has_ctrl(val) {
                return Err(ConfigError::Validation(format!(
                    "{label} contains control characters"
                )));
            }
        }
        if let Some(host) = &self.host {
            let trimmed = host.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('-')
                || host.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(ConfigError::Validation(
                    "Server host cannot be empty, start with '-', or contain whitespace"
                        .to_string(),
                ));
            }
        }
        if self.username.starts_with('-') || self.username.chars().any(char::is_whitespace) {
            return Err(ConfigError::Validation(
                "Username cannot start with '-' or contain whitespace".to_string(),
            ));
        }
        for (label, opt_val) in [
            ("endpoint", self.endpoint.as_deref()),
            ("bucket", self.bucket.as_deref()),
            ("region", self.region.as_deref()),
            ("key_path", self.key_path.as_deref()),
        ] {
            if let Some(v) = opt_val {
                if has_ctrl(v) || v.starts_with('-') {
                    return Err(ConfigError::Validation(format!(
                        "{label} cannot start with '-' or contain control characters"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Top-level `connections.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionsConfig {
    #[serde(default)]
    pub connections: Vec<ConnectionProfile>,
}

impl ConnectionsConfig {
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(input)?)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path)?;
        Self::from_toml_str(&raw)
    }

    pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let serialized = self.to_toml_string()?;
        write_secure_config_file(path.as_ref(), &serialized)
    }

    pub fn default_config_path() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("manifold").join("connections.toml");
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata)
                    .join("Manifold")
                    .join("connections.toml");
            }
        }
        let home_var = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
        if let Ok(home) = home_var {
            if !home.trim().is_empty() {
                return PathBuf::from(home)
                    .join(".config")
                    .join("manifold")
                    .join("connections.toml");
            }
        }
        std::env::temp_dir()
            .join("manifold")
            .join("connections.toml")
    }

    pub fn load_default() -> Self {
        let path = Self::default_config_path();
        Self::load_from_file(path).unwrap_or_default()
    }

    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = Self::default_config_path();
        self.save_to_file(path)
    }

    pub fn upsert_profile(&mut self, profile: ConnectionProfile) {
        if let Some(existing) = self.connections.iter_mut().find(|c| c.id == profile.id) {
            *existing = profile;
        } else {
            self.connections.push(profile);
        }
    }

    pub fn remove_profile(&mut self, id: &str) -> bool {
        let prev_len = self.connections.len();
        self.connections.retain(|c| c.id != id);
        self.connections.len() != prev_len
    }
}

/// Escapes a token for macOS `security -i` interactive stdin commands (`SEC-06`),
/// ensuring secrets are passed via piped `stdin` rather than process `argv`.
pub fn escape_macos_security_token(val: &str) -> String {
    let mut out = String::with_capacity(val.len() + 2);
    out.push('"');
    for ch in val.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' | '\n' | '\0' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(windows)]
mod win_cred {
    use std::ptr;

    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    #[repr(C)]
    #[allow(dead_code)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct CredentialW {
        flags: u32,
        cred_type: u32,
        target_name: *mut u16,
        comment: *mut u16,
        last_written: FileTime,
        credential_blob_size: u32,
        credential_blob: *mut u8,
        persist: u32,
        attribute_count: u32,
        attributes: *mut core::ffi::c_void,
        target_alias: *mut u16,
        user_name: *mut u16,
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
        fn CredReadW(
            target_name: *const u16,
            cred_type: u32,
            flags: u32,
            credential: *mut *mut CredentialW,
        ) -> i32;
        fn CredDeleteW(target_name: *const u16, cred_type: u32, flags: u32) -> i32;
        fn CredFree(buffer: *mut core::ffi::c_void);
    }

    fn to_wide_nul(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn write_generic(target: &str, user: &str, secret: &str) -> bool {
        let mut target_w = to_wide_nul(target);
        let mut user_w = to_wide_nul(user);
        let blob_u16: Vec<u16> = secret.encode_utf16().collect();
        let blob_len = (blob_u16.len() * 2) as u32;
        let cred = CredentialW {
            flags: 0,
            cred_type: CRED_TYPE_GENERIC,
            target_name: target_w.as_mut_ptr(),
            comment: ptr::null_mut(),
            last_written: FileTime {
                dw_low_date_time: 0,
                dw_high_date_time: 0,
            },
            credential_blob_size: blob_len,
            credential_blob: blob_u16.as_ptr() as *mut u8,
            persist: CRED_PERSIST_LOCAL_MACHINE,
            attribute_count: 0,
            attributes: ptr::null_mut(),
            target_alias: ptr::null_mut(),
            user_name: user_w.as_mut_ptr(),
        };
        unsafe { CredWriteW(&cred, 0) != 0 }
    }

    pub fn read_generic(target: &str) -> Option<String> {
        let target_w = to_wide_nul(target);
        let mut pcred: *mut CredentialW = ptr::null_mut();
        let ok = unsafe { CredReadW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0, &mut pcred) };
        if ok == 0 || pcred.is_null() {
            return None;
        }
        let result = unsafe {
            let cred = &*pcred;
            if cred.credential_blob.is_null() || cred.credential_blob_size == 0 {
                None
            } else if cred.credential_blob_size.is_multiple_of(2) {
                let u16_len = (cred.credential_blob_size / 2) as usize;
                let slice = std::slice::from_raw_parts(cred.credential_blob as *const u16, u16_len);
                Some(String::from_utf16_lossy(slice))
            } else {
                let slice = std::slice::from_raw_parts(
                    cred.credential_blob,
                    cred.credential_blob_size as usize,
                );
                Some(String::from_utf8_lossy(slice).into_owned())
            }
        };
        unsafe {
            CredFree(pcred as *mut core::ffi::c_void);
        }
        result.filter(|s| !s.is_empty())
    }

    pub fn delete_generic(target: &str) -> bool {
        let target_w = to_wide_nul(target);
        unsafe { CredDeleteW(target_w.as_ptr(), CRED_TYPE_GENERIC, 0) != 0 }
    }
}

/// Cross-platform OS keyring credential manager using `service = "manifold"`.
/// Uses native OS secret storage (`secret-tool` on Linux, `security -i` on macOS,
/// `Advapi32` `CredWriteW`/`CredReadW` on Windows) with a thread-safe in-memory vault fallback.
#[derive(Clone, Default)]
pub struct CredentialStore {
    memory_vault: Arc<RwLock<HashMap<String, String>>>,
    use_os_backend: bool,
}

impl CredentialStore {
    pub fn new() -> Self {
        Self {
            memory_vault: Arc::new(RwLock::new(HashMap::new())),
            use_os_backend: false,
        }
    }

    pub fn with_os_backend() -> Self {
        Self {
            memory_vault: Arc::new(RwLock::new(HashMap::new())),
            use_os_backend: true,
        }
    }

    pub fn service_name(&self) -> &'static str {
        KEYRING_SERVICE_NAME
    }

    fn lock_err<T>(_: T) -> ConfigError {
        ConfigError::Keyring("Keyring lock poisoned".to_string())
    }

    pub fn set_secret(&self, connection_id: &str, secret: &str) -> Result<(), ConfigError> {
        if self.use_os_backend {
            #[cfg(windows)]
            {
                let target = format!("{KEYRING_SERVICE_NAME}:{connection_id}");
                let _ = win_cred::write_generic(&target, KEYRING_SERVICE_NAME, secret);
            }
            #[cfg(not(windows))]
            if cfg!(target_os = "macos") {
                let child_res = Command::new("security")
                    .arg("-i")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                if let Ok(mut child) = child_res {
                    if let Some(mut stdin) = child.stdin.take() {
                        let svc = escape_macos_security_token(KEYRING_SERVICE_NAME);
                        let acct = escape_macos_security_token(connection_id);
                        let pw = escape_macos_security_token(secret);
                        let cmd = format!("add-generic-password -U -s {svc} -a {acct} -w {pw}\n");
                        let _ = stdin.write_all(cmd.as_bytes());
                    }
                    let _ = child.wait();
                }
            } else {
                let child_res = Command::new("secret-tool")
                    .args([
                        "store",
                        "--label=Manifold Credential",
                        "service",
                        KEYRING_SERVICE_NAME,
                        "account",
                        connection_id,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                if let Ok(mut child) = child_res {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(secret.as_bytes());
                    }
                    let _ = child.wait();
                }
            }
        }
        let mut guard = self.memory_vault.write().map_err(Self::lock_err)?;
        guard.insert(connection_id.to_string(), secret.to_string());
        Ok(())
    }

    pub fn get_secret(&self, connection_id: &str) -> Result<Option<String>, ConfigError> {
        {
            let guard = self.memory_vault.read().map_err(Self::lock_err)?;
            if let Some(val) = guard.get(connection_id) {
                return Ok(Some(val.clone()));
            }
        }
        if self.use_os_backend {
            #[cfg(windows)]
            {
                let target = format!("{KEYRING_SERVICE_NAME}:{connection_id}");
                if let Some(s) = win_cred::read_generic(&target) {
                    let mut guard = self.memory_vault.write().map_err(Self::lock_err)?;
                    guard.insert(connection_id.to_string(), s.clone());
                    return Ok(Some(s));
                }
            }
            #[cfg(not(windows))]
            {
                let output = if cfg!(target_os = "macos") {
                    Command::new("security")
                        .args([
                            "find-generic-password",
                            "-s",
                            KEYRING_SERVICE_NAME,
                            "-a",
                            connection_id,
                            "-w",
                        ])
                        .output()
                } else {
                    Command::new("secret-tool")
                        .args([
                            "lookup",
                            "service",
                            KEYRING_SERVICE_NAME,
                            "account",
                            connection_id,
                        ])
                        .output()
                };
                if let Ok(out) = output {
                    if out.status.success() {
                        let s = String::from_utf8_lossy(&out.stdout)
                            .trim_end_matches(&['\r', '\n'][..])
                            .to_string();
                        if !s.is_empty() {
                            let mut guard = self.memory_vault.write().map_err(Self::lock_err)?;
                            guard.insert(connection_id.to_string(), s.clone());
                            return Ok(Some(s));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn delete_secret(&self, connection_id: &str) -> Result<(), ConfigError> {
        if self.use_os_backend {
            #[cfg(windows)]
            {
                let target = format!("{KEYRING_SERVICE_NAME}:{connection_id}");
                let _ = win_cred::delete_generic(&target);
            }
            #[cfg(not(windows))]
            if cfg!(target_os = "macos") {
                let _ = Command::new("security")
                    .args([
                        "delete-generic-password",
                        "-s",
                        KEYRING_SERVICE_NAME,
                        "-a",
                        connection_id,
                    ])
                    .output();
            } else {
                let _ = Command::new("secret-tool")
                    .args([
                        "clear",
                        "service",
                        KEYRING_SERVICE_NAME,
                        "account",
                        connection_id,
                    ])
                    .output();
            }
        }
        let mut guard = self.memory_vault.write().map_err(Self::lock_err)?;
        guard.remove(connection_id);
        Ok(())
    }
}

/// View mode per tab (`Icon View`, `List View`, `Column View`, `Tree View`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PaneViewMode {
    Icons,
    #[default]
    List,
    Columns,
    Tree,
}

/// Sort column per tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortColumn {
    #[default]
    Name,
    Extension,
    Modified,
    Created,
    Added,
    Size,
    Kind,
    Permissions,
    Owner,
    Group,
    ColorLabel,
}

/// Sort direction (`ascending` or `descending`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

/// Split orientation of the dual panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    #[default]
    Vertical,
    Horizontal,
}

fn default_active_pane() -> String {
    "left".to_string()
}

const fn default_sidebar_width() -> f32 {
    212.0
}

const fn default_inspector_width() -> f32 {
    296.0
}

const fn default_size_col_width() -> f32 {
    88.0
}

const fn default_date_col_width() -> f32 {
    136.0
}

const fn default_miller_parent_width() -> f32 {
    210.0
}

const fn default_terminal_drawer_height() -> f32 {
    220.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSessionConfig {
    pub width: u32,
    pub height: u32,
    pub split_ratio: f64,
    pub split_direction: SplitDirection,
    #[serde(default)]
    pub show_hidden_files: bool,
    #[serde(default = "default_true")]
    pub show_preview_pane: bool,
    #[serde(default)]
    pub dual_pane_enabled: bool,
    #[serde(default = "default_active_pane")]
    pub active_pane: String,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default)]
    pub inspector_open: bool,
    #[serde(default = "default_inspector_width")]
    pub inspector_width: f32,
    #[serde(default = "default_size_col_width")]
    pub size_col_width: f32,
    #[serde(default = "default_date_col_width")]
    pub date_col_width: f32,
    #[serde(default = "default_miller_parent_width")]
    pub miller_parent_width: f32,
    #[serde(default = "default_terminal_drawer_height")]
    pub terminal_drawer_height: f32,
    #[serde(default)]
    pub inspector_tab_index: usize,
    #[serde(default)]
    pub terminal_drawer_open: bool,
}

impl Default for WindowSessionConfig {
    fn default() -> Self {
        Self {
            width: 1440,
            height: 900,
            split_ratio: 0.5,
            split_direction: SplitDirection::Vertical,
            show_hidden_files: false,
            show_preview_pane: true,
            dual_pane_enabled: false,
            active_pane: default_active_pane(),
            sidebar_width: default_sidebar_width(),
            inspector_open: false,
            inspector_width: default_inspector_width(),
            size_col_width: default_size_col_width(),
            date_col_width: default_date_col_width(),
            miller_parent_width: default_miller_parent_width(),
            terminal_drawer_height: default_terminal_drawer_height(),
            inspector_tab_index: 0,
            terminal_drawer_open: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSessionConfig {
    pub vfs_url: String,
    #[serde(default)]
    pub current_path: String,
    #[serde(default)]
    pub title: Option<String>,
    pub view_mode: PaneViewMode,
    pub sort_column: SortColumn,
    pub sort_direction: SortDirection,
    #[serde(default)]
    pub expanded_folders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneSessionConfig {
    pub active_tab_index: usize,
    pub tabs: Vec<TabSessionConfig>,
}

/// Workspace and session state (`session.toml`) from `Section 4.2` of `BRIEF.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionState {
    pub window: WindowSessionConfig,
    pub left_pane: PaneSessionConfig,
    pub right_pane: PaneSessionConfig,
}

impl SessionState {
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(input)?)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn default_config_path() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("manifold").join("session.toml");
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata).join("Manifold").join("session.toml");
            }
        }
        let home_var = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
        if let Ok(home) = home_var {
            if !home.trim().is_empty() {
                return PathBuf::from(home)
                    .join(".config")
                    .join("manifold")
                    .join("session.toml");
            }
        }
        std::env::temp_dir().join("manifold").join("session.toml")
    }

    pub fn load_default() -> Option<Self> {
        let path = Self::default_config_path();
        let raw = fs::read_to_string(path).ok()?;
        Self::from_toml_str(&raw).ok()
    }

    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = Self::default_config_path();
        let serialized = self.to_toml_string()?;
        write_secure_config_file(&path, &serialized)
    }
}

/// Writes a TOML configuration file atomically with `0700` parent directory and `0600` file
/// permissions on Unix (`SEC-07`).
fn write_secure_config_file(path: &Path, serialized: &str) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    let tmp_name = format!(".{file_name}.tmp.{}", std::process::id());
    let tmp_path = path.with_file_name(tmp_name);
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp_path)?;
        f.write_all(serialized.as_bytes())?;
        f.sync_all()?;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ConfigError::Io(err));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn default_fav_icon_kind() -> String {
    "folder".to_string()
}

/// Pinned folder shortcut in the Left Sidebar `Favorites` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteFolderEntry {
    pub id: String,
    pub label: String,
    pub vfs_url: String,
    #[serde(default = "default_fav_icon_kind")]
    pub icon_kind: String,
    #[serde(default)]
    pub color_label: ColorLabel,
}

fn default_theme_preset() -> String {
    "dark_zinc".to_string()
}

const fn default_font_size_delta() -> i32 {
    1
}

fn default_font_family() -> String {
    "sans-serif".to_string()
}

const fn default_icon_size() -> u16 {
    16
}

/// Scope for File & Directory View Options (`All Folders`, `This Folder`, `This Folder and Subfolders`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ViewOptionsScope {
    #[default]
    AllFolders,
    ThisFolder,
    ThisFolderAndSubfolders,
}

/// Normalizes a local or remote directory path across Windows (`C:\Users\...`),
/// Linux (`/home/...`), macOS (`/Users/...`), and Remote VFS (`sftp://...`)
/// for consistent folder-view-options lookup and subtree matching.
pub fn normalize_view_options_path(raw: &str) -> String {
    let trimmed = raw.trim().strip_prefix("file://").unwrap_or(raw.trim());
    let unified = trimmed.replace('\\', "/");
    if unified.is_empty() || unified == "/" {
        return "/".to_string();
    }
    let clean = unified.trim_end_matches('/');
    if clean.len() == 2 && clean.as_bytes()[1] == b':' {
        let drive = (clean.as_bytes()[0] as char).to_ascii_uppercase();
        return format!("{drive}:/");
    }
    if clean.len() > 2 && clean.as_bytes()[1] == b':' && clean.as_bytes()[2] == b'/' {
        let drive = (clean.as_bytes()[0] as char).to_ascii_uppercase();
        let rest = &clean[2..];
        return format!("{drive}:{rest}");
    }
    clean.to_string()
}

/// Per-folder or global File & Directory View Options (`Show View Options` popover).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderViewOptions {
    #[serde(default)]
    pub scope: ViewOptionsScope,
    #[serde(default)]
    pub sort_column: SortColumn,
    #[serde(default = "default_true")]
    pub col_size: bool,
    #[serde(default)]
    pub col_kind: bool,
    #[serde(default = "default_true")]
    pub col_date_modified: bool,
    #[serde(default)]
    pub col_date_created: bool,
    #[serde(default)]
    pub col_date_added: bool,
    #[serde(default)]
    pub col_owner: bool,
    #[serde(default)]
    pub col_group: bool,
    #[serde(default)]
    pub col_permissions: bool,
    #[serde(default = "default_true")]
    pub col_tags: bool,
    #[serde(default = "default_icon_size")]
    pub icon_size: u16,
    #[serde(default)]
    pub bold_folders: bool,
    #[serde(default)]
    pub show_hidden_files: bool,
    #[serde(default = "default_true")]
    pub show_icon_preview: bool,
    #[serde(default)]
    pub treat_packages_as_folders: bool,
    #[serde(default = "default_true")]
    pub show_folders_on_top: bool,
    #[serde(default)]
    pub folders_always_alpha: bool,
    #[serde(default)]
    pub use_relative_dates: bool,
    #[serde(default)]
    pub calculate_all_sizes: bool,
    #[serde(default)]
    pub use_alternating_row_bg: bool,
}

impl Default for FolderViewOptions {
    fn default() -> Self {
        Self {
            scope: ViewOptionsScope::AllFolders,
            sort_column: SortColumn::Name,
            col_size: true,
            col_kind: false,
            col_date_modified: true,
            col_date_created: false,
            col_date_added: false,
            col_owner: false,
            col_group: false,
            col_permissions: false,
            col_tags: true,
            icon_size: default_icon_size(),
            bold_folders: false,
            show_hidden_files: false,
            show_icon_preview: true,
            treat_packages_as_folders: false,
            show_folders_on_top: true,
            folders_always_alpha: false,
            use_relative_dates: false,
            calculate_all_sizes: false,
            use_alternating_row_bg: false,
        }
    }
}

/// Persisted rule step inside a saved Multi-Rename favorite preset (`preferences.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRenameRuleConfig {
    pub operation: String,
    pub apply_to: String,
    pub occurrences: String,
    pub find_text: String,
    pub replace_text: String,
    pub case_sensitive: bool,
    pub case_mode: String,
    pub text_position: String,
    pub add_text: String,
    pub seq_prefix: String,
    pub seq_position: String,
    pub seq_start: i32,
    pub seq_step: i32,
    pub seq_digits: i32,
    pub date_position: String,
    pub date_format: String,
}

/// Named Multi-Rename preset saved via `Add to Favorites` (`preferences.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRenameFavoritePreset {
    pub name: String,
    pub rules: Vec<BatchRenameRuleConfig>,
}

/// Persistent UI Preferences (`preferences.toml`) for Typography, Themes, Favorites, and View Options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPreferences {
    #[serde(default = "default_theme_preset")]
    pub theme_preset: String,
    #[serde(default = "default_font_size_delta")]
    pub font_size_delta: i32,
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default)]
    pub font_bold: bool,
    #[serde(default)]
    pub custom_favorites: Vec<FavoriteFolderEntry>,
    #[serde(default)]
    pub favorite_order: Vec<String>,
    #[serde(default)]
    pub sidebar_colors: HashMap<String, ColorLabel>,
    #[serde(default)]
    pub sidebar_labels: HashMap<String, String>,
    #[serde(default)]
    pub removed_favorites: Vec<String>,
    #[serde(default)]
    pub file_color_labels: HashMap<String, ColorLabel>,
    #[serde(default)]
    pub default_view_options: FolderViewOptions,
    #[serde(default)]
    pub folder_view_options: HashMap<String, FolderViewOptions>,
    #[serde(default)]
    pub max_bandwidth_mbps: u32,
    #[serde(default)]
    pub batch_rename_favorites: Vec<BatchRenameFavoritePreset>,
}

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            theme_preset: default_theme_preset(),
            font_size_delta: default_font_size_delta(),
            font_family: default_font_family(),
            font_bold: false,
            custom_favorites: Vec::new(),
            favorite_order: Vec::new(),
            sidebar_colors: HashMap::new(),
            sidebar_labels: HashMap::new(),
            removed_favorites: Vec::new(),
            file_color_labels: HashMap::new(),
            default_view_options: FolderViewOptions::default(),
            folder_view_options: HashMap::new(),
            max_bandwidth_mbps: 0,
            batch_rename_favorites: Vec::new(),
        }
    }
}

impl AppPreferences {
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(input)?)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn default_config_path() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("manifold").join("preferences.toml");
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return PathBuf::from(appdata)
                    .join("Manifold")
                    .join("preferences.toml");
            }
        }
        let home_var = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
        if let Ok(home) = home_var {
            if !home.trim().is_empty() {
                return PathBuf::from(home)
                    .join(".config")
                    .join("manifold")
                    .join("preferences.toml");
            }
        }
        std::env::temp_dir()
            .join("manifold")
            .join("preferences.toml")
    }

    pub fn load_default() -> Self {
        let path = Self::default_config_path();
        fs::read_to_string(path)
            .ok()
            .and_then(|s| Self::from_toml_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = Self::default_config_path();
        let serialized = self.to_toml_string()?;
        write_secure_config_file(&path, &serialized)
    }

    pub fn upsert_batch_rename_favorite(&mut self, preset: BatchRenameFavoritePreset) {
        if let Some(existing) = self
            .batch_rename_favorites
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&preset.name))
        {
            *existing = preset;
        } else {
            self.batch_rename_favorites.push(preset);
        }
    }

    pub fn remove_batch_rename_favorite(&mut self, idx: usize) -> bool {
        if idx < self.batch_rename_favorites.len() {
            self.batch_rename_favorites.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn upsert_favorite(&mut self, entry: FavoriteFolderEntry) {
        self.removed_favorites
            .retain(|r| r != &entry.id && r != &entry.vfs_url);
        let existing = self
            .custom_favorites
            .iter_mut()
            .find(|f| f.id == entry.id || f.vfs_url == entry.vfs_url);
        if let Some(found) = existing {
            *found = entry;
        } else {
            self.custom_favorites.push(entry);
        }
    }

    pub fn remove_favorite(&mut self, id_or_url: &str) -> bool {
        let prev = self.custom_favorites.len();
        self.custom_favorites
            .retain(|f| f.id != id_or_url && f.vfs_url != id_or_url);
        self.custom_favorites.len() != prev
    }

    pub fn remove_favorite_by_id_or_path(&mut self, id: &str, target_path: &str) -> bool {
        let prev = self.custom_favorites.len();
        self.custom_favorites
            .retain(|f| f.id != id && (target_path.is_empty() || f.vfs_url != target_path));
        let removed_custom = self.custom_favorites.len() != prev;
        let mut recorded_builtin = false;
        if !id.is_empty() && !self.removed_favorites.iter().any(|r| r == id) {
            self.removed_favorites.push(id.to_string());
            recorded_builtin = true;
        }
        if !target_path.is_empty() && !self.removed_favorites.iter().any(|r| r == target_path) {
            self.removed_favorites.push(target_path.to_string());
            recorded_builtin = true;
        }
        removed_custom || recorded_builtin
    }

    pub fn set_sidebar_color(&mut self, id: &str, color: ColorLabel) {
        if id.is_empty() {
            return;
        }
        if color == ColorLabel::None {
            self.sidebar_colors.remove(id);
        } else {
            self.sidebar_colors.insert(id.to_string(), color);
        }
        for fav in &mut self.custom_favorites {
            if fav.id == id {
                fav.color_label = color;
            }
        }
    }

    pub fn set_sidebar_label(&mut self, id: &str, label: &str) {
        let trimmed = label.trim();
        if id.is_empty() || trimmed.is_empty() {
            return;
        }
        self.sidebar_labels
            .insert(id.to_string(), trimmed.to_string());
        for fav in &mut self.custom_favorites {
            if fav.id == id {
                fav.label = trimmed.to_string();
            }
        }
    }

    pub fn set_file_color_label(&mut self, path: &str, color: ColorLabel) {
        if path.is_empty() {
            return;
        }
        if color == ColorLabel::None {
            self.file_color_labels.remove(path);
        } else {
            self.file_color_labels.insert(path.to_string(), color);
        }
    }

    /// Resolves the effective [`FolderViewOptions`] for `dir_path`:
    /// 1. Exact directory match in `folder_view_options` (`ThisFolder` or `ThisFolderAndSubfolders`).
    /// 2. Longest-prefix ancestor match in `folder_view_options` where `scope == ThisFolderAndSubfolders`.
    /// 3. Global `default_view_options` (`AllFolders`).
    pub fn effective_view_options(&self, dir_path: &str) -> FolderViewOptions {
        let norm = normalize_view_options_path(dir_path);
        if let Some(exact) = self.folder_view_options.get(&norm) {
            return exact.clone();
        }
        let mut best_match: Option<(usize, &FolderViewOptions)> = None;
        for (rule_path, opts) in &self.folder_view_options {
            if opts.scope != ViewOptionsScope::ThisFolderAndSubfolders {
                continue;
            }
            let r_norm = normalize_view_options_path(rule_path);
            let is_ancestor = if r_norm == "/" {
                norm.starts_with('/') && norm.len() > 1
            } else if r_norm.ends_with(":/") {
                norm.starts_with(&r_norm) && norm.len() > r_norm.len()
            } else {
                let prefix = format!("{r_norm}/");
                norm.starts_with(&prefix)
            };
            if is_ancestor {
                let r_len = r_norm.len();
                let better = best_match.is_none_or(|(best_len, _)| r_len > best_len);
                if better {
                    best_match = Some((r_len, opts));
                }
            }
        }
        if let Some((_, inherited)) = best_match {
            return inherited.clone();
        }
        self.default_view_options.clone()
    }

    /// Saves `opts` according to its `opts.scope` (`AllFolders` vs `ThisFolder` / `ThisFolderAndSubfolders`).
    pub fn save_view_options_for_path(&mut self, dir_path: &str, mut opts: FolderViewOptions) {
        let norm = normalize_view_options_path(dir_path);
        match opts.scope {
            ViewOptionsScope::AllFolders => {
                opts.scope = ViewOptionsScope::AllFolders;
                self.default_view_options = opts;
                self.folder_view_options.remove(&norm);
            }
            ViewOptionsScope::ThisFolder | ViewOptionsScope::ThisFolderAndSubfolders => {
                self.folder_view_options.insert(norm, opts);
            }
        }
    }

    /// Copies `opts` into `default_view_options` (`AllFolders`) and clears any redundant per-folder
    /// override on `dir_path` (`[ Use as Defaults ]` button in `Show View Options`).
    pub fn use_view_options_as_defaults(&mut self, dir_path: &str, mut opts: FolderViewOptions) {
        let norm = normalize_view_options_path(dir_path);
        opts.scope = ViewOptionsScope::AllFolders;
        self.default_view_options = opts;
        self.folder_view_options.remove(&norm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connections_toml_and_keyring_isolation() {
        let id = "c3f1b0a9-8d2e-4b31-97c2-1e9a2b5e7d4f";
        let profile = ConnectionProfile::new_sftp_default(id, "Staging SFTP");
        let cfg = ConnectionsConfig {
            connections: vec![profile.clone()],
        };

        let toml_text = cfg.to_toml_string().unwrap();
        // Verify serialized TOML contains expected fields and no secret values
        assert!(toml_text.contains("johnappleseed.com"));
        assert!(toml_text.contains("color_label = \"purple\""));

        let parsed = ConnectionsConfig::from_toml_str(&toml_text).unwrap();
        assert_eq!(parsed, cfg);

        // Verify web URL resolution from Base URL (doc/image3.png)
        let web_url = profile.web_url_for_remote_path("/User/Shared/Website/index.php");
        assert_eq!(
            web_url,
            Some("https://johnappleseed.com/index.php".to_string())
        );

        // Verify Keyring service is strictly "manifold"
        let store = CredentialStore::new();
        assert_eq!(store.service_name(), "manifold");
        store.set_secret(&profile.id, "s3cr3t-p@ss").unwrap();
        let secret = store.get_secret(&profile.id).unwrap();
        assert_eq!(secret, Some("s3cr3t-p@ss".to_string()));
        assert_eq!(
            escape_macos_security_token("p@ss\"w\\ord\n"),
            "\"p@ss\\\"w\\\\ord\""
        );
    }

    #[test]
    fn test_session_toml_roundtrip() {
        let sample = r#"
[window]
width = 1440
height = 900
split_ratio = 0.5
split_direction = "vertical"

[left_pane]
active_tab_index = 2

[[left_pane.tabs]]
vfs_url = "file:///Users/johnappleseed/Downloads"
view_mode = "list"
sort_column = "name"
sort_direction = "ascending"

[[left_pane.tabs]]
vfs_url = "file:///Users/johnappleseed/Desktop"
view_mode = "list"
sort_column = "name"
sort_direction = "ascending"

[[left_pane.tabs]]
vfs_url = "file:///Users/johnappleseed/Documents/Website Backup"
view_mode = "tree"
sort_column = "name"
sort_direction = "ascending"

[right_pane]
active_tab_index = 2

[[right_pane.tabs]]
vfs_url = "favorites://root"
view_mode = "icons"
sort_column = "name"
sort_direction = "ascending"

[[right_pane.tabs]]
vfs_url = "file:///Users/johnappleseed"
view_mode = "columns"
sort_column = "name"
sort_direction = "ascending"

[[right_pane.tabs]]
vfs_url = "file:///Users/johnappleseed/Photos"
view_mode = "icons"
sort_column = "name"
sort_direction = "ascending"
"#;
        let state = SessionState::from_toml_str(sample).unwrap();
        assert_eq!(state.left_pane.active_tab_index, 2);
        assert_eq!(state.left_pane.tabs.len(), 3);
        assert_eq!(state.left_pane.tabs[2].view_mode, PaneViewMode::Tree);
        assert_eq!(state.right_pane.tabs[1].view_mode, PaneViewMode::Columns);
        assert_eq!(state.right_pane.tabs[2].view_mode, PaneViewMode::Icons);
    }

    #[test]
    fn preferences_roundtrips_and_manages_favorites() {
        let mut prefs = AppPreferences::default();
        assert_eq!(prefs.font_size_delta, 1);
        assert_eq!(prefs.theme_preset, "dark_zinc");
        assert_eq!(prefs.font_family, "sans-serif");

        prefs.theme_preset = "midnight_ocean".to_string();
        prefs.font_size_delta = 2;
        prefs.font_family = "monospace".to_string();
        prefs.font_bold = true;
        prefs.upsert_favorite(FavoriteFolderEntry {
            id: "fav-work".to_string(),
            label: "Workspace".to_string(),
            vfs_url: "/tmp/workspace".to_string(),
            icon_kind: "folder".to_string(),
            color_label: ColorLabel::Blue,
        });
        prefs.set_sidebar_color("fav-documents", ColorLabel::Purple);
        prefs.set_sidebar_label("fav-documents", "My Docs");
        prefs.set_file_color_label("/tmp/workspace/report.pdf", ColorLabel::Green);
        assert_eq!(prefs.custom_favorites.len(), 1);
        assert_eq!(
            prefs.sidebar_colors.get("fav-documents"),
            Some(&ColorLabel::Purple)
        );
        assert_eq!(
            prefs.sidebar_labels.get("fav-documents"),
            Some(&"My Docs".to_string())
        );
        assert_eq!(
            prefs.file_color_labels.get("/tmp/workspace/report.pdf"),
            Some(&ColorLabel::Green)
        );

        let serialized = prefs.to_toml_string().unwrap();
        let parsed = AppPreferences::from_toml_str(&serialized).unwrap();
        assert_eq!(parsed, prefs);

        let mut updated = parsed;
        assert!(updated.remove_favorite_by_id_or_path("fav-work", "/tmp/workspace"));
        assert!(updated.custom_favorites.is_empty());
        assert!(updated.removed_favorites.iter().any(|r| r == "fav-work"));
    }

    #[test]
    fn test_folder_view_options_scope_inheritance_and_cross_platform_paths() {
        let mut prefs = AppPreferences::default();
        assert_eq!(
            normalize_view_options_path(r"c:\Users\dimitri\Downloads\"),
            "C:/Users/dimitri/Downloads"
        );
        assert_eq!(
            normalize_view_options_path("file:///home/dimitri/Documents/"),
            "/home/dimitri/Documents"
        );

        // 1. Subtree override ("This Folder and Subfolders") on /Users/dimitri/Projects
        let mut subtree_opts = FolderViewOptions {
            scope: ViewOptionsScope::ThisFolderAndSubfolders,
            col_owner: true,
            col_permissions: true,
            bold_folders: true,
            ..FolderViewOptions::default()
        };
        prefs.save_view_options_for_path("/Users/dimitri/Projects/", subtree_opts.clone());

        // Child directory inherits subtree_opts
        let child_eff = prefs.effective_view_options("/Users/dimitri/Projects/Manifold/src");
        assert_eq!(child_eff.scope, ViewOptionsScope::ThisFolderAndSubfolders);
        assert!(child_eff.col_owner);
        assert!(child_eff.col_permissions);
        assert!(child_eff.bold_folders);

        // 2. Specific folder override ("This Folder") on /Users/dimitri/Projects/Manifold
        let folder_only = FolderViewOptions {
            scope: ViewOptionsScope::ThisFolder,
            col_kind: true,
            use_alternating_row_bg: true,
            ..FolderViewOptions::default()
        };
        prefs.save_view_options_for_path("/Users/dimitri/Projects/Manifold", folder_only);
        let exact_eff = prefs.effective_view_options("/Users/dimitri/Projects/Manifold");
        assert_eq!(exact_eff.scope, ViewOptionsScope::ThisFolder);
        assert!(exact_eff.col_kind);
        assert!(!exact_eff.col_owner);

        // Deeper subfolder still inherits the ancestor's ThisFolderAndSubfolders rule
        let deep_eff = prefs.effective_view_options("/Users/dimitri/Projects/Manifold/crates");
        assert_eq!(deep_eff.scope, ViewOptionsScope::ThisFolderAndSubfolders);
        assert!(deep_eff.col_owner);

        // 3. Use as Defaults promotes options to AllFolders
        subtree_opts.use_relative_dates = true;
        prefs.use_view_options_as_defaults("/Users/dimitri/Projects", subtree_opts);
        let other_dir = prefs.effective_view_options(r"C:\Users\dimitri\Desktop");
        assert_eq!(other_dir.scope, ViewOptionsScope::AllFolders);
        assert!(other_dir.use_relative_dates);
    }

    #[test]
    fn test_v070_connection_profile_validation_and_batch_rename_favorites() {
        let mut valid = ConnectionProfile::new_sftp_default("id-ok", "Prod SFTP");
        assert!(valid.validate().is_ok());

        let mut port_zero = valid.clone();
        port_zero.port = Some(0);
        assert!(port_zero.validate().is_err());

        let mut bad_host = valid.clone();
        bad_host.host = Some("-oProxyCommand=sh".to_string());
        assert!(bad_host.validate().is_err());

        let mut space_host = valid.clone();
        space_host.host = Some("prod.example.com --bad".to_string());
        assert!(space_host.validate().is_err());

        valid.username = "-oPermitLocalCommand=yes".to_string();
        assert!(valid.validate().is_err());

        let mut prefs = AppPreferences {
            max_bandwidth_mbps: 50,
            ..AppPreferences::default()
        };
        prefs.upsert_batch_rename_favorite(BatchRenameFavoritePreset {
            name: "Photo Sequence".to_string(),
            rules: vec![BatchRenameRuleConfig {
                operation: "Add Sequence".to_string(),
                apply_to: "Name".to_string(),
                occurrences: "All".to_string(),
                find_text: String::new(),
                replace_text: String::new(),
                case_sensitive: false,
                case_mode: "Title Case".to_string(),
                text_position: "After Name".to_string(),
                add_text: String::new(),
                seq_prefix: "_".to_string(),
                seq_position: "After Name".to_string(),
                seq_start: 1,
                seq_step: 1,
                seq_digits: 3,
                date_position: "After Name".to_string(),
                date_format: "File Modified (YYYY-MM-DD)".to_string(),
            }],
        });
        let raw = prefs.to_toml_string().unwrap();
        let roundtrip = AppPreferences::from_toml_str(&raw).unwrap();
        assert_eq!(roundtrip.max_bandwidth_mbps, 50);
        assert_eq!(roundtrip.batch_rename_favorites.len(), 1);
        assert_eq!(roundtrip.batch_rename_favorites[0].name, "Photo Sequence");

        let dir = std::env::temp_dir().join(format!("manifold_cfg_sec07_{}", std::process::id()));
        let cfg_file = dir.join("preferences.toml");
        write_secure_config_file(&cfg_file, &raw).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_meta = fs::metadata(&cfg_file).unwrap();
            let dir_meta = fs::metadata(&dir).unwrap();
            let file_mode = file_meta.permissions().mode() & 0o777;
            let dir_mode = dir_meta.permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            assert_eq!(dir_mode, 0o700);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
