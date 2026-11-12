//! Remote VFS Connectors (`manifold-vfs-remote`) for Project Manifold.
//!
//! Provides unified [`FileSystemBackend`] implementations for SFTP/SSH, Cloud Object Storage
//! (S3, Google Drive, GCS, Azure Blob), FTP/FTPS, WebDAV, and SMB shares, plus the
//! Remote Edit-in-Place staging manager (`$XDG_CACHE_HOME/manifold/staging/<session_id>/<filename>`)
//! and SSH Terminal command builder.

pub mod protocols;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use manifold_config::{AuthType, ConnectionProfile, ProtocolType};
use manifold_vfs::{
    create_secure_temp_file, secure_runtime_subdir, ColorLabel, FileMetadata, FileSystemBackend,
    VfsError, VolumeStats,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static SSH_SENTINEL_SEQ: AtomicU64 = AtomicU64::new(1);

const PTY_SSH_SCRIPT: &str = r#"
import os, pty, select, sys

cmd = sys.argv[1:]
auth_mode = os.environ.get("MANIFOLD_SSH_AUTH_MODE", "none")
sentinel = os.environ.get("MANIFOLD_SSH_SENTINEL", "").encode("utf-8")
pw = ""
if auth_mode in ("password", "key"):
    raw_pw = sys.stdin.readline()
    pw = raw_pw.rstrip("\r\n")
pid, fd = pty.fork()
if pid == 0:
    os.environ["LC_ALL"] = "C"
    os.execvp(cmd[0], cmd)

out = bytearray()
sent_pw = False
auth_closed = auth_mode not in ("password", "key") or not pw
timeout_sec = float(os.environ.get("MANIFOLD_SSH_TIMEOUT", "30.0"))
while True:
    r, _, _ = select.select([fd], [], [], timeout_sec)
    if not r:
        break
    try:
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        out.extend(chunk)
        if sentinel and sentinel in out:
            auth_closed = True
        if not sent_pw and not auth_closed and pw:
            low = out.lower()
            matched = (
                (auth_mode == "password" and b"password:" in low)
                or (auth_mode == "key" and b"passphrase" in low)
            )
            if matched:
                os.write(fd, (pw + "\n").encode("utf-8"))
                sent_pw = True
                auth_closed = True
                out.clear()
    except OSError:
        break

os.close(fd)
_, status = os.waitpid(pid, 0)
sys.stdout.buffer.write(bytes(out))
sys.stdout.buffer.flush()
code = os.waitstatus_to_exitcode(status) if hasattr(os, "waitstatus_to_exitcode") else (status >> 8)
sys.exit(code)
"#;

/// Validates SSH/SCP host, username, and optional key path against option injection (`SEC-01`).
pub fn validate_ssh_endpoint(
    host: &str,
    username: &str,
    key_path: Option<&str>,
) -> Result<(), VfsError> {
    let h = host.trim();
    if h.is_empty() {
        return Err(VfsError::InvalidPath("Missing SFTP host".to_string()));
    }
    if h.starts_with('-') {
        let msg = format!("Rejected unsafe SSH host starting with '-': {h}");
        return Err(VfsError::InvalidPath(msg));
    }
    let u = username.trim();
    if u.starts_with('-') {
        let msg = format!("Rejected unsafe SSH username starting with '-': {u}");
        return Err(VfsError::InvalidPath(msg));
    }
    let unsafe_chars = [';', '&', '|', '`', '$', '\n', '\r', '\0', ' ', '\t'];
    if h.contains(unsafe_chars) || u.contains(unsafe_chars) {
        let msg = "Unsafe characters in SSH host or username".to_string();
        return Err(VfsError::InvalidPath(msg));
    }
    if let Some(kp) = key_path {
        let k = kp.trim();
        if k.starts_with('-') || k.contains(['\0', '\n', '\r']) {
            let msg = format!("Rejected unsafe SSH key path: {k}");
            return Err(VfsError::InvalidPath(msg));
        }
    }
    Ok(())
}

fn safe_local_scp_arg(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let needs_prefix = raw.starts_with('-')
        || raw
            .split('/')
            .next()
            .is_some_and(|first_seg| first_seg.contains(':'));
    if needs_prefix {
        format!("./{raw}")
    } else {
        raw.into_owned()
    }
}

#[derive(Debug, Clone)]
struct RemoteNode {
    metadata: FileMetadata,
    data: Vec<u8>,
}

/// Unified Remote VFS Backend supporting SFTP, S3, Google Drive, GCS, Azure, FTP/FTPS, WebDAV, and SMB.
#[derive(Clone)]
pub struct RemoteVfsBackend {
    profile: ConnectionProfile,
    secret: Option<String>,
    remote_uid: Option<u32>,
    live_mode: bool,
    connected: Arc<RwLock<bool>>,
    nodes: Arc<RwLock<HashMap<String, RemoteNode>>>,
    volume_stats: VolumeStats,
}

impl RemoteVfsBackend {
    /// Creates and connects a remote VFS session from a [`ConnectionProfile`].
    pub fn connect(profile: ConnectionProfile) -> Result<Self, VfsError> {
        if profile.username.trim().is_empty()
            && !matches!(
                profile.protocol,
                ProtocolType::S3
                    | ProtocolType::GoogleDrive
                    | ProtocolType::WebDav
                    | ProtocolType::Ftp
                    | ProtocolType::Ftps
            )
        {
            let msg = format!("Missing username for connection '{}'", profile.name);
            return Err(VfsError::AuthFailed(msg));
        }
        if profile.protocol == ProtocolType::Sftp {
            if let Some(host) = profile.host.as_deref() {
                validate_ssh_endpoint(host, &profile.username, profile.key_path.as_deref())?;
            }
        }

        let raw_base = if profile.base_path.is_empty() {
            "/"
        } else {
            &profile.base_path
        };
        let base = Self::normalize_remote_path(raw_base)?;

        let mut map = HashMap::new();
        // Ensure "/" and base_path exist
        for dir in ["/", base.as_str()] {
            let name = if dir == "/" {
                "/".to_string()
            } else {
                dir.rsplit('/').next().unwrap_or(dir).to_string()
            };
            map.insert(
                dir.to_string(),
                RemoteNode {
                    metadata: FileMetadata {
                        name,
                        path: dir.to_string(),
                        extension: None,
                        kind: "Folder".to_string(),
                        size: None,
                        is_dir: true,
                        is_symlink: false,
                        is_hidden: false,
                        is_locked: false,
                        hide_extension: false,
                        color_label: profile.color_label,
                        modified: Some(SystemTime::now()),
                        created: Some(SystemTime::now()),
                        added: Some(SystemTime::now()),
                        permissions: 0o755,
                        owner: Some(profile.username.clone()),
                        group: Some("staff".to_string()),
                    },
                    data: Vec::new(),
                },
            );
        }

        // Default capacity matching `470,07 GB` in `doc/image1.png` (`Shares > Binarynights > Office`)
        let default_avail = 470_070_000_000u64;
        Ok(Self {
            profile,
            secret: None,
            remote_uid: None,
            live_mode: false,
            connected: Arc::new(RwLock::new(true)),
            nodes: Arc::new(RwLock::new(map)),
            volume_stats: VolumeStats {
                total_bytes: 500_000_000_000,
                available_bytes: default_avail,
            },
        })
    }

    /// Creates and connects a live remote VFS session across SFTP, S3, WebDAV, FTP/FTPS, and SMB (`GAP-01`).
    pub fn connect_live(
        profile: ConnectionProfile,
        secret: Option<String>,
    ) -> Result<Self, VfsError> {
        let mut backend = Self::connect(profile)?;
        backend.secret = secret;
        backend.live_mode = true;
        if backend.profile.protocol == ProtocolType::Sftp {
            if let Ok(uid_out) = backend.run_remote_shell("id -u 2>/dev/null") {
                backend.remote_uid = uid_out.trim().parse::<u32>().ok();
            }
        }
        let init_path = if backend.profile.base_path.trim().is_empty() {
            "/".to_string()
        } else {
            backend.profile.base_path.clone()
        };
        if backend.list_directory(&init_path).is_err() {
            backend.list_directory("/")?;
            backend.profile.base_path = "/".to_string();
        }
        Ok(backend)
    }

    /// Returns the optional secret (password or passphrase) held for this remote session.
    pub fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }

    fn is_live_ssh(&self) -> bool {
        self.live_mode && self.profile.protocol == ProtocolType::Sftp
    }

    fn ssh_base_args(&self, is_scp: bool) -> Result<(Vec<String>, String), VfsError> {
        let missing_host = || VfsError::InvalidPath("Missing SFTP host".to_string());
        let host = self
            .profile
            .host
            .as_deref()
            .ok_or_else(missing_host)?
            .trim();
        let user = self.profile.username.trim();
        validate_ssh_endpoint(host, user, self.profile.key_path.as_deref())?;
        let port = self.profile.port.unwrap_or(22).to_string();
        let port_flag = if is_scp { "-P" } else { "-p" };
        let mut args = vec![
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            "ConnectTimeout=6".to_string(),
            port_flag.to_string(),
            port,
        ];
        if let Some(key) = &self.profile.key_path {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{}/{rest}", home.trim_end_matches('/'))
                } else {
                    trimmed.to_string()
                };
                if Path::new(&expanded).exists() {
                    args.push("-i".to_string());
                    args.push(expanded);
                }
            }
        }
        if self.secret.as_deref().unwrap_or("").is_empty() {
            args.push("-o".to_string());
            args.push("BatchMode=yes".to_string());
        }
        let target = format!("{user}@{host}");
        Ok((args, target))
    }

    fn exec_pty_cmd_with_sentinel(
        &self,
        cmd_vec: &[String],
        timeout_sec: &str,
        sentinel: &str,
    ) -> Result<Vec<u8>, VfsError> {
        let secret_str = self.secret.as_deref().unwrap_or("");
        let auth_mode = if secret_str.is_empty() {
            "none"
        } else {
            match self.profile.auth_type {
                AuthType::Password => "password",
                AuthType::PublicKey => "key",
                _ => "none",
            }
        };
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(PTY_SSH_SCRIPT)
            .args(cmd_vec)
            .env("MANIFOLD_SSH_AUTH_MODE", auth_mode)
            .env("MANIFOLD_SSH_SENTINEL", sentinel)
            .env("MANIFOLD_SSH_TIMEOUT", timeout_sec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| VfsError::NetworkError(format!("Failed to invoke python3/ssh: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            if auth_mode != "none" {
                let _ = writeln!(stdin, "{secret_str}");
            }
        }
        let out = child
            .wait_with_output()
            .map_err(|e| VfsError::NetworkError(format!("Failed waiting for python3/ssh: {e}")))?;
        if !out.status.success() {
            let err_txt = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let msg = if err_txt.is_empty() {
                format!("SSH exited with status {}", out.status)
            } else {
                err_txt
            };
            return Err(VfsError::NetworkError(msg));
        }
        Ok(out.stdout)
    }

    fn exec_pty_cmd_with_timeout(
        &self,
        cmd_vec: &[String],
        timeout_sec: &str,
    ) -> Result<Vec<u8>, VfsError> {
        self.exec_pty_cmd_with_sentinel(cmd_vec, timeout_sec, "")
    }

    fn run_remote_shell(&self, remote_sh: &str) -> Result<String, VfsError> {
        let (base_args, target) = self.ssh_base_args(false)?;
        let seq = SSH_SENTINEL_SEQ.fetch_add(1, Ordering::Relaxed);
        let sentinel = format!("__MANIFOLD_BEGIN_{}_{seq}__", std::process::id());
        let mut cmd_vec = vec!["ssh".to_string()];
        cmd_vec.extend(base_args);
        cmd_vec.push("--".to_string());
        cmd_vec.push(target);
        let wrapped = format!("printf '{sentinel}\\n'; {remote_sh}");
        cmd_vec.push(wrapped);
        let raw = self.exec_pty_cmd_with_sentinel(&cmd_vec, "30.0", &sentinel)?;
        let text = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
        let marker = format!("{sentinel}\n");
        if let Some((_, body)) = text.split_once(&marker) {
            Ok(body.to_string())
        } else {
            Ok(text)
        }
    }

    /// Executes a command inside `remote_dir` for the embedded bottom terminal.
    pub fn execute_shell_in_dir(&self, remote_dir: &str, cmd: &str) -> Result<String, VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(remote_dir)?;
        if self.is_live_ssh() {
            let esc = norm.replace('\'', "'\\''");
            let sh = format!("cd '{esc}' 2>/dev/null && ({cmd}) 2>&1");
            return self.run_remote_shell(&sh);
        }
        Ok(format!("[{}] ({norm}) $ {cmd}\nOK\n", self.profile.name))
    }

    /// Streams a local file directly to a remote path without buffering the entire file in RAM.
    pub fn upload_local_file(&self, local_path: &Path, remote_path: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(remote_path)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let (base_args, target) = self.ssh_base_args(true)?;
                    let local_str = safe_local_scp_arg(local_path);
                    let mut cmd_vec = vec!["scp".to_string(), "-q".to_string()];
                    cmd_vec.extend(base_args);
                    cmd_vec.push("--".to_string());
                    cmd_vec.push(local_str);
                    cmd_vec.push(format!("{target}:{norm}"));
                    self.exec_pty_cmd_with_timeout(&cmd_vec, "3600.0")?;
                    return Ok(());
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    return protocols::s3_upload_file(
                        &self.profile,
                        self.secret.as_deref(),
                        local_path,
                        &norm,
                    );
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    return protocols::webdav_upload_file(
                        &self.profile,
                        self.secret.as_deref(),
                        local_path,
                        &norm,
                    );
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    return protocols::ftp_upload_file(
                        &self.profile,
                        self.secret.as_deref(),
                        local_path,
                        &norm,
                    );
                }
                ProtocolType::Smb => {
                    return protocols::smb_upload_file(
                        &self.profile,
                        self.secret.as_deref(),
                        local_path,
                        &norm,
                    );
                }
            }
        }
        let file_meta = fs::metadata(local_path)?;
        let file_len = file_meta.len();
        let sample = if file_len <= 65_536 {
            fs::read(local_path).unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some((parent, _)) = norm.rsplit_once('/') {
            let parent_norm = if parent.is_empty() { "/" } else { parent };
            if !self.nodes.read().unwrap().contains_key(parent_norm) {
                self.create_dir(parent_norm)?;
            }
        }
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        let (extension, kind) = manifold_vfs::infer_file_kind(&name, false);
        let mut nodes = self.nodes.write().unwrap();
        nodes.insert(
            norm.clone(),
            RemoteNode {
                metadata: FileMetadata {
                    name,
                    path: norm,
                    extension,
                    kind,
                    size: Some(file_len),
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
                    owner: Some(self.profile.username.clone()),
                    group: Some("staff".to_string()),
                },
                data: sample,
            },
        );
        Ok(())
    }

    /// Streams a remote file directly to a local path without buffering the entire file in RAM.
    pub fn download_to_local_file(
        &self,
        remote_path: &str,
        local_path: &Path,
    ) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(remote_path)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let (base_args, target) = self.ssh_base_args(true)?;
                    let local_str = safe_local_scp_arg(local_path);
                    let mut cmd_vec = vec!["scp".to_string(), "-q".to_string()];
                    cmd_vec.extend(base_args);
                    cmd_vec.push("--".to_string());
                    cmd_vec.push(format!("{target}:{norm}"));
                    cmd_vec.push(local_str);
                    self.exec_pty_cmd_with_timeout(&cmd_vec, "3600.0")?;
                    return Ok(());
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    return protocols::s3_download_file(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        local_path,
                    );
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    return protocols::webdav_download_file(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        local_path,
                    );
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    return protocols::ftp_download_file(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        local_path,
                    );
                }
                ProtocolType::Smb => {
                    return protocols::smb_download_file(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        local_path,
                    );
                }
            }
        }
        let nodes = self.nodes.read().unwrap();
        let found = nodes
            .get(&norm)
            .map(|n| n.data.clone())
            .ok_or(VfsError::NotFound(norm))?;
        drop(nodes);
        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(local_path, &found)?;
        Ok(())
    }

    pub fn profile(&self) -> &ConnectionProfile {
        &self.profile
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.read().unwrap()
    }

    pub fn disconnect(&self) {
        *self.connected.write().unwrap() = false;
    }

    /// Recursively searches `base_dir` and its downstream subdirectories for entries matching `query`.
    pub fn search_recursive(
        &self,
        base_dir: &str,
        query: &str,
        show_hidden: bool,
        max_results: usize,
    ) -> Result<Vec<FileMetadata>, VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(base_dir)?;
        let q_low = query.trim().to_lowercase();
        if q_low.is_empty() {
            return Ok(Vec::new());
        }
        let cap = max_results.clamp(1, 2500);

        if self.is_live_ssh() {
            let esc_dir = norm.replace('\'', "'\\''");
            let glob_raw = if q_low.contains('*') || q_low.contains('?') {
                q_low.clone()
            } else {
                format!("*{q_low}*")
            };
            let esc_glob = glob_raw.replace('\'', "'\\''");
            let prune_hidden = if show_hidden {
                ""
            } else {
                "\\( -name '.*' -prune \\) -o "
            };
            let find_cmd =
                format!("find '{esc_dir}' -maxdepth 8 {prune_hidden}-iname '{esc_glob}'");
            let sh = format!("{find_cmd} -exec ls -ldn -- {{}} + 2>/dev/null | head -n {cap}");
            let out = self.run_remote_shell(&sh)?;
            let mut list = parse_ls_ldn_find_output(
                &norm,
                &out,
                &self.profile.username,
                self.remote_uid,
                show_hidden,
            );
            list.truncate(cap);
            return Ok(list);
        }

        let prefix = if norm == "/" {
            "/".to_string()
        } else {
            format!("{norm}/")
        };
        let nodes = self.nodes.read().unwrap();
        let mut results = Vec::new();
        for (path, node) in nodes.iter() {
            if path == &norm || !path.starts_with(&prefix) {
                continue;
            }
            if !show_hidden {
                let rel = path.strip_prefix(&prefix).unwrap_or(path);
                if rel.split('/').any(|seg| seg.starts_with('.')) {
                    continue;
                }
            }
            if remote_name_matches_query(&node.metadata.name, &q_low) {
                results.push(node.metadata.clone());
                if results.len() >= cap {
                    break;
                }
            }
        }
        Ok(results)
    }

    fn ensure_connected(&self) -> Result<(), VfsError> {
        if !self.is_connected() {
            let msg = format!("Connection '{}' is disconnected", self.profile.name);
            return Err(VfsError::NetworkError(msg));
        }
        Ok(())
    }

    fn normalize_remote_path(path: &str) -> Result<String, VfsError> {
        if path.contains(['\0', '\r', '\n']) || path.split(['/', '\\']).any(|seg| seg == "..") {
            let msg = format!("Unsafe remote path rejected: {path}");
            return Err(VfsError::InvalidPath(msg));
        }
        if path.trim().is_empty() {
            return Ok("/".to_string());
        }
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(format!("/{trimmed}"))
        }
    }

    fn cache_listed_entries(&self, norm: &str, list: &[FileMetadata]) {
        let mut nodes = self.nodes.write().unwrap();
        nodes.entry(norm.to_string()).or_insert_with(|| RemoteNode {
            metadata: FileMetadata {
                name: norm.rsplit('/').next().unwrap_or("/").to_string(),
                path: norm.to_string(),
                extension: None,
                kind: "Folder".to_string(),
                size: None,
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
                owner: Some(self.profile.username.clone()),
                group: Some("staff".to_string()),
            },
            data: Vec::new(),
        });
        for item in list {
            nodes.insert(
                item.path.clone(),
                RemoteNode {
                    metadata: item.clone(),
                    data: Vec::new(),
                },
            );
        }
    }
}

