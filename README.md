# Pathfinder

Pathfinder is a file manager for Windows 11. It is written end to end in Rust and rendered with Slint, so there is no HTML, no WebView, and no embedded browser involved. The whole app cold starts in well under a second and stays smooth on huge folders.

The goal is to be faster and more efficient than the built in File Explorer while looking like an app you actually want to open. A few of the reasons it ends up that way:

- Rust everywhere instead of C++ plus COM glue keeps the code tight, avoids garbage collector pauses, and removes the extra process boundaries Explorer has on hot paths
- Slint renders the UI as a native window with a real GPU pipeline rather than a packaged browser, so resize and scroll stay at frame rate
- Directories stream in chunks of 2000 entries with metadata fetched in parallel through rayon, so a folder with 50 000 files shows the first page in under a hundred milliseconds
- Sort uses a natural comparator, so file2.txt sorts before file10.txt the way you would expect
- A SQLite full text index sits behind the search bar, so finding files in folders you have already visited is basically instant
- A dedicated two thread image pool generates thumbnails at below normal priority, so they never compete with the foreground click you just made
- Every shell operation goes through Win32 directly instead of spawning PowerShell, which is the biggest single reason Explorer feels sluggish in practice

It also has a bit of personality. Eleven themes, each with its own folder color and font. The Retro theme bakes the Press Start 2P pixel arcade font into the binary. Custom folder icons, file type glyphs, drag and drop with a destination pane highlight, a contextual action bar when you select files, tabs, dual pane mode with a draggable splitter, a Recycle Bin browser, a bulk rename template engine, git badges, color tags, per file notes, a Mica window backdrop, a command palette, and a small NPU detector in the top right that reports your AI acceleration tier on startup.

## What is new

**Version 0.9.8** tightens the Storage analyzer layout so all category buckets fit the overview without scrolling, makes bucket drill-ins show compact top folder/app groups, verifies app drill-ins roll up folders instead of individual files, and trims UI-thread work that could cause small hitches when going back or switching tabs.

**Version 0.7.3** adds a Folder Color picker on the Appearance tab. You can override the per theme folder color with any hex value or one of eight presets, and the choice persists across launches.

**Version 0.7.2** added a real updater log at `%APPDATA%\Pathfinder\updater.log` so silent failures are visible without rebuilding with a console subsystem, tiered retry on update check failure (30 seconds, 2 minutes, 10 minutes, then hourly) so a network blip during the first cycle no longer wastes an hour, explicit TLS 1.2 and 1.3 in the PowerShell call, an enlarged first run welcome dialog where every step box grows from its own text height instead of fixed pixel boxes, and a DialogButton that auto sizes to its label so long labels like Set as default folder handler no longer overlap.

**Version 0.7.1** added a first run welcome dialog that registers Pathfinder as the default folder handler in one click and walks you through pinning Pathfinder and unpinning File Explorer (Windows 11 does not let third party apps do those for you), redesigned the Retro folder as a chunky pixel art floppy and the High Fantasy folder as a wooden treasure chest, gave High Fantasy a new emerald and gold palette, added Gold, Indigo, and Crimson accents alongside Black and White, expanded the Performance tab with a plain English explanation of indexing plus real memory usage from GetProcessMemoryInfo, surfaced detected discrete GPUs in the AI tab, isolated the custom theme editor preview so picking colors no longer recolors the whole app, and wrapped the View tab in a ScrollView with explanatory copy.

**Version 0.7.0** added ONNX Runtime inference (DirectML first, CPU fallback), file name embeddings in SQLite for semantic ranking in the search bar, optional MobileNet based tag suggestions, dHash based duplicate image detection, a `--path` launch argument, and a Windows Settings panel to register Pathfinder as your per user default folder handler. The release also ships `extras/set-pathfinder-default-folder-handler.reg` for users who want to edit the registry by hand.

## Features

