# Pathfinder

Pathfinder is a native Windows 11 file manager built in Rust with a Slint UI. It covers the everyday File Explorer experience while adding search filters, tags, notes, themes, batch operations, storage views, git badges, and a command palette.

## Features

- **Native Slint UI** - no HTML or WebView, direct Slint rendering with a Windows 11 style shell and Mica blur
- **Explorer-style navigation** - tabs, back/forward history, clickable breadcrumbs, address bar, Quick Access, drives, bookmarks, and status bar
- **Multi-select** - Ctrl+click to toggle individual files, Shift+click to extend a range, or Ctrl+A to select everything
- **Multiple views** - icon grid, details list, gallery view, preview pane, and dual-pane mode
- **Inline rename** - press F2 or use the context menu to rename in place without a dialog
- **Fast folders** - directory caching, background file watching, and a SQLite index of visited directories for instant cross-directory search
- **Search** - Windows Search integration where available, recursive fallback search, and prefix filters: `ext:`, `kind:`, `size:`, `name:`, `content:`, `modified:`, `tag:`
- **File tools** - copy, cut, paste, rename, delete to Recycle Bin, new folder, archive actions, checksums, batch rename, duplicate finder, and storage treemap
- **Tags and notes** - local color tags and per-file notes with visible indicators in all views
- **Git badges** - file and folder status badges for repositories without any extension or daemon
- **Themes** - Mica Dark, Mica Light, Warm, Flat, Terminal, Paper, Retro, Fantasy, and Cyberpunk, with accent color and density controls in the Settings panel
- **Settings panel** - tabbed Appearance, View, and AI tabs accessible from the toolbar or Ctrl+,
- **Command palette** - Ctrl+P to run any action by name
- **AI status** - detects NPU, GPU, or CPU acceleration on startup and reports it in the AI settings tab
- **Windows integration** - shell extensions context menu, VSS Previous Versions, UAC/TrustedInstaller handling, and taskbar/Start menu pinning
- **Admin features** - retry operations as administrator, take ownership of TrustedInstaller files, and manage system-protected items

## Download

Head to the [Releases](../../releases) page and grab the installer.

| File | Description |
|------|-------------|
| `Pathfinder_x.x.x_x64-setup.exe` | NSIS installer (recommended) |
| `Pathfinder_x.x.x_x64_en-US.msi` | Windows Installer package |

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Alt+Left / Right` | Back / Forward |
| `Alt+Up` | Go up one level |
| `F2` | Rename in place |
| `F5` | Refresh |
| `Delete` | Move to Recycle Bin |
| `Ctrl+T` | New tab |
| `Ctrl+W` | Close tab |
| `Ctrl+L` | Focus address bar |
| `Ctrl+F` | Focus search |
| `Ctrl+P` | Command palette |
| `Ctrl+,` | Settings |
| `Ctrl+I` | Toggle preview pane |
| `Ctrl+1 / 2 / 3` | Grid / List / Gallery view |
| `Ctrl+C / X / V` | Copy / Cut / Paste |
| `Ctrl+A` | Select all |
| `Ctrl+Z` | Undo last operation |
| `Ctrl+Shift+N` | New folder |
| `Space` | Quick Look |

## Build from Source

**Prerequisites:** [Rust](https://rustup.rs) stable, Windows build tools, and the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```powershell
git clone https://github.com/rorohecker/pathfinder.git
cd pathfinder/src-tauri
cargo tauri build
```

The installer will be in `src-tauri/target/release/bundle/`.

To run in dev mode:

```powershell
cargo tauri dev
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Shell | Native Slint window hosted from the Rust app |
| Backend | Rust, Tauri 2, Rayon, walkdir, notify, image, zip, trash |
| UI | Slint 1 with the winit backend |
| Index | SQLite via rusqlite (bundled), used for cross-directory file search |
| Installer | NSIS and WiX MSI via the Tauri bundler |

## AI Features

Pathfinder checks for local acceleration hardware at startup. If an NPU or GPU path is available, that gets reflected in the AI tab under Settings. If nothing supported is found it falls back to CPU and stays fully functional.

You can point it at a custom runtime with:

```powershell
$env:PATHFINDER_LOCAL_AI_RUNTIME = "path\to\runtime"
```

Without it, the AI tab will show what was detected and explain why features are off.

## Windows Compatibility Notes

Pathfinder is built around the everyday File Explorer workflow, but some Windows behaviors sit outside what any third-party app can control.

- Third-party shell extensions may appear under "Show more options" rather than the primary context menu.
- Virtual shell locations, phones, cameras, and Control Panel views may open in Windows Explorer if they do not behave as normal filesystem folders.
- Previous Versions only appears when File History, server shadow copies, restore points, or VSS snapshots exist on the system.
- Some Properties tabs (Compatibility, vendor-specific tabs) may open in the native Windows Properties dialog.
- Protected OS files can still be blocked by UAC, TrustedInstaller, Windows Resource Protection, or group policy regardless of what Pathfinder shows.
- Taskbar and Start pinning use only the verbs that Windows exposes to third-party apps.

## License

MIT