impl FileSystemBackend for RemoteVfsBackend {
    fn scheme(&self) -> &str {
        match self.profile.protocol {
            ProtocolType::Sftp => "sftp",
            ProtocolType::Ftp => "ftp",
            ProtocolType::Ftps => "ftps",
            ProtocolType::S3 => "s3",
            ProtocolType::GoogleDrive => "gdrive",
            ProtocolType::Gcs => "gcs",
            ProtocolType::AzureBlob => "azure",
            ProtocolType::WebDav => "webdav",
            ProtocolType::Smb => "smb",
        }
    }

    fn list_directory(&self, path: &str) -> Result<Vec<FileMetadata>, VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_mode {
            let list = match self.profile.protocol {
                ProtocolType::Sftp => {
                    let esc = norm.replace('\'', "'\\''");
                    let sh = format!(
                        "LC_ALL=C ls -aln -- '{esc}' \
                         && printf '\\n__MANIFOLD_DIRLINKS__\\n' \
                         && (cd -- '{esc}' 2>/dev/null \
                         && for f in .* *; do \
                         [ \"$f\" != \".\" ] && [ \"$f\" != \"..\" ] \
                         && [ -L \"$f\" ] && [ -d \"$f\" ] \
                         && printf '%s\\n' \"$f\"; \
                         done; true)"
                    );
                    let out = self.run_remote_shell(&sh)?;
                    let (ls_part, dirlinks_part) = out
                        .split_once("\n__MANIFOLD_DIRLINKS__\n")
                        .unwrap_or((&out, ""));
                    let dir_symlinks: HashSet<&str> = dirlinks_part
                        .lines()
                        .map(|l| l.trim_end_matches('\r'))
                        .filter(|l| !l.is_empty())
                        .collect();
                    let mut items = parse_ls_aln_output(
                        &norm,
                        ls_part,
                        &self.profile.username,
                        self.remote_uid,
                    );
                    for item in &mut items {
                        if item.is_symlink && dir_symlinks.contains(item.name.as_str()) {
                            item.is_dir = true;
                            item.size = None;
                            item.kind = "Folder".to_string();
                            item.extension = None;
                        }
                    }
                    items.sort_by_key(|a| a.name.to_lowercase());
                    items
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    protocols::s3_list_directory(&self.profile, self.secret.as_deref(), &norm)?
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    protocols::webdav_list_directory(&self.profile, self.secret.as_deref(), &norm)?
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    protocols::ftp_list_directory(&self.profile, self.secret.as_deref(), &norm)?
                }
                ProtocolType::Smb => {
                    protocols::smb_list_directory(&self.profile, self.secret.as_deref(), &norm)?
                }
            };
            self.cache_listed_entries(&norm, &list);
            return Ok(list);
        }
        let nodes = self.nodes.read().unwrap();
        if !nodes.contains_key(&norm) {
            return Err(VfsError::NotFound(norm));
        }
        let prefix = if norm == "/" {
            "/".to_string()
        } else {
            format!("{norm}/")
        };

