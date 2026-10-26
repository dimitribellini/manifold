# Manifold: Cross-Platform Dual-Pane File Manager & Remote Transfer Engine

<div align="center">

[![GitHub](https://img.shields.io/badge/GITHUB-DIMITRIBELLINI%2FMANIFOLD-24292F?style=for-the-badge&logo=github&logoColor=white)](https://github.com/dimitribellini/manifold)
[![Release](https://img.shields.io/badge/RELEASE-V0.1.0-0A84FF?style=for-the-badge&logo=github&logoColor=white)](https://github.com/dimitribellini/manifold/releases)
[![Platforms](https://img.shields.io/badge/PLATFORMS-MACOS%20ARM64%20%7C%20LINUX%20X86__64-EE7F2D?style=for-the-badge&logo=apple&logoColor=white)](#quick-start)
[![Engine](https://img.shields.io/badge/ENGINE-100%25%20RUST%20%2B%20SLINT-8A2BE2?style=for-the-badge&logo=rust&logoColor=white)](#architecture--crates)
[![License](https://img.shields.io/badge/LICENSE-MIT-4CC61E?style=for-the-badge)](#license)

**Navigate** local volumes, archives (`.zip`, `.tar.gz`, `.7z`, `.rar`), and remote filesystems in a unified **Single or Dual-Pane Workspace**.<br/>
**Connect** to **SFTP/SSH, S3, SMB, FTP, and WebDAV** endpoints with native **OS Keychain / Secret Service** credential isolation.<br/>
**Transfer & Sync** files with **Universal Drag & Drop**, **Per-Tab Wildcard Search** (`*pdf`), **8-Color Tags**, and a **Prioritized Transfer Queue**.

📦 [**GitHub Releases**](https://github.com/dimitribellini/manifold/releases) · ⚡ [**Quick Start**](#quick-start) · ✨ [**Key Features**](#key-features) · 🔌 [**Remote Connections**](#remote-connections) · ⌨️ [**Keyboard Shortcuts**](#keyboard-shortcuts) · 🛠️ [**Build from Source**](#build-from-source)

</div>

---

<div align="center">
  <img src="screenshots/manifold_fig1.png" alt="Manifold Main Workspace" width="100%" />
</div>

---

## ⚡ Quick Start

Download the latest prebuilt release for **macOS (Apple Silicon)** or **Linux (x86_64)** from [**GitHub Releases**](https://github.com/dimitribellini/manifold/releases):

### macOS (Apple Silicon / ARM64)
```bash
curl -sSL https://github.com/dimitribellini/manifold/releases/latest/download/manifold-macos-arm64.tar.gz | tar -xz
mv Manifold.app /Applications/
open /Applications/Manifold.app
```

### Linux (x86_64)
```bash
curl -sSL https://github.com/dimitribellini/manifold/releases/latest/download/manifold-linux-x86_64.tar.gz | tar -xz
chmod +x manifold-linux-x86_64
sudo mv manifold-linux-x86_64 /usr/local/bin/manifold
manifold
```

---

## ✨ Key Features

- **Single & Dual-Pane Workspace**: Switch seamlessly between **Single Pane** and **Dual Pane** (horizontal or vertical split) with independent **List**, **Miller Columns**, and **Icon Grid** views per pane.
- **500k+ Row Virtualized Table**: `O(1)` viewport slicing renders massive directories smoothly alongside separate **Directories-First / Files-Second** sorting across `Name`, `Size`, `Date Modified`, and `Kind`.
- **Universal Drag & Drop**: Drag single or multi-selected files and folders with a live **Drag Ghost Pill HUD** onto any subfolder row, opposite pane, tab pill, breadcrumb bar, or Sidebar **Devices / Connections / Favorites** item.
- **Per-Tab Wildcard Search (`*` & `?`)**: Each tab maintains its own isolated filter (e.g. `*pdf`, `*.rs`) with a dedicated `Search: "<query>"` tab badge, result count banner, and one-click clear.
- **Rich 8-Color Tagging**: Tag files, folders, and remote connections with 8 color labels (`Red`, `Orange`, `Yellow`, `Green`, `Blue`, `Purple`, `Gray`, `None`) rendered with row tints, left accent bars, solid color-matched folder icons, and `[ ● Color ]` pill badges.
- **Real-Time Tasks & Inspector Drawer**: Monitor active transfers and operations from the top toolbar **Tasks & Operations** popover (`⇅`), or open the 3-tab right inspector (`ⓘ Info` with `R/W/X` + `Octal 644` permissions matrix & Hex/Code preview, `⇅ Tasks`, and `⚠ Logs`).
- **Archive Virtualization & Directory Sync**: Browse `.zip`, `.tar.*`, `.7z`, and `.rar` archives inline as virtual folders, run **SHA-256 / Metadata / Binary Diff** directory synchronization, and apply multi-rule **Batch Renaming**.

---

## 🔌 Remote Connections

Click the toolbar **World (`🌐`)** icon to open the protocol-aware connection manager, or right-click any saved server under **Connections** in the sidebar to edit, duplicate, reconnect, or launch an interactive **SSH Terminal**:

<div align="center">
  <img src="screenshots/manifold_fig2.png" alt="Manifold New SFTP Connection Modal" width="100%" />
</div>

- **Supported Protocols**: `SFTP / SSH` (password or SSH private key), `Amazon S3` (and S3-compatible endpoints), `SMB`, `FTP / FTPS`, and `WebDAV`.
- **Zero Plaintext Secrets**: Connection profiles are saved in `~/.config/manifold/connections.toml`, while passwords and private key passphrases are stored exclusively in your **OS Keychain** (`macOS Keychain` / Linux `libsecret` under `service = "manifold"`).

---

## ⌨️ Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Cmd/Ctrl + F` | Focus per-tab Filter / Wildcard Search (`*pdf`) |
| `Cmd/Ctrl + L` | Toggle canonical Path Bar edit mode (`Tab` auto-complete) |
| `Cmd/Ctrl + T` | Open selected folder in a new Tab |
| `Cmd/Ctrl + A` | Select All items in active pane |
| `Shift + Click` / `Cmd/Ctrl + Click` | Range selection / Multi-item toggle selection |
| `Cmd/Ctrl + C` / `X` / `V` | Copy / Cut / Paste selected items |
| `Cmd/Ctrl + D` | Duplicate selected items |
| `Cmd/Ctrl + N` | Create New Folder |
| `Cmd/Ctrl + R` or `F2` | Rename selected item |
| `Cmd/Ctrl + Shift + C` | Copy full path / URI to clipboard |
| `Cmd/Ctrl + I` | Toggle Right Inspector Drawer (`Info / Tasks / Logs`) |
| `Delete` / `Backspace` | Move selected items to Trash (with confirmation) |
| `Shift + Delete` | Permanently delete selected items (with confirmation) |
| `Alt + Left` / `Alt + Right` | Navigate Back / Forward in tab history |

---

## 🛠️ Build from Source

### Prerequisites
- **Rust**: Stable toolchain (`1.80+`, 2021 edition)
- **Linux Development Packages**:
  ```bash
  sudo apt-get install -y build-essential pkg-config cmake ninja-build clang \
    libssl-dev libdbus-1-dev libfontconfig1-dev libfreetype6-dev \
    libx11-dev libxcb1-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev
  ```

### Compile & Run
```bash
# Clone the repository
git clone https://github.com/dimitribellini/manifold.git
cd manifold

# Launch the desktop GUI in release mode
cargo run --release --bin manifold

# Run the full test suite
cargo test --workspace --lib
```

---

## 🏗️ Architecture & Crates

```text
manifold/
├── crates/
│   ├── vfs/          # Core FileSystemBackend, LocalFsBackend, PermissionsMatrix, ArchiveBackend
│   ├── vfs-remote/   # RemoteVfsBackend (SFTP/S3/SMB/FTP/WebDAV), SSH PTY & Staging Manager
│   ├── config/       # ConnectionsConfig (connections.toml) & OS Keyring CredentialStore
│   ├── transfer/     # Prioritized TransferEngine, TokenBucketLimiter & Conflict Resolution
│   ├── sync/         # SyncExecutionPlan (SHA-256 / Metadata / BinaryDiff) & BatchRenamer
│   ├── preview/      # FileInspectorViewModel, Permissions Matrix & 16-byte Hex/ASCII Formatter
│   └── ui/           # Slint Desktop GUI (ui/manifold.slint + gui.rs) & VirtualizedTableViewport
└── src/
    └── main.rs       # Desktop GUI & CLI entrypoint
```

---

## 📄 License

Released under the **MIT License**. See [`LICENSE`](LICENSE) for details.
