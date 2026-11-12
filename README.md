# Manifold

<div align="center">

[![GitHub](https://img.shields.io/badge/GITHUB-DIMITRIBELLINI%2FMANIFOLD-24292F?style=for-the-badge&logo=github&logoColor=white)](https://github.com/dimitribellini/manifold)
[![Release](https://img.shields.io/github/v/release/dimitribellini/manifold?style=for-the-badge&label=RELEASE&color=0A84FF&logo=github&logoColor=white)](https://github.com/dimitribellini/manifold/releases)
[![Platforms](https://img.shields.io/badge/PLATFORMS-MACOS%20ARM64%20%7C%20LINUX%20X86__64%20%7C%20WINDOWS%20X86__64-EE7F2D?style=for-the-badge&logo=apple&logoColor=white)](#quick-start)
[![Engine](https://img.shields.io/badge/ENGINE-RUST%20%2B%20SLINT-8A2BE2?style=for-the-badge&logo=rust&logoColor=white)](#architecture)
[![License](https://img.shields.io/badge/LICENSE-MIT-4CC61E?style=for-the-badge)](#license)

Cross-platform dual-pane file manager and remote transfer client built in Rust and Slint.

[**Releases**](https://github.com/dimitribellini/manifold/releases) · [**Quick Start**](#quick-start) · [**Features**](#features) · [**Multi-Rename**](#multi-rename-batch-rename) · [**Directory Sync**](#directory-sync--diff-cmdctrl--shift--y) · [**Remote Connections**](#remote-connections) · [**Keyboard Shortcuts**](#keyboard-shortcuts) · [**Build from Source**](#build-from-source) · [**Architecture**](#architecture)

</div>

---

<div align="center">
  <img src="screenshots/manifold_fig1.png" alt="Manifold Main Workspace" width="100%" />
</div>

---

## Quick Start

Download the latest release for **macOS (Apple Silicon)**, **Linux (x86_64)**, or **Windows (x86_64)** from [GitHub Releases](https://github.com/dimitribellini/manifold/releases).

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

### Windows (x86_64)
```powershell
Invoke-WebRequest -Uri https://github.com/dimitribellini/manifold/releases/latest/download/manifold-windows-x86_64.zip -OutFile manifold-windows-x86_64.zip
Expand-Archive -Path manifold-windows-x86_64.zip -DestinationPath .
.\Manifold.exe
```

---

## Features

- **Single & Dual-Pane Layouts**: Switch between single, side-by-side, and top/bottom dual panes (`Cmd/Ctrl + Alt + T`) with independent per-tab history and automatic session restore.
- **Four View Modes**: Browse `500,000+` entry directories smoothly in **Icons**, **List**, **Columns** (Miller view), or **Tree** (`Cmd/Ctrl + 1..4`).
- **Recursive Search**: Filter the current folder and subfolders using wildcards (`*`, `?`), and jump to any result's parent folder with **Show in Enclosing Folder**.
- **Inspector & Quick Look**: Inspect file metadata, media details, color tags, and `R/W/X` permissions (`Cmd/Ctrl + I`), or preview files with `Space`.
- **Built-in Terminal**: Integrated terminal drawer (`>_`) running your local shell (`zsh`, `bash`, `PowerShell`) or an interactive remote `ssh` session when browsing SFTP servers, with one-click **Sync CWD**.
- **Archives & Packages**: Create `.zip` and `.tar.gz` archives, browse `.zip` and `.tar.*` archives virtually in place (`archive://`) or via **Show Archive Contents**, and safely extract `.zip`, `.tar.*`, `.7z`, `.rar`, `.gz`, `.bz2`, and `.xz` files with automatic Zip-Slip/Tar-Slip traversal protection.
- **File Transfers**: Background copy and move queue with transactional `.part` staging, Pause/Resume/Cancel controls, a 5-stop bandwidth rate-limiting slider (`Unlimited`, `5`, `20`, `50`, `100 MB/s`), conflict resolution (`Replace`, `Replace Older`, `Keep Both`, `Skip`), and remote edit-in-place with automatic SHA-256 re-upload.
- **Multi-Rename & Directory Sync**: Multi-step batch file renamer with saved presets and live preview (`F2` with multiple items selected), plus an interactive **Directory Sync & Diff** preview accessible from the top toolbar or `Cmd/Ctrl + Shift + Y`.
- **Themes & Fonts**: Six built-in color themes and adjustable UI font family, size, and weight (`Cmd/Ctrl + ,`).

---

## Multi-Rename (Batch Rename)

<div align="center">
  <img src="screenshots/manifold_fig4.png" alt="Manifold Multi-Rename Modal" width="100%" />
</div>

- **Multi-Rule Pipeline**: Chain multiple rename operations in sequence (`+` / `−`) and scope each rule to **Name**, **Name with Extension**, or **Extension**.
- **Operations**:
  - **Replace Text**: Match **All**, **First**, **Last**, or **Regular Expression** (with `$1`–`$9` capture groups and optional case sensitivity).
  - **Change Case**: Convert to `lowercase`, `UPPERCASE`, `Title Case`, `camelCase`, `snake_case`, or `kebab-case`.
  - **Add Sequence**: Insert padded numeric counters with custom prefix, start value, step, and digit width.
  - **Add Text**: Insert custom text before or after the name, or replace the base name.
  - **Add Date**: Append or prepend the current date or the file's modification timestamp (`YYYY-MM-DD`, `YYYYMMDD`, `YYYY-MM-DD_HH-mm`).
- **Saved Presets**: Save rule pipelines to **Favorites** (`+ Save Preset`) for reuse across sessions.
- **Live Preview & Collision Guard**: Inspect before/after filenames in real time (`=` unchanged, `→` renamed, warning icon on duplicate or invalid targets) with two-phase collision-safe execution and rollback on error.

---

## Directory Sync & Diff (`Cmd/Ctrl + Shift + Y`)

<div align="center">
  <img src="screenshots/manifold_fig5.png" alt="Manifold Directory Sync & Diff Preview" width="100%" />
</div>

- **Sync Strategies**: Compare the Left and Right panes using **Left → Right**, **Right → Left**, **Mirror L → R**, **Mirror R → L**, or **Bi-Directional** synchronization across local and remote VFS folders.
- **Comparison Modes**: Detect changes by content **Hash** (`SHA-256`) or **Time + Size** metadata.
- **Interactive Changeset**: Review every proposed operation (`Copy →`, `← Copy`, `Delete L`, `Delete R`, `Conflict`) with left/right file sizes, toggle individual items with per-row **Run** checkboxes, and inspect total create/update/delete counts before clicking **Synchronize**.

---

## Folder View Options (`Cmd/Ctrl + J`)

<div align="center">
  <img src="screenshots/manifold_fig3.png" alt="Manifold Folder View Options" width="100%" />
</div>

- **Scope & Inheritance**: Apply view settings to **All Folders**, **This Folder**, or **This Folder and Subfolders**, and save any layout with **Use as Defaults**.
- **Sorting & Sizing**: Choose the default sort field (`Name`, `Modified`, `Created`, `Added`, `Size`, `Kind`), row icon size (`16px` / `20px` / `24px`), and text size (`11px`–`16px`).
- **Customizable Columns**: Show or hide `Size`, `Kind`, `Date Modified`, `Date Added`, `Date Created`, `Owner`, `Group`, `Permissions`, and `Tags`.
- **Display Preferences**: Toggle **Bold Folders**, **Show Hidden Files**, **Show Icon Preview**, **Treat Packages as Folders**, **Show Folders on Top**, **Use Relative Dates**, **Calculate All Sizes**, and **Use Alternating Background**.

---

## Remote Connections

<div align="center">
  <img src="screenshots/manifold_fig2.png" alt="Manifold Remote Connection Modal" width="100%" />
</div>

- **Supported Protocols**: `SFTP / SSH` (password or SSH key), `Amazon S3` (and S3-compatible endpoints), `SMB`, `FTP / FTPS`, and `WebDAV`.
- **Automatic Terminal Sync**: Opening an SFTP connection switches the bottom terminal to a live `ssh` session in the active remote folder.
- **Mounted Shares**: Active Linux `NFS`, `SMB / CIFS`, and `SSHFS` mounts appear automatically in the `Connections` sidebar.
- **Bandwidth Control**: Per-connection and live transfer rate limiting via a 5-stop slider (`Unlimited`, `5 MB/s`, `20 MB/s`, `50 MB/s`, `100 MB/s`).
- **Keychain Integration**: Passwords and key passphrases are stored in the OS keyring (`macOS Keychain`, Linux `libsecret`, or `Windows Credential Manager`).

---

## Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` | Move selection (`Shift` extends selection) |
| `Right` / `Enter` | Open selected folder or file (`Right` expands folder in `Tree` view) |
| `Left` / `Backspace` | Go to parent directory (`Left` collapses folder in `Tree` view) |
| `Cmd/Ctrl + 1..4` | Switch view mode (`1` Icons, `2` List, `3` Columns, `4` Tree) |
| `ESC` | Clear active input or close active menu, modal, or terminal drawer |
| `Space` | Toggle Quick Look preview for selected item |
| `Cmd/Ctrl + F` | Focus per-tab search/filter box |
| `Cmd/Ctrl + L` | Toggle path bar edit mode (`Tab` auto-complete) |
| `Cmd/Ctrl + T` | Open selected folder in a new tab |
| `Cmd/Ctrl + A` | Select all items in active pane |
| `Cmd/Ctrl + C` / `X` / `V` | Copy / Cut / Paste selected items |
| `Cmd/Ctrl + D` | Duplicate selected items |
| `Cmd/Ctrl + N` | Create new folder |
| `Cmd/Ctrl + R` | Refresh active pane(s) |
| `F2` or click selected name | Rename selected item (or open Multi-Rename window when multiple items are selected) |
| `Cmd/Ctrl + Shift + Y` | Open Directory Sync & Diff modal |
| `Cmd/Ctrl + Shift + C` | Copy full path or URI to clipboard |
| `Cmd/Ctrl + I` | Toggle Inspector drawer (`Info` / `Tasks`) |
| `Cmd/Ctrl + J` | Toggle File & Directory View Options (`Show View Options`) |
| `Cmd/Ctrl + ,` | Open Preferences modal |
| `Delete` | Move selected items to Trash |
| `Shift + Delete` | Permanently delete selected items |
| `Alt + Left` / `Alt + Right` | Navigate Back / Forward in tab history |
| `Alt + Up` | Navigate to enclosing folder |

---

## Build from Source

### Prerequisites
- **Rust**: Stable toolchain (`1.80+`, 2021 edition)
- **Linux dependencies**:
  ```bash
  sudo apt-get install -y build-essential pkg-config cmake ninja-build clang \
    libssl-dev libdbus-1-dev libfontconfig1-dev libfreetype6-dev \
    libx11-dev libxcb1-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev
  ```

### Compile & Run
```bash
git clone https://github.com/dimitribellini/manifold.git
cd manifold

# Run in release mode
cargo run --release --bin manifold

# Run unit tests
cargo test --workspace --lib
```

---

## Architecture

```text
manifold/
├── crates/
│   ├── vfs/          # Local filesystem backend, permissions matrix, and archive reader
│   ├── vfs-remote/   # Remote backends (SFTP, S3, SMB, FTP, WebDAV) and streaming transfers
│   ├── config/       # Connection profiles, user preferences, and OS keyring integration
│   ├── transfer/     # Transfer queue, bandwidth limiter, and conflict resolution models
│   ├── sync/         # Directory comparison/sync engine and batch rename rules
│   ├── preview/      # File metadata, permissions inspector, and text/hex preview formatters
│   └── ui/           # Modular Slint UI (ui/{components,modals,manifold}.slint), GUI controllers (src/gui/*), and PTY terminal
└── src/
    └── main.rs       # Application entrypoint
```

---

## License

Released under the [MIT License](LICENSE).