        let mut list = Vec::new();
        for (k, node) in nodes.iter() {
            if k == &norm {
                continue;
            }
            if let Some(rest) = k.strip_prefix(&prefix) {
                let is_direct_child = !rest.is_empty() && !rest.contains('/');
                if is_direct_child {
                    list.push(node.metadata.clone());
                }
            }
        }
        list.sort_by_key(|a| a.name.to_lowercase());
        Ok(list)
    }

    fn metadata(&self, path: &str) -> Result<FileMetadata, VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        let parent_norm = norm
            .rsplit_once('/')
            .map(|(p, _)| if p.is_empty() { "/" } else { p })
            .unwrap_or("/");
        let nodes = self.nodes.read().unwrap();
        if let Some(n) = nodes.get(&norm) {
            return Ok(n.metadata.clone());
        }
        let parent_cached = norm != "/" && nodes.contains_key(parent_norm);
        drop(nodes);
        if self.live_mode {
            if norm == "/" {
                return Ok(FileMetadata {
                    name: "/".to_string(),
                    path: norm,
                    extension: None,
                    kind: "Folder".to_string(),
                    size: None,
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
                    owner: Some(self.profile.username.clone()),
                    group: Some("staff".to_string()),
                });
            }
            if parent_cached {
                return Err(VfsError::NotFound(norm));
            }
            if self.is_live_ssh() {
                let esc = norm.replace('\'', "'\\''");
                let cmd = format!(
                    "LC_ALL=C ls -ldnL -- '{esc}' 2>/dev/null || LC_ALL=C ls -ldn -- '{esc}'"
                );
                if let Ok(raw) = self.run_remote_shell(&cmd) {
                    let parsed = parse_ls_ldn_find_output(
                        "",
                        &raw,
                        &self.profile.username,
                        self.remote_uid,
                        true,
                    );
                    if let Some(meta) = parsed.into_iter().next() {
                        let mut nodes = self.nodes.write().unwrap();
                        nodes.insert(
                            norm,
                            RemoteNode {
                                metadata: meta.clone(),
                                data: Vec::new(),
                            },
                        );
                        return Ok(meta);
                    }
                }
            }
        }
        Err(VfsError::NotFound(norm))
    }

    fn volume_stats(&self, _path: &str) -> Result<VolumeStats, VfsError> {
        self.ensure_connected()?;
        Ok(self.volume_stats)
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let esc = norm.replace('\'', "'\\''");
                    self.run_remote_shell(&format!("mkdir -p -- '{esc}'"))?;
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    protocols::s3_create_dir(&self.profile, self.secret.as_deref(), &norm)?;
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    protocols::webdav_create_dir(&self.profile, self.secret.as_deref(), &norm)?;
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    protocols::ftp_create_dir(&self.profile, self.secret.as_deref(), &norm)?;
                }
                ProtocolType::Smb => {
                    protocols::smb_create_dir(&self.profile, self.secret.as_deref(), &norm)?;
                }
            }
        }
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        let mut nodes = self.nodes.write().unwrap();
        nodes.insert(
            norm.clone(),
            RemoteNode {
                metadata: FileMetadata {
                    name,
                    path: norm,
                    extension: None,
                    kind: "Folder".to_string(),
                    size: None,
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
                    owner: Some(self.profile.username.clone()),
                    group: Some("staff".to_string()),
                },
                data: Vec::new(),
            },
        );
        Ok(())
    }

    fn remove(&self, path: &str, recursive: bool) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let esc = norm.replace('\'', "'\\''");
                    let flag = if recursive { "-rf" } else { "-f" };
                    self.run_remote_shell(&format!("rm {flag} -- '{esc}'"))?;
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    protocols::s3_remove(&self.profile, self.secret.as_deref(), &norm)?;
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    protocols::webdav_remove(&self.profile, self.secret.as_deref(), &norm)?;
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    let is_dir = self
                        .nodes
                        .read()
                        .unwrap()
                        .get(&norm)
                        .is_some_and(|n| n.metadata.is_dir);
                    protocols::ftp_remove(&self.profile, self.secret.as_deref(), &norm, is_dir)?;
                }
                ProtocolType::Smb => {
                    protocols::smb_remove(&self.profile, self.secret.as_deref(), &norm, recursive)?;
                }
            }
        }
        let mut nodes = self.nodes.write().unwrap();
        if !self.live_mode && !nodes.contains_key(&norm) {
            return Err(VfsError::NotFound(norm));
        }
        if recursive {
            let prefix = format!("{norm}/");
            nodes.retain(|k, _| k != &norm && !k.starts_with(&prefix));
        } else {
            nodes.remove(&norm);
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let src = Self::normalize_remote_path(from)?;
        let dst = Self::normalize_remote_path(to)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let esc_s = src.replace('\'', "'\\''");
                    let esc_d = dst.replace('\'', "'\\''");
                    self.run_remote_shell(&format!("mv -f -- '{esc_s}' '{esc_d}'"))?;
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    protocols::webdav_move_or_copy(
                        &self.profile,
                        self.secret.as_deref(),
                        &src,
                        &dst,
                        true,
                    )?;
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    protocols::ftp_rename(&self.profile, self.secret.as_deref(), &src, &dst)?;
                }
                ProtocolType::Smb => {
                    protocols::smb_rename(&self.profile, self.secret.as_deref(), &src, &dst)?;
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    let bytes = self.read_bytes(&src)?;
                    self.write_bytes(&dst, &bytes)?;
                    self.remove(&src, false)?;
                }
            }
        }
        let mut nodes = self.nodes.write().unwrap();
        if let Some(mut node) = nodes.remove(&src) {
            node.metadata.name = dst.rsplit('/').next().unwrap_or(&dst).to_string();
            node.metadata.path = dst.clone();
            nodes.insert(dst, node);
        } else if !self.live_mode {
            return Err(VfsError::NotFound(src));
        }
        Ok(())
    }

    fn copy_file(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let src = Self::normalize_remote_path(from)?;
        let dst = Self::normalize_remote_path(to)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let esc_s = src.replace('\'', "'\\''");
                    let esc_d = dst.replace('\'', "'\\''");
                    self.run_remote_shell(&format!("cp -RPp -- '{esc_s}' '{esc_d}'"))?;
                    return Ok(());
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    return protocols::webdav_move_or_copy(
                        &self.profile,
                        self.secret.as_deref(),
                        &src,
                        &dst,
                        false,
                    );
                }
                _ => {
                    let bytes = self.read_bytes(&src)?;
                    return self.write_bytes(&dst, &bytes);
                }
            }
        }
        let mut nodes = self.nodes.write().unwrap();
        let mut node = nodes.get(&src).cloned().ok_or(VfsError::NotFound(src))?;
        node.metadata.name = dst.rsplit('/').next().unwrap_or(&dst).to_string();
        node.metadata.path = dst.clone();
        nodes.insert(dst, node);
        Ok(())
    }

    fn read_bytes(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let (base_args, target) = self.ssh_base_args(true)?;
                    let (tmp_file, _f) =
                        create_secure_temp_file("remote_io", "manifold-dl", ".tmp")?;
                    let tmp_str = safe_local_scp_arg(&tmp_file);
                    let mut cmd_vec = vec!["scp".to_string(), "-q".to_string()];
                    cmd_vec.extend(base_args);
                    cmd_vec.push("--".to_string());
                    cmd_vec.push(format!("{target}:{norm}"));
                    cmd_vec.push(tmp_str);
                    let res = self.exec_pty_cmd_with_timeout(&cmd_vec, "30.0");
                    if let Err(e) = res {
                        let _ = fs::remove_file(&tmp_file);
                        return Err(e);
                    }
                    let bytes = fs::read(&tmp_file)?;
                    let _ = fs::remove_file(&tmp_file);
                    return Ok(bytes);
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    return protocols::s3_read_bytes(&self.profile, self.secret.as_deref(), &norm);
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    return protocols::webdav_read_bytes(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                    );
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    return protocols::ftp_read_bytes(&self.profile, self.secret.as_deref(), &norm);
                }
                ProtocolType::Smb => {
                    return protocols::smb_read_bytes(&self.profile, self.secret.as_deref(), &norm);
                }
            }
        }
        let nodes = self.nodes.read().unwrap();
        let found = nodes.get(&norm).map(|n| n.data.clone());
        found.ok_or(VfsError::NotFound(norm))
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_mode {
            match self.profile.protocol {
                ProtocolType::Sftp => {
                    let (base_args, target) = self.ssh_base_args(true)?;
                    let (tmp_file, mut f) =
                        create_secure_temp_file("remote_io", "manifold-ul", ".tmp")?;
                    f.write_all(data)?;
                    f.sync_all()?;
                    drop(f);
                    let tmp_str = safe_local_scp_arg(&tmp_file);
                    let mut cmd_vec = vec!["scp".to_string(), "-q".to_string()];
                    cmd_vec.extend(base_args);
                    cmd_vec.push("--".to_string());
                    cmd_vec.push(tmp_str);
                    cmd_vec.push(format!("{target}:{norm}"));
                    let res = self.exec_pty_cmd_with_timeout(&cmd_vec, "30.0");
                    let _ = fs::remove_file(&tmp_file);
                    res?;
                    return Ok(());
                }
                ProtocolType::S3 | ProtocolType::Gcs | ProtocolType::AzureBlob => {
                    return protocols::s3_write_bytes(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        data,
                    );
                }
                ProtocolType::WebDav | ProtocolType::GoogleDrive => {
                    return protocols::webdav_write_bytes(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        data,
                    );
                }
                ProtocolType::Ftp | ProtocolType::Ftps => {
                    return protocols::ftp_write_bytes(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        data,
                    );
                }
                ProtocolType::Smb => {
                    return protocols::smb_write_bytes(
                        &self.profile,
                        self.secret.as_deref(),
                        &norm,
                        data,
                    );
                }
            }
        }
        if let Some((parent, _)) = norm.rsplit_once('/') {
            let parent_norm = if parent.is_empty() { "/" } else { parent };
            if !self.nodes.read().unwrap().contains_key(parent_norm) {
                self.create_dir(parent_norm)?;
            }
        }
        let name = norm.rsplit('/').next().unwrap_or(&norm).to_string();
        let (extension, kind) = manifold_vfs::infer_file_kind(&name, false);
        let mut nodes = self.nodes.write().unwrap();
        nodes.insert(
            norm.clone(),
            RemoteNode {
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
                    owner: Some(self.profile.username.clone()),
                    group: Some("staff".to_string()),
                },
                data: data.to_vec(),
            },
        );
        Ok(())
    }

    fn append_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_mode {
            if self.profile.protocol == ProtocolType::Sftp {
                let seq = SSH_SENTINEL_SEQ.fetch_add(1, Ordering::Relaxed);
                let chunk_remote = format!("{norm}.__mchunk_{}_{seq}", std::process::id());
                let (base_args, target) = self.ssh_base_args(true)?;
                let (tmp_file, mut f) =
                    create_secure_temp_file("remote_io", "manifold-ap", ".tmp")?;
                f.write_all(data)?;
                f.sync_all()?;
                drop(f);
                let tmp_str = safe_local_scp_arg(&tmp_file);
                let mut cmd_vec = vec!["scp".to_string(), "-q".to_string()];
                cmd_vec.extend(base_args);
                cmd_vec.push("--".to_string());
                cmd_vec.push(tmp_str);
                cmd_vec.push(format!("{target}:{chunk_remote}"));
                let up_res = self.exec_pty_cmd_with_timeout(&cmd_vec, "120.0");
                let _ = fs::remove_file(&tmp_file);
                up_res?;
                let esc_dst = norm.replace('\'', "'\\''");
                let esc_chk = chunk_remote.replace('\'', "'\\''");
                let sh = format!(
                    "cat -- '{esc_chk}' >> '{esc_dst}'; st=$?; rm -f -- '{esc_chk}'; exit $st"
                );
                return self.run_remote_shell(&sh).map(|_| ());
            }
            let mut existing = self.read_bytes(&norm).unwrap_or_default();
            existing.extend_from_slice(data);
            return self.write_bytes(&norm, &existing);
        }
        let mut nodes = self.nodes.write().unwrap();
        if let Some(node) = nodes.get_mut(&norm) {
            node.data.extend_from_slice(data);
            node.metadata.size = Some(node.data.len() as u64);
            node.metadata.modified = Some(SystemTime::now());
            return Ok(());
        }
        drop(nodes);
        self.write_bytes(&norm, data)
    }

    fn create_symlink(&self, target: &str, link_path: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        if target.is_empty() || target.contains(['\0', '\r', '\n']) {
            return Err(VfsError::InvalidPath("Unsafe symlink target".to_string()));
        }
        let link_norm = Self::normalize_remote_path(link_path)?;
        if self.live_mode {
            if self.profile.protocol != ProtocolType::Sftp {
                let msg = format!("create_symlink unsupported on {}", self.scheme());
                return Err(VfsError::UnsupportedOperation(msg));
            }
            let esc_target = target.replace('\'', "'\\''");
            let esc_link = link_norm.replace('\'', "'\\''");
            self.run_remote_shell(&format!("ln -s -- '{esc_target}' '{esc_link}'"))?;
        }
        let name = link_norm
            .rsplit('/')
            .next()
            .unwrap_or(&link_norm)
            .to_string();
        let (extension, kind) = manifold_vfs::infer_file_kind(&name, false);
        let mut nodes = self.nodes.write().unwrap();
        nodes.insert(
            link_norm.clone(),
            RemoteNode {
                metadata: FileMetadata {
                    is_hidden: name.starts_with('.'),
                    name,
                    path: link_norm,
                    extension,
                    kind,
                    size: None,
                    is_dir: false,
                    is_symlink: true,
                    is_locked: false,
                    hide_extension: false,
                    color_label: ColorLabel::None,
                    modified: Some(SystemTime::now()),
                    created: Some(SystemTime::now()),
                    added: Some(SystemTime::now()),
                    permissions: 0o777,
                    owner: Some(self.profile.username.clone()),
                    group: Some("staff".to_string()),
                },
                data: Vec::new(),
            },
        );
        Ok(())
    }

    fn set_permissions(&self, path: &str, mode: u32) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.is_live_ssh() {
            let esc = norm.replace('\'', "'\\''");
            let oct = format!("{:o}", mode & 0o7777);
            self.run_remote_shell(&format!("chmod {oct} -- '{esc}'"))?;
        }
        let mut nodes = self.nodes.write().unwrap();
        if let Some(node) = nodes.get_mut(&norm) {
            node.metadata.permissions = mode & 0o7777;
        } else if !self.live_mode {
            return Err(VfsError::NotFound(norm));
        }
        Ok(())
    }

    fn set_color_label(&self, path: &str, label: ColorLabel) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        let mut nodes = self.nodes.write().unwrap();
        let node = nodes.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.color_label = label;
        Ok(())
    }

    fn set_hide_extension(&self, path: &str, hide: bool) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        let mut nodes = self.nodes.write().unwrap();
        let node = nodes.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.hide_extension = hide;
        Ok(())
    }

    fn set_locked(&self, path: &str, locked: bool) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        let mut nodes = self.nodes.write().unwrap();
        let node = nodes.get_mut(&norm).ok_or(VfsError::NotFound(norm))?;
        node.metadata.is_locked = locked;
        Ok(())
    }
}

