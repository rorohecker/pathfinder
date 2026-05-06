# Pathfinder

Pathfinder is a Windows 11 file manager built with Rust, Tauri 2, and a native Slint interface. It aims to feel familiar if you use File Explorer every day, while adding power tools like dual-pane workflows, tags, notes, themes, batch rename, checksums, duplicate finding, storage views, and AI-aware local features.

## Features

- **Native Slint UI** - no HTML frontend, with a custom Windows 11 style shell
- **Explorer-style navigation** - tabs, back and forward history, breadcrumbs, address bar, quick access, drives, bookmarks, and status bar
- **Multiple views** - icon grid, details list, gallery view, preview pane, and dual-pane mode
- **Fast folders** - directory caching, prefetching, file watching, and virtualized rendering for large folders
- **Search** - Windows Search support where available, recursive fallback search, and prefix filters like `ext:`, `kind:`, `size:`, `name:`, `content:`, `modified:`, and `tag:`
- **File tools** - copy, cut, paste, rename, delete to Recycle Bin, new folder, archive actions, checksums, batch rename, duplicate finder, and storage treemap
- **Tags and notes** - local color tags and per-file notes with visible indicators
- **Git badges** - lightweight file and folder status badges for repositories
- **Themes** - Mica Dark, Mica Light, Warm, Flat, Terminal, Paper, Retro, Fantasy, and Cyberpunk, plus accent and density controls
- **Command palette** - hit Ctrl+P and type the action you want
- **AI status** - detects NPU/GPU/CPU capability on startup and shows the result in Settings

## Download

Head to the [**Releases**](../../releases) page and grab the installer:

| File | Description |
|------|-------------|
| `Pathfinder_x.x.x_x64-setup.exe` | NSIS installer (recommended) |
| `Pathfinder_x.x.x_x64_en-US.msi` | Windows Installer package |

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Alt+Left / Right` | Back / Forward |
| `Alt+Up` | Go up |
| `F5` | Refresh |
| `F2` | Rename |
| `Delete` | Move to Recycle Bin |
| `Ctrl+T` | New tab |
| `Ctrl+L` | Focus address bar |
| `Ctrl+F` | Focus search |
| `Ctrl+P` | Command palette |
| `Ctrl+,` | Settings |
| `Ctrl+I` | Toggle preview pane |
| `Ctrl+1/2/3` | Icon / Details / Gallery view |
| `Ctrl+C / X / V` | Copy / Cut / Paste |
| `Ctrl+A` | Select all |
| `Ctrl+Shift+N` | New folder |
| `Space` | Quick Look |

## Build from Source

**Prerequisites:** [Rust](https://rustup.rs) stable, Windows build tools, and the normal [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/).

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
| Backend | Rust with Tauri 2 helpers, Rayon, walkdir, notify, image, zip, trash |
| UI | Slint 1 with the winit backend |
| Installer | NSIS / WiX MSI via Tauri bundler |

## AI Features

Pathfinder checks for local acceleration hardware on startup. If an NPU or GPU path is available, the UI can show accelerated feature status. If no supported hardware or runtime is found, Pathfinder falls back to CPU behavior and stays usable.

```powershell
$env:PATHFINDER_LOCAL_AI_RUNTIME = "path\to\runtime"
```

Without it, the AI tab in Settings will tell you what was detected and why the features are off.

## Windows Compatibility Side Note

Pathfinder is designed to cover the everyday File Explorer experience, but a few Windows features depend on shell extensions, cloud providers, enterprise policy, or system services that an app cannot fully control.

- Third-party context menu items may be delegated to the native Windows `Show more options` menu.
- Some virtual shell locations, phones, cameras, and Control Panel style views may open in Windows Explorer if they do not behave like normal folders.
- Previous Versions only appears when File History, server shadow copies, restore points, or VSS snapshots exist.
- Some Properties tabs, such as Compatibility or vendor-specific tabs, may open in the native Windows Properties dialog.
- Protected OS files may still be blocked by Windows permissions, UAC, TrustedInstaller, Windows Resource Protection, or policy.
- Defender scans can be started, but detailed scan results may be limited by Windows Security settings.
- Taskbar and Start pinning are restricted by Windows, so Pathfinder only uses the verbs Windows exposes.

## License

MIT