- **Native Slint UI.** No HTML, no WebView. Direct Slint rendering with a Windows 11 style shell and Mica blur.
- **Explorer style navigation.** Tabs, back and forward history, clickable breadcrumbs, address bar, Quick Access, drives, bookmarks, and a real status bar.
- **Multi select.** Ctrl click to toggle individual files, Shift click to extend a range, or Ctrl A to select everything.
- **Multiple views.** Icon grid, details list, gallery view, preview pane, and dual pane mode.
- **Inline rename.** F2 or the context menu renames in place without a dialog.
- **Fast folders.** Directory caching using the Windows FindFirstFileExW cache, background file watching, and a SQLite index of visited directories for instant cross directory search.
- **Search.** Windows Search integration where available, a recursive fallback scan, and prefix filters: `ext:`, `kind:`, `size:`, `name:`, `content:`, `modified:`, `tag:`. An optional semantic mode (S) ranks indexed hits by on device MiniLM embeddings when Local AI models are installed.
- **Command palette.** Ctrl P runs any action by name. Results are ranked by relevance so exact matches always come first.
- **File tools.** Copy, cut, paste, rename, delete to Recycle Bin, new folder, archive actions, checksums, batch rename, duplicate finder, and a storage treemap.
- **Smart duplicate detection.** Three phase finder that groups by file size first, then compares a 64 KB partial hash, then reads the full file only for candidates that survived both filters.
- **File preview.** Text with binary detection, images with dimension metadata for files too large to inline, and generic metadata for everything else.
- **Tags and notes.** Local color tags and per file notes with visible indicators in every view.
- **Git badges.** File and folder status badges for repositories without any extension or daemon.
- **Themes.** Mica Dark, Mica Light, Frost, Warm, Flat, Terminal, Paper, Retro, High Fantasy, Sunset, and Cyberpunk. Each ships with its own folder icon style, palette, and font. Appearance also gives you accent color, density, and a folder color override.
- **Drag and drop.** Drop files from File Explorer or between dual panes. The destination pane lights up while you hover, and holding Ctrl copies instead of moves.
- **Recycle Bin browser.** A sidebar entry that lists everything in the OS trash with the deletion time and the original folder visible, plus restore, delete permanently, and empty actions.
- **Streamed directory loading.** Very large folders show the first 2500 entries instantly and fill in progressively in chunks of 2000 with parallel metadata reads.
- **Natural sort order.** file2.txt sorts before file10.txt, matching what File Explorer does.
- **Bulk rename with templates.** Apply a template like `IMG_{n:04}.{ext}` across a selection to renumber files in one go.
- **Conflict resolution.** Clear Skip, Replace, and Keep Both buttons when a paste or drop hits an existing file.
- **Settings panel.** Tabbed Appearance, View, Performance, and AI tabs, accessible from the toolbar or with Ctrl `,`.
- **AI status.** Detects NPU, GPU, or CPU acceleration on startup. The ONNX Runtime line gets merged into the AI tab explanation once models are loaded.
- **Local AI.** Optional model pack (text embedding, tokenizer, MobileNet classifier) with install progress in Settings. Embeddings and dHashes are written during background indexing.
- **Windows integration.** Shell extensions context menu, VSS Previous Versions, UAC and TrustedInstaller handling, taskbar and Start menu pinning, an optional per user default folder handler (`--path "%1"`), and `extras/set-pathfinder-default-folder-handler.reg` for manual registry import.
- **Win32 shell operations.** Properties dialog, shortcut creation, run as administrator, and clipboard writes all go through Win32 APIs directly with no PowerShell process overhead.
- **Admin features.** Retry operations as administrator, take ownership of TrustedInstaller files, and manage system protected items.
- **Battery aware indexing.** Background indexing pauses automatically when the device is on battery below 20 percent.
- **Thumbnail pool.** Image thumbnails are generated on a dedicated two thread pool running at below normal priority, so they never compete with foreground work.
- **Operation limits.** At most two heavy background operations run at the same time, so the app stays responsive during large scans or duplicate searches.
- **Auto updater.** Pathfinder checks GitHub Releases on launch and every hour after, with tiered retry on failure. When a newer version exists a small green pill appears in the status bar with an Install button that downloads the installer and runs it. Every cycle writes a timestamped line to `%APPDATA%\Pathfinder\updater.log` so failures are diagnosable.
- **Side mouse buttons.** XButton1 navigates back, XButton2 navigates forward. Both classic WM_XBUTTONDOWN events and the WM_APPCOMMAND form some mice send instead are handled.

## Download

Head to the [Releases](../../releases) page and grab the installer.

| File | Description |
|------|-------------|
| `Pathfinder_x.x.x_x64-setup.exe` | NSIS installer (recommended) |
| `Pathfinder_x.x.x_x64_en-US.msi` | Windows Installer package |
| `extras/set-pathfinder-default-folder-handler.reg` | Optional template to set Pathfinder as the default folder handler. Edit `CHANGEME_EXE` first, then import. |

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

The side mouse buttons (XButton1 and XButton2) also work for back and forward.

## Build from Source

Prerequisites: [Rust](https://rustup.rs) stable, Windows build tools, and the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/).

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
| UI | Slint 1 with the winit backend (femtovg renderer) |
| Index | SQLite via rusqlite (bundled), used for cross directory file search |
| Installer | NSIS and WiX MSI via the Tauri bundler |

A note on the renderer: the femtovg backend was chosen because the Skia backend's bundled ICU collides with symbols from the windows-rs crate at link time (duplicate `u_memcmp`, `u_unescapeAt`, and so on). femtovg keeps the build clean and the perf difference is small in practice for a file manager UI.

## AI Features

Pathfinder checks for local acceleration hardware at startup. If an NPU or a discrete GPU is available, the AI tab in Settings reports it. If nothing supported is found it falls back to CPU and stays fully functional. The optional Local AI model pack installs from inside the AI tab and only downloads when you ask for it.

You can point it at a custom runtime with:

```powershell
$env:PATHFINDER_LOCAL_AI_RUNTIME = "path\to\runtime"
```

Without it, the AI tab will show what was detected and explain why specific features are off.

## Windows Compatibility Notes

Pathfinder is built around the everyday File Explorer workflow, but some Windows behaviors sit outside what any third party app can control.

- Third party shell extensions may appear under "Show more options" rather than the primary context menu.
- Virtual shell locations, phones, cameras, and Control Panel views may open in Windows Explorer if they do not behave like normal filesystem folders.
- Previous Versions only appears when File History, server shadow copies, restore points, or VSS snapshots exist on the system.
- Some Properties tabs (Compatibility, vendor specific tabs) may open in the native Windows Properties dialog.
- Protected OS files can still be blocked by UAC, TrustedInstaller, Windows Resource Protection, or group policy regardless of what Pathfinder shows.
- Taskbar and Start pinning use only the verbs that Windows exposes to third party apps. Pathfinder cannot silently pin itself or unpin Explorer for you, so the first run welcome dialog walks you through the manual steps for both.

## Disclaimer

Pathfinder is provided as is, without warranty of any kind, express or implied. That includes the implied warranties of merchantability, fitness for a particular purpose, and non infringement. The authors and contributors are not liable for any claim, damage, or other liability arising from the use of this software, whether in contract, tort, or otherwise. You are running an early version of a file manager that touches real files on your real disk. Back up anything you care about. Bugs happen, especially around delete, move, and overwrite operations. If you find one, an issue on GitHub is the fastest way to get it fixed.

## License

MIT