fn parse_symbolic_permissions(mode_str: &str) -> u32 {
    let b = mode_str.as_bytes();
    if b.len() < 10 {
        return 0o644;
    }
    let mut mode = 0u32;
    if b[1] == b'r' {
        mode |= 0o400;
    }
    if b[2] == b'w' {
        mode |= 0o200;
    }
    match b[3] {
        b'x' => mode |= 0o100,
        b's' => mode |= 0o4100,
        b'S' => mode |= 0o4000,
        _ => {}
    }
    if b[4] == b'r' {
        mode |= 0o040;
    }
    if b[5] == b'w' {
        mode |= 0o020;
    }
    match b[6] {
        b'x' => mode |= 0o010,
        b's' => mode |= 0o2010,
        b'S' => mode |= 0o2000,
        _ => {}
    }
    if b[7] == b'r' {
        mode |= 0o004;
    }
    if b[8] == b'w' {
        mode |= 0o002;
    }
    match b[9] {
        b'x' => mode |= 0o001,
        b't' => mode |= 0o1001,
        b'T' => mode |= 0o1000,
        _ => {}
    }
    mode
}

fn parse_ls_datetime(month_str: &str, day_str: &str, time_or_year: &str) -> SystemTime {
    let month: u64 = match month_str {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return SystemTime::now(),
    };
    let Ok(day) = day_str.parse::<u64>() else {
        return SystemTime::now();
    };
    if !(1..=31).contains(&day) {
        return SystemTime::now();
    }
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_762_000_000);
    let approx_current_year = 1970 + (now_secs / 31_556_952);
    let (mut year, hour, minute, has_time) = if let Some((h_s, m_s)) = time_or_year.split_once(':')
    {
        let h = h_s.parse::<u64>().unwrap_or(0).min(23);
        let m = m_s.parse::<u64>().unwrap_or(0).min(59);
        (approx_current_year, h, m, true)
    } else if let Ok(y) = time_or_year.parse::<u64>() {
        (y.clamp(1970, 2200), 0, 0, false)
    } else {
        return SystemTime::now();
    };
    let compute_secs = |yr: u64| -> u64 {
        let days_before_month = [0u64, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
        let mut days = (yr - 1970) * 365 + (yr - 1969) / 4;
        days += days_before_month[(month - 1) as usize] + (day - 1);
        if month > 2 && yr.is_multiple_of(4) && (!yr.is_multiple_of(100) || yr.is_multiple_of(400))
        {
            days += 1;
        }
        days * 86_400 + hour * 3_600 + minute * 60
    };
    let mut total_secs = compute_secs(year);
    if has_time && total_secs > now_secs + 86_400 && year > 1970 {
        year -= 1;
        total_secs = compute_secs(year);
    }
    UNIX_EPOCH + Duration::from_secs(total_secs)
}

