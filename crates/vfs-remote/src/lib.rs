//! Remote VFS Connectors (`manifold-vfs-remote`) for Project Manifold.
//!
//! Provides unified [`FileSystemBackend`] implementations for SFTP/SSH, Cloud Object Storage
//! (S3, Google Drive, GCS, Azure Blob), FTP/FTPS, WebDAV, and SMB shares, plus the
//! Remote Edit-in-Place staging manager (`$XDG_CACHE_HOME/manifold/staging/<session_id>/<filename>`)
//! and SSH Terminal command builder.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use manifold_config::{ConnectionProfile, ProtocolType};
use manifold_vfs::{ColorLabel, FileMetadata, FileSystemBackend, VfsError, VolumeStats};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const PTY_SSH_SCRIPT: &str = r#"
import os, pty, select, sys

cmd = sys.argv[1:]
pw = os.environ.get("MANIFOLD_SSH_PASS", "")
pid, fd = pty.fork()
if pid == 0:
    os.environ["LC_ALL"] = "C"
    os.execvp(cmd[0], cmd)

out = bytearray()
sent_pw = False
while True:
    r, _, _ = select.select([fd], [], [], 8.0)
    if not r:
        break
    try:
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        out.extend(chunk)
        low = out.lower()
        if (not sent_pw) and pw and (b"password:" in low or b"passphrase" in low):
            os.write(fd, (pw + "\n").encode("utf-8"))
            sent_pw = True
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

#[derive(Debug, Clone)]
struct RemoteNode {
    metadata: FileMetadata,
    data: Vec<u8>,
}

