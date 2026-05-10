# Pathfinder

Pathfinder is a native Windows 11 file manager built in Rust with a Slint UI. It covers the everyday File Explorer experience while adding fast search, tags, notes, themes, batch operations, storage views, git badges, and a command palette. Every shell operation uses direct Win32 API calls rather than spawning external processes, so interactions feel instant.

## Features

- **Native Slint UI** - no HTML or WebView, direct Slint rendering with a Windows 11 style shell and Mica blur
- **Explorer-style navigation** - tabs, back/forward history, clickable breadcrumbs, address bar, Quick Access, drives, bookmarks, and status bar
- **Multi-select** - Ctrl+click to toggle individual files, Shift+click to extend a range, or Ctrl+A to select everything
- **Multiple views** - icon grid, details list, gallery view, preview pane, and dual-pane mode
- **Inline rename** - press F2 or use the context menu to rename in place without a dialog
- **Fast folders** - directory caching using the Windows FindFirstFileExW cache, background file watching, and a SQLite index of visited directories for instant cross-directory search
- **Search** - Windows Search integration where available, recursive fallback scan, and prefix filters: `ext:`, `kind:`, `size:`, `name:`, `content:`, `modified:`, `tag:`
- **Command palette** - Ctrl+P to run any action by name, with results ranked by relevance so exact matches always come first
- **File tools** - copy, cut, paste, rename, delete to Recycle Bin, new folder, archive actions, checksums, batch rename, duplicate finder, and storage treemap
- **Smart duplicate detection** - three-phase finder that groups by file size first, then compares a 64 KB partial hash, then reads the full file only for candidates that survived both filters
- **File preview** - text with binary detection, images with dimension metadata for files too large to inline, and generic metadata for everything else
- **Tags and notes** - local color tags and per-file notes with visible indicators in all views
- **Git badges** - file and folder status badges for repositories without any extension or daemon
- **Themes** - Mica Dark, Mica Light, Frost, Warm, Flat, Terminal, Paper, Retro, Fantasy, Sunset, and Cyberpunk, each with its own folder color and matching font, plus accent color and density controls in the Settings panel
- **Drag and drop** - drop files from File Explorer or between dual panes, the destination pane lights up while you hover and Ctrl held copies instead of moves
- **Recycle Bin browser** - sidebar entry that lists everything in the OS trash with the deletion time and original folder visible, plus restore, delete permanently, and empty actions
- **Streamed directory loading** - very large folders show the first 2500 entries instantly and then fill in progressively in chunks of 2000 with parallel metadata reads
- **Natural sort order** - file2.txt sorts before file10.txt, matching what File Explorer does
- **Bulk rename with templates** - select multiple files and apply a template like IMG_{n:04}.{ext} to renumber them in one go
- **Conflict resolution** - clear Skip, Replace, and Keep Both buttons when a paste or drop hits an existing file
- **Settings panel** - tabbed Appearance, View, and AI tabs accessible from the toolbar or Ctrl+,
- **AI status** - detects NPU, GPU, or CPU acceleration on startup and reports it in the AI settings tab
- **Windows integration** - shell extensions context menu, VSS Previous Versions, UAC and TrustedInstaller handling, and taskbar and Start menu pinning
- **Win32 shell operations** - properties dialog, shortcut creation, run as administrator, and clipboard writes all go through Win32 APIs directly with no PowerShell process overhead
- **Admin features** - retry operations as administrator, take ownership of TrustedInstaller files, and manage system-protected items
- **Battery-aware indexing** - background indexing pauses automatically when the device is on battery below 20 percent charge
- **Thumbnail pool** - image thumbnails are generated on a dedicated two-thread pool running at below-normal priority so they never compete with foreground work
- **Operation limits** - at most two heavy background operations run at the same time so the app stays responsive during large scans or duplicate searches

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