fn resolve_ls_owner(uid_token: &str, default_owner: &str, remote_uid: Option<u32>) -> String {
    if uid_token == "0" {
        "root".to_string()
    } else if !default_owner.is_empty()
        && (uid_token == default_owner
            || remote_uid.is_some_and(|u| uid_token.parse::<u32>().ok() == Some(u)))
    {
        default_owner.to_string()
    } else if uid_token.chars().all(|c| c.is_ascii_digit()) {
        format!("uid:{uid_token}")
    } else {
        uid_token.to_string()
    }
}

pub(crate) fn parse_ls_aln_output(
    dir_norm: &str,
    raw: &str,
    default_owner: &str,
    remote_uid: Option<u32>,
) -> Vec<FileMetadata> {
    let mut entries = Vec::new();
    let trimmed_dir = dir_norm.trim_end_matches('/');
    for line in raw.lines() {
        let line = line.trim_end_matches('\r').trim_start();
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        let mut idx = 0usize;
        let bytes = line.as_bytes();
        let len = bytes.len();
        let mut tokens = Vec::with_capacity(8);
        while tokens.len() < 8 && idx < len {
            while idx < len && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            let start = idx;
            while idx < len && !bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if start < idx {
                tokens.push(&line[start..idx]);
            }
        }
        if tokens.len() < 8 {
            continue;
        }
        while idx < len && bytes[idx] == b' ' {
            idx += 1;
        }
        let raw_name = &line[idx..];
        let mode_str = tokens[0];
        let first_ch = mode_str.chars().next().unwrap_or('-');
        if !matches!(first_ch, '-' | 'd' | 'l' | 'b' | 'c' | 'p' | 's') {
            continue;
        }
        let is_symlink = first_ch == 'l';
        let name = if is_symlink {
            raw_name
                .split_once(" -> ")
                .map(|(n, _)| n)
                .unwrap_or(raw_name)
                .to_string()
        } else {
            raw_name.to_string()
        };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let is_dir = first_ch == 'd';
        let size_val = tokens[4].parse::<u64>().ok();
        let parsed_time = parse_ls_datetime(tokens[5], tokens[6], tokens[7]);
        let (extension, kind) = manifold_vfs::infer_file_kind(&name, is_dir);
        let full_path = format!("{trimmed_dir}/{name}");
        entries.push(FileMetadata {
            is_hidden: name.starts_with('.'),
            name,
            path: full_path,
            extension,
            kind,
            size: if is_dir { None } else { size_val },
            is_dir,
            is_symlink,
            is_locked: false,
            hide_extension: false,
            color_label: ColorLabel::None,
            modified: Some(parsed_time),
            created: Some(parsed_time),
            added: Some(parsed_time),
            permissions: parse_symbolic_permissions(mode_str),
            owner: Some(resolve_ls_owner(tokens[2], default_owner, remote_uid)),
            group: Some(tokens[3].to_string()),
        });
    }
    entries
}