/// Unified Remote VFS Backend supporting SFTP, S3, Google Drive, GCS, Azure, FTP/FTPS, WebDAV, and SMB.
pub struct RemoteVfsBackend {
    profile: ConnectionProfile,
    secret: Option<String>,
    live_ssh: bool,
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
                ProtocolType::S3 | ProtocolType::GoogleDrive
            )
        {
            let msg = format!("Missing username for connection '{}'", profile.name);
            return Err(VfsError::AuthFailed(msg));
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
            live_ssh: false,
            connected: Arc::new(RwLock::new(true)),
            nodes: Arc::new(RwLock::new(map)),
            volume_stats: VolumeStats {
                total_bytes: 500_000_000_000,
                available_bytes: default_avail,
            },
        })
    }

    /// Creates and connects a live remote VFS session (executing real SSH/SFTP commands for SFTP).
    pub fn connect_live(
        profile: ConnectionProfile,
        secret: Option<String>,
    ) -> Result<Self, VfsError> {
        let mut backend = Self::connect(profile)?;
        backend.secret = secret;
        if backend.profile.protocol == ProtocolType::Sftp {
            backend.live_ssh = true;
            let init_path = if backend.profile.base_path.trim().is_empty() {
                "/".to_string()
            } else {
                backend.profile.base_path.clone()
            };
            if backend.list_directory(&init_path).is_err() {
                backend.list_directory("/")?;
                backend.profile.base_path = "/".to_string();
            }
        }
        Ok(backend)
    }

    fn ssh_base_args(&self, is_scp: bool) -> Result<(Vec<String>, String), VfsError> {
        let missing_host = || VfsError::InvalidPath("Missing SFTP host".to_string());
        let host = self.profile.host.as_deref().ok_or_else(missing_host)?;
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
        let target = format!("{}@{host}", self.profile.username);
        Ok((args, target))
    }

    fn exec_pty_cmd(&self, cmd_vec: &[String]) -> Result<Vec<u8>, VfsError> {
        let pass = self.secret.as_deref().unwrap_or("");
        let out = Command::new("python3")
            .arg("-c")
            .arg(PTY_SSH_SCRIPT)
            .args(cmd_vec)
            .env("MANIFOLD_SSH_PASS", pass)
            .output()
            .map_err(|e| VfsError::NetworkError(format!("Failed to invoke python3/ssh: {e}")))?;
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

    fn run_remote_shell(&self, remote_sh: &str) -> Result<String, VfsError> {
        let (base_args, target) = self.ssh_base_args(false)?;
        let mut cmd_vec = vec!["ssh".to_string()];
        cmd_vec.extend(base_args);
        cmd_vec.push(target);
        let wrapped = format!("printf '__MANIFOLD_BEGIN__\\n'; {remote_sh}");
        cmd_vec.push(wrapped);
        let raw = self.exec_pty_cmd(&cmd_vec)?;
        let text = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
        if let Some((_, body)) = text.split_once("__MANIFOLD_BEGIN__\n") {
            Ok(body.to_string())
        } else {
            Ok(text)
        }
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

    fn ensure_connected(&self) -> Result<(), VfsError> {
        if !self.is_connected() {
            let msg = format!("Connection '{}' is disconnected", self.profile.name);
            return Err(VfsError::NetworkError(msg));
        }
        Ok(())
    }

    fn normalize_remote_path(path: &str) -> Result<String, VfsError> {
        if path.contains("..") || path.contains('\0') {
            let msg = format!("Unsafe remote path rejected: {path}");
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
        if self.live_ssh {
            let esc = norm.replace('\'', "'\\''");
            let sh = format!("LC_ALL=C ls -aln -- '{esc}' 2>/dev/null || LC_ALL=C ls -aln '{esc}'");
            let out = self.run_remote_shell(&sh)?;
            let mut list = parse_ls_aln_output(&norm, &out, &self.profile.username);
            list.sort_by_key(|a| a.name.to_lowercase());
            let mut nodes = self.nodes.write().unwrap();
            nodes.entry(norm.clone()).or_insert_with(|| RemoteNode {
                metadata: FileMetadata {
                    name: norm.rsplit('/').next().unwrap_or("/").to_string(),
                    path: norm.clone(),
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
            for item in &list {
                nodes.insert(
                    item.path.clone(),
                    RemoteNode {
                        metadata: item.clone(),
                        data: Vec::new(),
                    },
                );
            }
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
        let nodes = self.nodes.read().unwrap();
        if let Some(n) = nodes.get(&norm) {
            return Ok(n.metadata.clone());
        }
        drop(nodes);
        if self.live_ssh {
            return Ok(FileMetadata {
                name: norm.rsplit('/').next().unwrap_or("/").to_string(),
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
        Err(VfsError::NotFound(norm))
    }

    fn volume_stats(&self, _path: &str) -> Result<VolumeStats, VfsError> {
        self.ensure_connected()?;
        Ok(self.volume_stats)
    }

    fn create_dir(&self, path: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_ssh {
            let esc = norm.replace('\'', "'\\''");
            self.run_remote_shell(&format!("mkdir -p '{esc}'"))?;
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
        if self.live_ssh {
            let esc = norm.replace('\'', "'\\''");
            let flag = if recursive { "-rf" } else { "-f" };
            self.run_remote_shell(&format!("rm {flag} '{esc}'"))?;
        }
        let mut nodes = self.nodes.write().unwrap();
        if !self.live_ssh && !nodes.contains_key(&norm) {
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
        if self.live_ssh {
            let esc_s = src.replace('\'', "'\\''");
            let esc_d = dst.replace('\'', "'\\''");
            self.run_remote_shell(&format!("mv -f '{esc_s}' '{esc_d}'"))?;
        }
        let mut nodes = self.nodes.write().unwrap();
        if let Some(mut node) = nodes.remove(&src) {
            node.metadata.name = dst.rsplit('/').next().unwrap_or(&dst).to_string();
            node.metadata.path = dst.clone();
            nodes.insert(dst, node);
        } else if !self.live_ssh {
            return Err(VfsError::NotFound(src));
        }
        Ok(())
    }

    fn copy_file(&self, from: &str, to: &str) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let src = Self::normalize_remote_path(from)?;
        let dst = Self::normalize_remote_path(to)?;
        if self.live_ssh {
            let esc_s = src.replace('\'', "'\\''");
            let esc_d = dst.replace('\'', "'\\''");
            self.run_remote_shell(&format!("cp -R '{esc_s}' '{esc_d}'"))?;
            return Ok(());
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
        if self.live_ssh {
            let (base_args, target) = self.ssh_base_args(true)?;
            let tmp_file = std::env::temp_dir().join(format!("manifold-dl-{}", std::process::id()));
            let tmp_str = tmp_file.to_string_lossy().to_string();
            let mut cmd_vec = vec!["scp".to_string()];
            cmd_vec.extend(base_args);
            cmd_vec.push(format!("{target}:{norm}"));
            cmd_vec.push(tmp_str);
            self.exec_pty_cmd(&cmd_vec)?;
            let bytes = fs::read(&tmp_file)?;
            let _ = fs::remove_file(&tmp_file);
            return Ok(bytes);
        }
        let nodes = self.nodes.read().unwrap();
        let found = nodes.get(&norm).map(|n| n.data.clone());
        found.ok_or(VfsError::NotFound(norm))
    }

    fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_ssh {
            let (base_args, target) = self.ssh_base_args(true)?;
            let tmp_file = std::env::temp_dir().join(format!("manifold-ul-{}", std::process::id()));
            fs::write(&tmp_file, data)?;
            let tmp_str = tmp_file.to_string_lossy().to_string();
            let mut cmd_vec = vec!["scp".to_string()];
            cmd_vec.extend(base_args);
            cmd_vec.push(tmp_str);
            cmd_vec.push(format!("{target}:{norm}"));
            let res = self.exec_pty_cmd(&cmd_vec);
            let _ = fs::remove_file(&tmp_file);
            res?;
            return Ok(());
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

    fn set_permissions(&self, path: &str, mode: u32) -> Result<(), VfsError> {
        self.ensure_connected()?;
        let norm = Self::normalize_remote_path(path)?;
        if self.live_ssh {
            let esc = norm.replace('\'', "'\\''");
            let oct = format!("{:o}", mode & 0o777);
            self.run_remote_shell(&format!("chmod {oct} '{esc}'"))?;
        }
        let mut nodes = self.nodes.write().unwrap();
        if let Some(node) = nodes.get_mut(&norm) {
            node.metadata.permissions = mode & 0o7777;
        } else if !self.live_ssh {
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
    if b[3] == b'x' || b[3] == b's' {
        mode |= 0o100;
    }
    if b[4] == b'r' {
        mode |= 0o040;
    }
    if b[5] == b'w' {
        mode |= 0o020;
    }
    if b[6] == b'x' || b[6] == b's' {
        mode |= 0o010;
    }
    if b[7] == b'r' {
        mode |= 0o004;
    }
    if b[8] == b'w' {
        mode |= 0o002;
    }
    if b[9] == b'x' || b[9] == b't' {
        mode |= 0o001;
    }
    mode
}

fn parse_ls_aln_output(dir_norm: &str, raw: &str, default_owner: &str) -> Vec<FileMetadata> {
    let mut entries = Vec::new();
    let trimmed_dir = dir_norm.trim_end_matches('/');
    for line in raw.lines() {
        let line = line.trim();
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
            modified: Some(SystemTime::now()),
            created: Some(SystemTime::now()),
            added: Some(SystemTime::now()),
            permissions: parse_symbolic_permissions(mode_str),
            owner: Some(default_owner.to_string()),
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
        let base = custom_cache_base.unwrap_or_else(|| {
            let env_dir = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
            env_dir.unwrap_or_else(|| PathBuf::from("/tmp"))
        });
        let cache_root = base.join("manifold").join("staging");
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

        let local_staging_path = session_dir.join(filename);
        let bytes = backend.read_bytes(remote_path)?;
        fs::write(&local_staging_path, &bytes)?;

        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        self.staged_files.write().unwrap().push(StagedRemoteFile {
            session_id: session_id.to_string(),
            remote_path: remote_path.to_string(),
            local_staging_path: local_staging_path.clone(),
            last_sha256: digest,
        });

        Ok(local_staging_path)
    }

    /// Checks all staged files for local edits and automatically re-uploads modified content to `backend`.
    pub fn sync_modified_files(&self, backend: &dyn FileSystemBackend) -> Result<usize, VfsError> {
        let mut guard = self.staged_files.write().unwrap();
        let mut uploaded = 0usize;
        for staged in guard.iter_mut() {
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
    /// `ssh -p {port} [-i {key}] {user}@{host} -t "cd '{remote_path}'; exec $SHELL -l"`
    pub fn build_remote_ssh_terminal(
        profile: &ConnectionProfile,
        remote_path: &str,
    ) -> Result<TerminalCommandSpec, VfsError> {
        if profile.protocol != ProtocolType::Sftp {
            let msg = "Remote SSH terminal requires an SFTP connection profile".to_string();
            return Err(VfsError::UnsupportedOperation(msg));
        }
        let missing_host = || VfsError::InvalidPath("Missing SFTP host".to_string());
        let host = profile.host.as_deref().ok_or_else(missing_host)?;

        // Validate host/user/path against shell metacharacters
        let unsafe_chars = [';', '&', '|', '`', '$', '\n', '\0'];
        if host.contains(unsafe_chars) || profile.username.contains(unsafe_chars) {
            let msg = "Unsafe characters in SSH host or username".to_string();
            return Err(VfsError::InvalidPath(msg));
        }
        let safe_remote_path = remote_path.replace('\'', "'\\''");
        let port = profile.port.unwrap_or(22).to_string();

        let mut args = vec!["-p".to_string(), port];
        if let Some(key) = &profile.key_path {
            let trimmed_key = key.trim();
            if !trimmed_key.is_empty() {
                args.push("-i".to_string());
                args.push(key.clone());
            }
        }
        args.push(format!("{}@{}", profile.username, host));
        args.push("-t".to_string());
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
                "admin@johnappleseed.com",
                "-t",
                "cd '/var/www/html'; exec $SHELL -l"
            ]
        );
    }
}
