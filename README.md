# Pathfinder

Pathfinder is a file manager for Windows 11 that wants to be the version of Explorer you actually like using. It is written end to end in Rust and rendered with Slint, no HTML and no WebView in sight, so the whole app cold starts in well under a second and stays smooth on huge folders.

**Version 0.7.1** adds a first-run welcome dialog that registers Pathfinder as the default folder handler in one click and walks the user through pinning Pathfinder and unpinning File Explorer (Windows 11 doesn't let third-party apps pin themselves), redesigns the Retro folder as a pixel-art floppy and the High Fantasy folder as a wooden treasure chest, gives High Fantasy a new emerald and gold palette, adds Gold, Indigo, and Crimson accent colors alongside Black and White, expands the Performance tab with a plain-English explanation and live memory usage from GetProcessMemoryInfo, surfaces detected discrete GPUs in the AI tab, isolates the custom theme editor preview so picking colors no longer recolors the whole app, and tidies up the View tab with a proper ScrollView and explanatory text.

**Version 0.7.0** added ONNX Runtime inference (DirectML first, CPU fallback), file-name embeddings in SQLite for semantic ranking in the search bar, optional MobileNet-based tag suggestions, dHash-based duplicate image detection, a `--path` launch argument, and a Windows Settings panel to register Pathfinder as your per-user default folder handler (plus an optional `extras/set-pathfinder-default-folder-handler.reg`).

The whole point is being faster and more efficient than the built in File Explorer while looking like an app you would pick on purpose. A few of the reasons it ends up that way:

- Rust everywhere instead of C++ plus COM glue means tighter code, no garbage collector pauses, and no extra process boundaries on hot paths
- Slint renders the UI as a native window with a real GPU pipeline, not a packaged browser, so resize and scroll stay at frame rate
- Directories stream into view in chunks of 2000 entries with metadata fetched in parallel via rayon, so a folder with 50 000 files renders the first page in under a hundred milliseconds
- Sort uses a natural comparator so file2.txt sorts before file10.txt the way you would expect
- A SQLite full text index sits behind the search bar, so finding files in folders you have already visited is basically instant
- A dedicated two thread image pool generates thumbnails at below normal priority, so they never compete with the foreground click you just made
- Every shell operation goes through Win32 directly rather than spawning PowerShell, which is the biggest single reason Explorer feels sluggish

It is also meant to have a personality. Eleven themes, each with its own folder color and font, including a Retro theme that ships with the Press Start 2P pixel arcade font baked into the binary. Custom folder icons, file type glyphs, drag and drop with a destination pane highlight, a contextual action bar when you select files, tabs, dual pane mode with a draggable splitter, a Recycle Bin browser, a bulk rename template engine, git badges, color tags, per file notes, a Mica window backdrop, a command palette, and a top right NPU detector that reports your AI acceleration tier on startup.

## Features

- **Native Slint UI** - no HTML or WebView, direct Slint rendering with a Windows 11 style shell and Mica blur
- **Explorer-style navigation** - tabs, back/forward history, clickable breadcrumbs, address bar, Quick Access, drives, bookmarks, and status bar
- **Multi-select** - Ctrl+click to toggle individual files, Shift+click to extend a range, or Ctrl+A to select everything
- **Multiple views** - icon grid, details list, gallery view, preview pane, and dual-pane mode
- **Inline rename** - press F2 or use the context menu to rename in place without a dialog
- **Fast folders** - directory caching using the Windows FindFirstFileExW cache, background file watching, and a SQLite index of visited directories for instant cross-directory search
- **Search** - Windows Search integration where available, recursive fallback scan, and prefix filters: `ext:`, `kind:`, `size:`, `name:`, `content:`, `modified:`, `tag:`; optional **semantic mode** (Σ) ranks indexed hits by on-device MiniLM embeddings when Local AI models are installed
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
- **AI status** - detects NPU, GPU, or CPU acceleration on startup; ONNX Runtime line is merged into the AI tab explanation once models are loaded
- **Local AI** - optional model pack (text embedding, tokenizer, MobileNet classifier) with install progress in Settings; embeddings and dHashes are written during background indexing
- **Windows integration** - shell extensions context menu, VSS Previous Versions, UAC and TrustedInstaller handling, taskbar and Start menu pinning, optional per-user default **folder** handler (`--path "%1"`), and `extras/set-pathfinder-default-folder-handler.reg` for manual registry import
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
| `extras/set-pathfinder-default-folder-handler.reg` | Optional template to set Pathfinder as the default folder handler (edit `CHANGEME_EXE`, then import) |

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

## Disclaimer

Pathfinder is provided as is, without warranty of any kind, express or implied. That includes the implied warranties of merchantability, fitness for a particular purpose, and non infringement. The authors and contributors are not liable for any claim, damage, or other liability arising from the use of this software, whether in contract, tort, or otherwise. You are running an early version of a file manager that touches real files on your real disk. Back up anything you care about. Bugs happen, especially around delete, move, and overwrite operations. If you find one, an issue on GitHub is the fastest way to get it fixed.

## License

MIT