fn remote_wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn remote_name_matches_query(name: &str, q_low: &str) -> bool {
    if q_low.is_empty() {
        return true;
    }
    let name_low = name.to_lowercase();
    if q_low.contains('*') || q_low.contains('?') {
        let anchored = format!("*{q_low}*");
        remote_wildcard_match(q_low, &name_low) || remote_wildcard_match(&anchored, &name_low)
    } else if q_low.contains(' ') {
        q_low.split_whitespace().all(|tok| name_low.contains(tok))
    } else {
        name_low.contains(q_low)
    }
}

fn parse_ls_ldn_find_output(
    base_norm: &str,
    raw: &str,
    default_owner: &str,
    remote_uid: Option<u32>,
    show_hidden: bool,
) -> Vec<FileMetadata> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r').trim_start();
        if line.is_empty() || line.starts_with("total ") {
            continue;
        }
        let mut idx = 0usize;
        let bytes = line.as_bytes();
        let len = bytes.len();
        let mut tokens = Vec::with_capacity(8);
        while tokens.len() < 8 && idx < len {
            while idx < len && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            let start = idx;
            while idx < len && !bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            if start < idx {
                tokens.push(&line[start..idx]);
            }
        }
        if tokens.len() < 8 {
            continue;
        }
        while idx < len && bytes[idx] == b' ' {
            idx += 1;
        }
        let raw_path = &line[idx..];
        let mode_str = tokens[0];
        let first_ch = mode_str.chars().next().unwrap_or('-');
        if !matches!(first_ch, '-' | 'd' | 'l' | 'b' | 'c' | 'p' | 's') {
            continue;
        }
        let is_symlink = first_ch == 'l';
        let full_path_raw = if is_symlink {
            raw_path
                .split_once(" -> ")
                .map(|(n, _)| n)
                .unwrap_or(raw_path)
        } else {
            raw_path
        };
        let Ok(full_path) = RemoteVfsBackend::normalize_remote_path(full_path_raw) else {
            continue;
        };
        if full_path == base_norm {
            continue;
        }
        let name = full_path.rsplit('/').next().unwrap_or("").to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let is_hidden = name.starts_with('.');
        if is_hidden && !show_hidden {
            continue;
        }
        let is_dir = first_ch == 'd';
        let size_val = tokens[4].parse::<u64>().ok();
        let parsed_time = parse_ls_datetime(tokens[5], tokens[6], tokens[7]);
        let (extension, kind) = manifold_vfs::infer_file_kind(&name, is_dir);
        entries.push(FileMetadata {
            is_hidden,
            name,
            path: full_path,
            extension,
            kind,
            size: if is_dir { None } else { size_val },
            is_dir,
            is_symlink,
            is_locked: false,
            hide_extension: false,
            color_label: ColorLabel::None,
            modified: Some(parsed_time),
            created: Some(parsed_time),
            added: Some(parsed_time),
            permissions: parse_symbolic_permissions(mode_str),
            owner: Some(resolve_ls_owner(tokens[2], default_owner, remote_uid)),
            group: Some(tokens[3].to_string()),
        });
    }
    entries
}

