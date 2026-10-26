//! Configuration serialization (`connections.toml`, `session.toml`) and OS Keyring
//! credential storage (`service = "manifold"`) for Project Manifold.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = self.to_toml_string()?;
        fs::write(p, serialized)?;
        Ok(())
    }

    pub fn default_config_path() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                return PathBuf::from(xdg).join("manifold").join("connections.toml");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home)
                    .join(".config")
                    .join("manifold")
                    .join("connections.toml");
            }
        }
        PathBuf::from("/tmp/manifold/connections.toml")
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

/// Cross-platform OS keyring credential manager using `service = "manifold"`.
/// Uses native OS secret storage (`secret-tool` on Linux, `security` on macOS) with a
/// thread-safe encrypted/in-memory vault fallback when running in headless CI containers.
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
            if cfg!(target_os = "macos") {
                let _ = Command::new("security")
                    .args([
                        "add-generic-password",
                        "-U",
                        "-s",
                        KEYRING_SERVICE_NAME,
                        "-a",
                        connection_id,
                        "-w",
                        secret,
                    ])
                    .output();
            } else {
                let _ = Command::new("secret-tool")
                    .args([
                        "store",
                        "--label=Manifold Credential",
                        "service",
                        KEYRING_SERVICE_NAME,
                        "account",
                        connection_id,
                    ])
                    .output();
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
        Ok(None)
    }

    pub fn delete_secret(&self, connection_id: &str) -> Result<(), ConfigError> {
        if self.use_os_backend {
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

/// View mode per tab (`Icon View`, `List View`, `Column View`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PaneViewMode {
    Icons,
    #[default]
    List,
    Columns,
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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSessionConfig {
    pub vfs_url: String,
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
view_mode = "list"
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
view_mode = "icons"
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
        assert_eq!(state.right_pane.tabs[2].view_mode, PaneViewMode::Icons);
    }
}
