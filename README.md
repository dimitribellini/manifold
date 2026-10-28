# Manifold: Cross-Platform Dual-Pane File Manager & Remote Transfer Engine

<div align="center">

[![GitHub](https://img.shields.io/badge/GITHUB-DIMITRIBELLINI%2FMANIFOLD-24292F?style=for-the-badge&logo=github&logoColor=white)](https://github.com/dimitribellini/manifold)
[![Release](https://img.shields.io/badge/RELEASE-V0.1.0-0A84FF?style=for-the-badge&logo=github&logoColor=white)](https://github.com/dimitribellini/manifold/releases)
[![Platforms](https://img.shields.io/badge/PLATFORMS-MACOS%20ARM64%20%7C%20LINUX%20X86__64-EE7F2D?style=for-the-badge&logo=apple&logoColor=white)](#quick-start)
[![Engine](https://img.shields.io/badge/ENGINE-100%25%20RUST%20%2B%20SLINT-8A2BE2?style=for-the-badge&logo=rust&logoColor=white)](#architecture--crates)
[![License](https://img.shields.io/badge/LICENSE-MIT-4CC61E?style=for-the-badge)](#license)

**Navigate** local volumes, archives (`.zip`, `.tar.gz`, `.7z`, `.rar`), and remote filesystems in a resizable **Single or Dual-Pane Workspace** with **Full Arrow-Key & `ESC` Navigation**.<br/>
**Connect** to **SFTP/SSH, S3, SMB, FTP, and WebDAV** endpoints with native **OS Keychain / Secret Service** credential isolation.<br/>
**Transfer, Preview & Automate** with **Non-Blocking Multi-GB ISO Streaming**, **Mouseover Previews & Media Metadata**, **Interactive PTY Terminal**, and **Font-Linked UI Scaling & 6 Themes**.

📦 [**GitHub Releases**](https://github.com/dimitribellini/manifold/releases) · ⚡ [**Quick Start**](#quick-start) · ✨ [**Key Features**](#key-features) · 🖥️ [**Workspace, Previews & Terminal**](#-workspace-direct-tab-previews--embedded-terminal) · 🎨 [**Themes & Typography**](#-themes-typography--favorites) · 🔌 [**Remote Connections**](#-remote-connections--large-iso-streaming) · ⌨️ [**Keyboard Shortcuts**](#️-keyboard-shortcuts) · 🛠️ [**Build from Source**](#️-build-from-source)

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

- **Resizable Single & Dual-Pane Workspace**: Switch seamlessly between **Single Pane** and **Dual Pane** with smooth, non-oscillating **1:1 resizable splitters** for the Left Sidebar, Dual-Pane center divider, Miller Columns parent pane, Title-Case `Name | Size | Date Modified` table columns, Bottom Terminal drawer, and Right Inspector Drawer.
- **Non-Blocking Multi-GB ISO Transfers & Interactive Conflict Resolution Modal**: Local↔Remote and large (`>= 4 MB`) file copy/move operations execute on dedicated background worker threads and stream directly between disk and `scp` (`3600s` transfer timeout) with live 50ms progress polling—never loading multi-GB ISOs into RAM or freezing the UI. Every Copy, Move, Paste (`Cmd/Ctrl + V`), Dual-Pane (`F5`/`F6`), and Drag-and-Drop operation performs a **Pre-Flight Collision Check** and presents an interactive **Conflict Resolution Modal** (`New` vs `Existing` with `DD/MM/YYYY, HH:mm` modification dates and file sizes, **`Replace`**, **`Replace Older`**, **`Keep Both`**, **`Skip`**, **`Stop`**, and **`Apply to all`**).
- **Drag-and-Drop Folder Tabs, Instant Tab Switching & Two-Line Path/Status Header**: Active folder tabs feature a deep `#08080A` pill background with crisp white monochrome outline icons (`folder`, `movies` filmstrip, `music`, `home`, `globe`, `search`), hover-only `×` close buttons, and a full-width `2.5px` theme-colored accent line directly above a **Two-Line Path & Status Header Bar** (`28×28px` Home/Globe/Volume icon on the left + `$HOME`-relative clickable breadcrumbs `user › Downloads › Folder` on Line 1 and `N items, X GB available` on Line 2). Long folder titles in the top toolbar automatically elide (`…`) so toolbar view/pane controls never shift. Reorder tabs within a pane or drag tabs between panes, and view remote tabs as `<Connection Name>: <remote_path>`.
- **Blank Directory Quick Actions**: Left-click or right-click inside any empty folder (across List, Miller Columns, or Icon Grid views) to create a **New Folder**, create a **New File**, or **Paste** clipboard items immediately.
- **Direct Tab Folder Previews (`List`, `Columns` & `Icons`), Media Metadata & Mouseover Preview Card**: View real raster image thumbnails (`PNG`, `JPG`, `JPEG`, `GIF`, `WEBP`, `BMP`, `ICO`, `SVG`) and semantic vector file-type icons (`#FF5F56` red for `PDF`, `#5AC8FA` for images, `#A0A0AB` for documents/archives, `#38B6FF`/`#4CC2FF` solid sky-blue fill for folders—all preserving their original colors when selected) directly inside folder tabs across **List**, **Columns** (including a **3rd Miller Preview & Media Metadata Column**), and **Icons** views. Extract format-specific **Photo, Audio/Music, and Video metadata** automatically, and hover over any item for a **Mouseover File Details & Non-Binary Preview Card** covering photos, UTF-8 text, JSON, YAML, TOML, XML, code, Markdown, and DOCX (never dumping raw binary hex).
- **Interactive PTY Bottom Terminal**: Click the toolbar **`>_` (Terminal)** icon or select **Open in Bottom Terminal** from any folder or sidebar context menu to open a persistent **Interactive PTY Shell** directly inside Manifold. Navigate command history with `Up` / `Down`, interrupt running processes with `Ctrl+C` or `ESC`, and move between folders and tabs with silent automatic working-directory synchronization (without cluttering terminal output with `[cd]` lines).
- **Manifold Main Menu, 6 Color Themes & Font-Linked UI Scaling (`660×476px` Modal)**: Open the top-left **`Manifold ▾`** menu (`Preferences & Themes…`) to customize **Font Size** (`Compact -1` to `XL +4`, dynamically scaling file rows, icon cards, sidebar items, tab bars, path headers, table column headers, inspector labels, and terminal text), **Font Type** (`System Sans`, `Inter / UI`, `Monospace`, `Serif` with native macOS CoreText & Linux Fontconfig resolution), **Font Style** (`Regular Weight` `500` or `Bold / High-Legibility` `700`), and **6 Built-In Color Themes** (`Dark Zinc` active by default).
- **Dual-Pane Theme-Colored Surrounding Highlight, Clean 2-Tab Inspector (`Info | Tasks`) & Instant Context Menus**: In Dual-Pane mode, the active pane is highlighted solely by a `2px` surrounding border using the active theme's accent color (`theme-accent`). Toggling the Right Inspector Drawer (`ⓘ Info | ⇅ Tasks`) dynamically budgets pane widths so the Inspector is never pushed off-screen, and all right-click context menus, popovers, and tooltips open instantaneously at the exact cursor position in both panes via in-place model updates without playing intrusive OS sound chimes or floating toast banners.
- **Universal Drag & Drop & Draggable/Sortable Favorites**: Drag single or multi-selected files and folders onto any subfolder row, opposite pane, tab pill, breadcrumb bar, or Sidebar item. Pin any local or remote folder to **Favorites** (via the toolbar `★` button or right-click menu) and **drag Favorites entries up or down** (or use `Move Up in Favorites` / `Move Down in Favorites`) to persist your preferred custom sort order in `~/.config/manifold/preferences.toml`.
- **Full Arrow-Key & Bounce-Free Double-Click Navigation, Universal `ESC` Dismissal & Instant In-Memory `Cmd/Ctrl + F` Wildcard Search (`*` & `?`)**: Navigate folders with `Up`, `Down`, `Left` (parent folder), `Right` / `Enter` (enter folder), `Backspace`, `Home`, `End`, `PageUp`, `PageDown`, or bounce-free double-click (`280ms` post-navigation guard), press `ESC` anywhere to exit active commands, inputs, popovers, or modals, and filter or sort large directories instantaneously in RAM (`0` disk reads per keystroke) with per-tab wildcard filters (`*pdf`, `*.rs`).
- **Rich 8-Color Tagging, Archive Virtualization & Directory Sync**: Tag items with 8 color labels (`Red`, `Orange`, `Yellow`, `Green`, `Blue`, `Purple`, `Gray`, `None`), browse `.zip`/`.tar.*`/`.7z`/`.rar` archives inline, run **SHA-256 / Metadata / Binary Diff** directory sync, and apply multi-rule **Batch Renaming**.

---

## 🖥️ Workspace, Direct Tab Previews & Embedded Terminal

### 🖼️ Direct In-Tab Previews, Media Metadata & Mouseover Details
- **Direct Folder Tab Previews (`List`, `Columns`, and `Icons` Modes)**:
  - **Icons View (`PolaroidCell`)**: Image files display their actual raster image thumbnail directly inside the card, while documents and archives (`.pdf`, `.docx`, `.zip`, `.tar.gz`, etc.) display a styled document preview card with color-coded format badges (`PDF`, `DOCX`, `ZIP`) and file size footers.
  - **List View (`FileRow`) & Columns View (`Miller Columns`)**: Image rows render `20×20px` raster thumbnails, while documents, archives, executables, music, and video files display clean semantic vector icons (`#FF5F56` red for `PDF`, `#5AC8FA` for images, `#A0A0AB` for documents/archives, `#38B6FF`/`#4CC2FF` solid fill for folders—preserving their original colors when selected) without redundant inline extension badge columns. In **`Columns` mode**, a **3rd Miller Preview & Media Metadata Column** displays the selected item's live preview and rich media metadata alongside the parent and current directory columns.
  - **Photo, Music & Video Metadata**: Selecting or inspecting media files automatically extracts and displays format-specific metadata in both **`Columns` mode** and the Right Inspector **`ⓘ Info`** tab:
    - **Photo / Image** (`.png`, `.jpg`, `.webp`, `.gif`, `.svg`, etc.): Pixel resolution (`W×H px`), megapixels (`MP`), color space (`RGBA` / `sRGB` / `Vector SVG`), and format.
    - **Audio / Music** (`.wav`, `.flac`, `.mp3`, `.m4a`, `.aac`, `.ogg`, etc.): Audio codec, sample rate (`kHz`), channel layout (`Mono` / `Stereo`), bit depth / bitrate (`kbps`), and duration.
    - **Video / Media** (`.mp4`, `.mov`, `.mkv`, `.webm`, `.avi`, etc.): Container format, video/audio codec family, estimated bitrate (`Mbps`), and duration.
  - **Mouseover File Details & Non-Binary Preview Card**: Hovering over any file row or icon card displays a floating details card with file size, modification timestamp, permissions, media metadata, and a live preview strictly for **photos, UTF-8 text, JSON, YAML, TOML, XML, source code, Markdown, and DOCX documents** (binary files show clean metadata without raw hex dumps). Pressing **`Space`** toggles the zoomable (`50%`–`300%`) floating Quick Look HUD.
  - **Dual-Pane Theme-Colored Highlight**: In Dual-Pane mode, the active pane is highlighted by a `2px` surrounding border in the active theme's accent color (`theme-accent`).

### `>_` Interactive PTY Bottom Terminal Drawer
- **Persistent Interactive PTY Shell**: Clicking the top toolbar **`>_` (Terminal)** icon, selecting **Open in Bottom Terminal** from the folder right-click menu, or choosing **Open Terminal Here / Open SSH Terminal** on any Sidebar item opens the **Embedded Bottom Terminal Drawer** backed by a persistent interactive PTY shell (`$SHELL -i`).
- **Silent Two-Way Directory Synchronization**: Navigating into a folder, switching tabs, or switching between the Left and Right panes silently synchronizes the shell's working directory (`cd`) and prompt badge without printing `[cd] -> ...` spam in the terminal buffer. Typing `cd <folder>` inside the terminal also navigates the active file pane to that directory.
- **Command History, Live Streaming & `Ctrl+C` / `ESC` Interrupts**: Browse previous shell commands with `Up` / `Down` arrows, stream live output with automatic ANSI/VT100 escape sequence cleaning, and stop running commands anytime with `Ctrl+C` (or the `Ctrl+C` button) or `ESC`.

### 📁 Folder Tab Visualization, Two-Line Path Header & Blank Directory Actions
- **Borderless Folder Tabs & Full-Width Theme Accent Line**:
  - **Active Tab Pill**: Deep `#08080A` pill background without outline box borders, crisp white `#FFFFFF` label, and monochrome outline icon (`folder`, `movies` filmstrip with sprocket holes, `music` note, `home` house, `globe` remote, or `search` lens), underscored across the full pane width by a `2.5px` theme-colored horizontal accent line.
  - **Responsive Dual-Pane Tab Resizing**: Folder tab pills dynamically resize (`56px..=184px`) to fit the available pane width and tab count with clean `…` label elision, so dragging the Dual-Pane center splitter keeps all open tabs and the `+` button visible inside the pane.
  - **Inactive Tab Pills & Hover-Only Close (`×`)**: Subtle `#212126` charcoal pills with muted `#9898A2` text and hover-revealed `×` close buttons. Drag any tab left/right to reorder within a pane or across to the opposite pane.
- **Two-Line Path & Status Header Bar**:
  - Features a **`28×28px` Context Icon** on the left (`Home` house icon inside `$HOME`, `Globe` on remote servers, or `Volume` drive icon on root/external disks) beside a 2-line stacked layout scaled with your chosen font size:
    - **Line 1 (Top)**: `$HOME`-relative clickable breadcrumb path (`user › Downloads › Test_Manifold`) with sibling folder dropdown and `Cmd/Ctrl + L` path edit mode.
    - **Line 2 (Bottom)**: Item count and available disk space subtitle (`4 items, 100,58 GB available`) aligned directly beneath the breadcrumb trail.
- **Empty Folder Quick Bar**: Empty directories display interactive **`+ New Folder`**, **`+ New File`**, and **`Paste`** action buttons in the center of the viewport, and clicking/right-clicking anywhere on the blank canvas opens the folder context menu (`New Folder…`, `New File…`, `Paste Here`, `Add Current Folder to Favorites`, `Open in Bottom Terminal`) at the exact cursor location.

---

## 🎨 Themes, Typography & Favorites

Click **`Manifold ▾`** at the top-left of the toolbar and select **`Preferences & Themes…`** (or press **`Cmd/Ctrl + ,`**) to customize the appearance of the application in real time. All settings are automatically persisted to `~/.config/manifold/preferences.toml`:

| Preference | Available Options |
| :--- | :--- |
| **Font Size** | `Compact (-1)` · `Standard (0)` · **`Medium (+1, Default)`** · `Large (+2)` · `XL (+4)` *(dynamically scales rows, cards, sidebar, tabs, headers, inspector & terminal)* |
| **Font Type** | **`System Sans (Default)`** · `Inter / UI` (`Avenir Next` / `DejaVu Sans`) · `Monospace` (`Menlo` / `DejaVu Sans Mono`) · `Serif` (`Georgia` / `DejaVu Serif`) |
| **Font Style** | **`Regular Weight (Default)`** · `Bold / High-Legibility` |
| **Color Themes** | **`Dark Zinc (Default)`** (`#121214` / `#0D52BF`) · **`Midnight Ocean`** (`#0B1120` / `#0284C7`) · **`Dracula Pro`** (`#191A21` / `#BD93F9`) · **`Nordic Slate`** (`#1D212A` / `#5E81AC`) · **`Solarized Dark`** (`#001E26` / `#268BD2`) · **`High Contrast`** (`#050507` / `#2563EB`) |

### ⭐ Pinning & Sorting Favorites
- Pin any local or remote folder to the Left Sidebar **Favorites** section by clicking the toolbar **`★`** button, dragging a folder into the **Favorites** section, or right-clicking a folder and choosing **`Add to Favorites`**.
- **Drag & Drop Sorting**: Drag any item under **Favorites** up or down in the sidebar—or right-click and choose **`Move Up in Favorites`** / **`Move Down in Favorites`**—to reorder your favorites list (`favorite_order` persisted automatically in `~/.config/manifold/preferences.toml`).
- Right-click any pinned favorite in the sidebar to open it in a new tab or dual pane, rename its label, open it in the **Integrated Bottom Terminal**, assign an **8-color tag**, or remove it from Favorites.

---

## 🔌 Remote Connections & Large ISO Streaming

Click the toolbar **World (`🌐`)** icon to open the protocol-aware connection manager, or right-click any saved server under **Connections** in the sidebar to edit, duplicate, reconnect, or open an integrated **SSH Session in the Bottom Terminal**:

<div align="center">
  <img src="screenshots/manifold_fig2.png" alt="Manifold New SFTP Connection Modal" width="100%" />
</div>

- **Supported Protocols**: `SFTP / SSH` (password or SSH private key), `Amazon S3` (and S3-compatible endpoints), `SMB`, `FTP / FTPS`, and `WebDAV`.
- **Direct Disk-to-SCP Streaming for Multi-GB ISOs**: Uploading or downloading large ISO images and archives between local disks and remote SFTP/SSH servers streams directly via background `scp` processes (`upload_local_file` / `download_to_local_file`) without buffering files in memory, keeping the UI responsive at 60 FPS with live progress updates in the **Tasks & Operations (`⇅`)** popover.
- **Connection-Aware Tab Titles**: Every tab connected to a remote server displays `<Connection Name>: <remote_path>` so multiple remote sessions are immediately distinguishable.
- **Zero Plaintext Secrets**: Connection profiles are saved in `~/.config/manifold/connections.toml`, while passwords and private key passphrases are stored exclusively in your **OS Keychain** (`macOS Keychain` / Linux `libsecret` under `service = "manifold"`).

---

## ⌨️ Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End` | Move row/icon selection with automatic viewport scrolling (`Shift` extends selection) |
| `Right` / `Enter` | Enter selected folder (or move right in Icon Grid) |
| `Left` / `Backspace` | Navigate up to parent directory (or move left in Icon Grid) |
| `ESC` | Exit active command/input, dismiss modals/popovers/menus, or interrupt/close Terminal Drawer |
| `Cmd/Ctrl + ,` | Open **Manifold Preferences — Fonts & Themes** modal |
| `Space` | Toggle zoomable **Quick Look Preview HUD** (`50%`–`300%`) for selected item |
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
| `Cmd/Ctrl + I` | Toggle Right Inspector Drawer (`Info / Tasks`) |
| `Delete` | Move selected items to Trash (with confirmation) |
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
│   ├── vfs-remote/   # RemoteVfsBackend (SFTP/S3/SMB/FTP/WebDAV), Direct SCP Streaming, Remote Shell & Staging
│   ├── config/       # ConnectionsConfig (connections.toml), AppPreferences (preferences.toml) & OS Keyring
│   ├── transfer/     # Prioritized TransferEngine, TokenBucketLimiter & Conflict Resolution
│   ├── sync/         # SyncExecutionPlan (SHA-256 / Metadata / BinaryDiff) & BatchRenamer
│   ├── preview/      # FileInspectorViewModel, Permissions Matrix & 16-byte Hex/ASCII Formatter
│   └── ui/           # Slint Desktop GUI (ui/manifold.slint + gui.rs), Interactive PTY Terminal, Media Metadata & Theme Engine
└── src/
    └── main.rs       # Desktop GUI & CLI entrypoint
```

---

## 📄 License

Released under the **MIT License**. See [`LICENSE`](LICENSE) for details.