/// Staged file entry managed by [`RemoteEditStagingManager`] (`Section 3.7` of `BRIEF.md`).
#[derive(Debug, Clone)]
pub struct StagedRemoteFile {
    pub session_id: String,
    pub remote_path: String,
    pub local_staging_path: PathBuf,
    pub last_sha256: [u8; 32],
}

/// Implements Remote File Editing (Edit-in-Place) using
/// `$XDG_CACHE_HOME/manifold/staging/<session_id>/<filename>`.
pub struct RemoteEditStagingManager {
    cache_root: PathBuf,
    staged_files: Arc<RwLock<Vec<StagedRemoteFile>>>,
}

impl RemoteEditStagingManager {
    pub fn new(custom_cache_base: Option<PathBuf>) -> Self {
        let cache_root = if let Some(base) = custom_cache_base {
            base.join("manifold").join("staging")
        } else {
            secure_runtime_subdir("staging").unwrap_or_else(|_| {
                let env_dir = std::env::var_os("XDG_CACHE_HOME")
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| PathBuf::from(h).join(".cache").into_os_string())
                    })
                    .map(PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir);
                env_dir.join("manifold").join("staging")
            })
        };
        Self {
            cache_root,
            staged_files: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Downloads `remote_path` from `backend` into `$XDG_CACHE_HOME/manifold/staging/<session_id>/<filename>`.
    pub fn stage_for_external_edit(
        &self,
        session_id: &str,
        remote_path: &str,
        backend: &dyn FileSystemBackend,
    ) -> Result<PathBuf, VfsError> {
        if session_id.contains("..") || session_id.contains('/') || session_id.contains('\\') {
            return Err(VfsError::InvalidPath("Invalid session_id".to_string()));
        }
        let raw_name = Path::new(remote_path).file_name().and_then(|n| n.to_str());
        let filename = raw_name.ok_or_else(|| VfsError::InvalidPath(remote_path.to_string()))?;

        let session_dir = self.cache_root.join(session_id);
        fs::create_dir_all(&session_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o700));
        }

        let local_staging_path = session_dir.join(filename);
        let bytes = backend.read_bytes(remote_path)?;
        fs::write(&local_staging_path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&local_staging_path, fs::Permissions::from_mode(0o600));
        }

        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let mut guard = self.staged_files.write().unwrap();
        if let Some(existing) = guard
            .iter_mut()
            .find(|s| s.session_id == session_id && s.remote_path == remote_path)
        {
            existing.local_staging_path = local_staging_path.clone();
            existing.last_sha256 = digest;
        } else {
            guard.push(StagedRemoteFile {
                session_id: session_id.to_string(),
                remote_path: remote_path.to_string(),
                local_staging_path: local_staging_path.clone(),
                last_sha256: digest,
            });
        }

        Ok(local_staging_path)
    }

    /// Checks all staged files for local edits and automatically re-uploads modified content to `backend`.
    pub fn sync_modified_files(&self, backend: &dyn FileSystemBackend) -> Result<usize, VfsError> {
        self.sync_modified_files_for_session(None, backend)
    }

    /// Checks staged files for a specific `session_id` (or all sessions if `None`) and uploads modified files.
    pub fn sync_modified_files_for_session(
        &self,
        session_id: Option<&str>,
        backend: &dyn FileSystemBackend,
    ) -> Result<usize, VfsError> {
        let mut guard = self.staged_files.write().unwrap();
        let mut uploaded = 0usize;
        for staged in guard.iter_mut() {
            if session_id.is_some_and(|sid| staged.session_id != sid) {
                continue;
            }
            if staged.local_staging_path.exists() {
                let current_bytes = fs::read(&staged.local_staging_path)?;
                let current_hash: [u8; 32] = Sha256::digest(&current_bytes).into();
                if current_hash != staged.last_sha256 {
                    backend.write_bytes(&staged.remote_path, &current_bytes)?;
                    staged.last_sha256 = current_hash;
                    uploaded += 1;
                }
            }
        }
        Ok(uploaded)
    }

    /// Returns all active session IDs that currently have staged files.
    pub fn active_session_ids(&self) -> Vec<String> {
        let guard = self.staged_files.read().unwrap();
        let mut ids = Vec::new();
        for item in guard.iter() {
            if !ids.contains(&item.session_id) {
                ids.push(item.session_id.clone());
            }
        }
        ids
    }

    /// Purges all staged files for a given session or on application shutdown.
    pub fn purge_session(&self, session_id: &str) -> Result<(), VfsError> {
        let mut guard = self.staged_files.write().unwrap();
        guard.retain(|f| f.session_id != session_id);
        let session_dir = self.cache_root.join(session_id);
        if session_dir.exists() {
            fs::remove_dir_all(&session_dir)?;
        }
        Ok(())
    }
}

/// Terminal launch specification (`Section 3.8` of `BRIEF.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: Option<PathBuf>,
}

/// Constructs validated local shell and remote SSH terminal launch commands.
pub struct TerminalIntegration;

impl TerminalIntegration {
    /// Builds the command to open a local shell in `local_dir`.
    pub fn build_local_terminal(
        local_dir: impl AsRef<Path>,
    ) -> Result<TerminalCommandSpec, VfsError> {
        let dir = local_dir.as_ref();
        if !dir.exists() {
            return Err(VfsError::NotFound(dir.display().to_string()));
        }
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Ok(TerminalCommandSpec {
            program: shell,
            args: vec!["-l".to_string()],
            working_dir: Some(dir.to_path_buf()),
        })
    }

