# Pathfinder

A fast file manager for Windows 11. Built end to end in Rust and rendered with Slint — no HTML, no WebView, no embedded browser. Cold starts in well under a second and stays smooth on huge folders.

The goal is simple: feel faster and cleaner than File Explorer, while keeping the workflows you already know.

## Why it feels fast

- **Native all the way.** Rust backend, Slint GPU UI, Win32 shell ops — no PowerShell spawning on hot paths
- **Streaming folders.** Large directories show the first page immediately, then fill in with parallel metadata reads
- **Indexed search.** A local SQLite index makes finding files in folders you have already visited feel instant
- **Background work stays in the background.** Thumbnails and heavy scans use a machine-scaled worker pool at lower priority so clicks stay responsive

## Features

### Browse and navigate
- Tabs, back/forward history, breadcrumbs, address bar, and a real status bar
- Quick Access, drives, bookmarks, pins, and a Home landing
- Multiple views: icon grid, details list, gallery, preview pane, and dual pane with a draggable splitter
- Drag and drop between panes or from File Explorer (Ctrl to copy)
- Side mouse buttons for back and forward

### Select and act
- Multi-select with Ctrl, Shift, and Ctrl+A
- Contextual action bar when files are selected
- Inline rename (F2), copy / cut / paste, delete to Recycle Bin
- Conflict resolution with Skip, Replace, and Keep Both
- Undo for the last operation
- Command palette (Ctrl+P) to run any action by name

### Preview and inspect
- Text and code previews with binary detection
- Image previews with size and dimension metadata
- PDF first-page rendering (bundled pdfium)
- HTML and Markdown with a Code / View toggle
- Archive browsing as a virtual folder
- Properties, checksums, git status badges, color tags, and per-file notes

### Find and organize
- Fast search with operators: `ext:`, `kind:`, `size:`, `name:`, `content:`, `modified:`, `tag:`
- Optional on-device semantic ranking when Local AI models are installed
- Smart folders and saved searches in the sidebar
- Bulk rename with templates (`IMG_{n:04}.{ext}`)
- Duplicate finder (size → partial hash → full hash)
- Storage analyzer with largest items and cleanup suggestions
- Recycle Bin browser with restore and permanent delete

### Appearance and system
- Multiple built-in themes with accent color, density, and folder color overrides
- Mica window backdrop on Windows 11
- Settings for Appearance, View, Performance, and AI
- Optional Local AI model pack with Compact / Balanced / Quality profiles, hashed downloads, and silent self-update (NPU → GPU → CPU)
- One-click silent app updater: download, install, and relaunch without a wizard
- Optional default folder handler registration

## Download

Grab the latest build from [Releases](https://github.com/rorohecker/pathfinder/releases).

| File | Description |
|------|-------------|
| `Pathfinder_x.x.x_x64-setup.exe` | NSIS installer (recommended) |
| `Pathfinder_x.x.x_x64_en-US.msi` | Windows Installer package |

## Keyboard shortcuts

| Shortcut | Action |
|----------|--------|
| `Alt+Left` / `Right` | Back / Forward |
| `Alt+Up` | Go up one level |
| `F2` | Rename |
| `F5` | Refresh |
| `Delete` | Move to Recycle Bin |
| `Ctrl+T` / `Ctrl+W` | New tab / Close tab |
| `Ctrl+L` | Focus address bar |
| `Ctrl+F` | Focus search |
| `Ctrl+P` | Command palette |
| `Ctrl+,` | Settings |
| `Ctrl+I` | Toggle preview pane |
| `Ctrl+1` / `2` / `3` | Grid / List / Gallery |
| `Ctrl+C` / `X` / `V` | Copy / Cut / Paste |
| `Ctrl+A` | Select all |
| `Ctrl+Z` | Undo |
| `Ctrl+Shift+N` | New folder |
| `Space` | Quick Look |

## Build from source

Prerequisites: [Rust](https://rustup.rs) stable, Windows build tools, and the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```powershell
git clone https://github.com/rorohecker/pathfinder.git
cd pathfinder/src-tauri
cargo tauri build
```

Installer output lands in `src-tauri/target/release/bundle/`.

```powershell
cargo tauri dev
```

## Tech stack

| Layer | Technology |
|-------|------------|
| UI | Slint (native window, GPU pipeline) |
| Backend | Rust, Tauri 2, Rayon, SQLite |
| Shell | Win32 APIs directly |
| Installer | NSIS and MSI via the Tauri bundler |

## Windows notes

- Some third-party shell extensions may appear under "Show more options"
- Virtual locations (phones, cameras, Control Panel) may open in Explorer when they are not normal folders
- Previous Versions only appears when VSS / File History snapshots exist
- Protected OS files can still be blocked by UAC or TrustedInstaller
- Taskbar and Start pinning require the manual steps Windows exposes to third-party apps

## Disclaimer

Pathfinder is provided as is, without warranty of any kind. It is a file manager that can move, overwrite, and delete real files. Back up anything important. If you hit a bug, open a GitHub issue.

## License

MIT