    /// Builds the SSH command for a remote SFTP connection profile (`Section 3.8` of `BRIEF.md`):
    /// `ssh -p {port} [-i {key}] -t -- {user}@{host} "cd '{remote_path}'; exec $SHELL -l"`
    pub fn build_remote_ssh_terminal(
        profile: &ConnectionProfile,
        remote_path: &str,
    ) -> Result<TerminalCommandSpec, VfsError> {
        if profile.protocol != ProtocolType::Sftp {
            let msg = "Remote SSH terminal requires an SFTP connection profile".to_string();
            return Err(VfsError::UnsupportedOperation(msg));
        }
        let missing_host = || VfsError::InvalidPath("Missing SFTP host".to_string());
        let host = profile.host.as_deref().ok_or_else(missing_host)?.trim();
        let user = profile.username.trim();
        validate_ssh_endpoint(host, user, profile.key_path.as_deref())?;

        let safe_remote_path = remote_path.replace('\'', "'\\''");
        let port = profile.port.unwrap_or(22).to_string();

        let mut args = vec!["-p".to_string(), port];
        if let Some(key) = &profile.key_path {
            let trimmed_key = key.trim();
            if !trimmed_key.is_empty() {
                args.push("-i".to_string());
                args.push(trimmed_key.to_string());
            }
        }
        args.push("-t".to_string());
        args.push("--".to_string());
        args.push(format!("{user}@{host}"));
        args.push(format!("cd '{safe_remote_path}'; exec $SHELL -l"));

        Ok(TerminalCommandSpec {
            program: "ssh".to_string(),
            args,
            working_dir: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_sftp_backend_and_edit_in_place_staging() {
        let profile = ConnectionProfile::new_sftp_default("sess-1", "Website SFTP");
        let remote = RemoteVfsBackend::connect(profile).unwrap();

        let php_path = "/User/Shared/Website/config-sample.php";
        remote.write_bytes(php_path, b"<?php // v1").unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let staging = RemoteEditStagingManager::new(Some(tmp.path().to_path_buf()));
        assert!(staging.cache_root().ends_with("manifold/staging"));

        let stage_res = staging.stage_for_external_edit("sess-1", php_path, &remote);
        let staged_path = stage_res.unwrap();
        assert!(staged_path.exists());

        // Simulate external editor saving modified content
        fs::write(&staged_path, b"<?php // v2 updated").unwrap();
        let uploaded_count = staging.sync_modified_files(&remote).unwrap();
        assert_eq!(uploaded_count, 1);

        let remote_updated = remote.read_bytes(php_path).unwrap();
        assert_eq!(remote_updated, b"<?php // v2 updated");

        staging.purge_session("sess-1").unwrap();
        assert!(!staged_path.exists());
    }

    #[test]
    fn test_remote_ssh_terminal_command_builder() {
        let mut profile = ConnectionProfile::new_sftp_default("id-1", "Staging SFTP");
        profile.key_path = Some("~/.ssh/id_ed25519".to_string());

        let cmd =
            TerminalIntegration::build_remote_ssh_terminal(&profile, "/var/www/html").unwrap();
        assert_eq!(cmd.program, "ssh");
        assert_eq!(
            cmd.args,
            vec![
                "-p",
                "22",
                "-i",
                "~/.ssh/id_ed25519",
                "-t",
                "--",
                "admin@johnappleseed.com",
                "cd '/var/www/html'; exec $SHELL -l"
            ]
        );
    }

    #[test]
    fn test_sec01_ssh_option_injection_rejected() {
        let mut profile = ConnectionProfile::new_sftp_default("id-sec", "Malicious Host");
        profile.host = Some("-oProxyCommand=sh".to_string());
        assert!(RemoteVfsBackend::connect(profile.clone()).is_err());
        assert!(TerminalIntegration::build_remote_ssh_terminal(&profile, "/").is_err());

        let mut profile_user = ConnectionProfile::new_sftp_default("id-sec2", "Malicious User");
        profile_user.username = "-oLocalCommand=id".to_string();
        assert!(RemoteVfsBackend::connect(profile_user).is_err());
    }

    #[test]
    fn test_ui06_double_dot_filename_and_owner_resolution() {
        let norm = RemoteVfsBackend::normalize_remote_path("/releases/v1..v2.patch").unwrap();
        assert_eq!(norm, "/releases/v1..v2.patch");
        let trailing = RemoteVfsBackend::normalize_remote_path("/releases/note ").unwrap();
        assert_eq!(trailing, "/releases/note ");
        assert!(RemoteVfsBackend::normalize_remote_path("/releases/../etc/passwd").is_err());
        assert!(RemoteVfsBackend::normalize_remote_path("/releases/bad\nname").is_err());
        assert_eq!(resolve_ls_owner("0", "admin", Some(1000)), "root");
        assert_eq!(resolve_ls_owner("1000", "admin", Some(1000)), "admin");
        assert_eq!(resolve_ls_owner("1000", "admin", Some(2005)), "uid:1000");
        assert_eq!(resolve_ls_owner("33", "admin", Some(1000)), "uid:33");
        assert_eq!(parse_symbolic_permissions("-rwsr-sr-t"), 0o7755);
        assert_eq!(parse_symbolic_permissions("-rwSr-Sr-T"), 0o7644);
    }

    #[test]
    fn test_gap01_s3_sigv4_and_xml_parsers() {
        let headers = protocols::build_s3_sigv4_headers(&protocols::S3SigV4Params {
            method: "GET",
            host_header: "mybucket.s3.us-east-1.amazonaws.com",
            canonical_uri: "/",
            canonical_query: "delimiter=%2F&list-type=2&prefix=",
            region: "us-east-1",
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            payload_sha256_hex: &protocols::sha256_hex(b""),
            amz_date: "20261110T120000Z",
        });
        assert_eq!(headers.len(), 4);
        assert!(headers[3].contains(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20261110/us-east-1/s3/aws4_request"
        ));

        let s3_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <ListBucketResult>
            <CommonPrefixes><Prefix>photos/</Prefix></CommonPrefixes>
            <Contents><Key>readme.md</Key><Size>1024</Size></Contents>
        </ListBucketResult>"#;
        let s3_items = protocols::parse_s3_list_xml(s3_xml, "/", "", "admin");
        assert_eq!(s3_items.len(), 2);
        assert!(s3_items.iter().any(|i| i.name == "photos" && i.is_dir));
        assert!(s3_items
            .iter()
            .any(|i| i.name == "readme.md" && !i.is_dir && i.size == Some(1024)));

        let dav_xml = concat!(
            "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n",
            "<d:multistatus xmlns:d=\"DAV:\">\n",
            "<d:response><d:href>/dav/</d:href><d:propstat><d:prop>",
            "<d:resourcetype><d:collection/></d:resourcetype>",
            "</d:prop></d:propstat></d:response>\n",
            "<d:response><d:href>/dav/docs/</d:href><d:propstat><d:prop>",
            "<d:resourcetype><d:collection/></d:resourcetype>",
            "</d:prop></d:propstat></d:response>\n",
            "<d:response><d:href>/dav/report.pdf</d:href><d:propstat><d:prop>",
            "<d:getcontentlength>4096</d:getcontentlength><d:resourcetype/>",
            "</d:prop></d:propstat></d:response>\n",
            "</d:multistatus>"
        );
        let dav_items = protocols::parse_webdav_propfind_xml(dav_xml, "/dav", "alice");
        assert_eq!(dav_items.len(), 2);
        assert!(dav_items.iter().any(|i| i.name == "docs" && i.is_dir));
        assert!(dav_items
            .iter()
            .any(|i| i.name == "report.pdf" && !i.is_dir && i.size == Some(4096)));

        let smb_out = concat!(
            "  .                                   D        0  Wed Nov 10 12:00:00 2026\n",
            "  ..                                  D        0  Wed Nov 10 12:00:00 2026\n",
            "  Project Docs                        D        0  Wed Nov 10 12:00:00 2026\n",
            "  Q4 Budget Report.xlsx               A    18432  Wed Nov 10 12:00:00 2026\n",
            "                10485760 blocks of size 1024. 5242880 blocks available\n"
        );
        let smb_items = protocols::parse_smbclient_ls_output(smb_out, "/Share", "alice");
        assert_eq!(smb_items.len(), 2);
        assert!(smb_items
            .iter()
            .any(|i| i.name == "Project Docs" && i.is_dir));
        assert!(smb_items
            .iter()
            .any(|i| { i.name == "Q4 Budget Report.xlsx" && !i.is_dir && i.size == Some(18432) }));
    }
}
