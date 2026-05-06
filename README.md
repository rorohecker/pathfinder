# Pathfinder

A Windows file explorer that doesn't get in your way. Built on Tauri 2 with a Rust backend and plain HTML/CSS/JS frontend.

## Features

- **Virtual scroll** - only renders what's visible, so huge folders stay snappy
- **Parallel search** - searches recursively across threads and cancels automatically when you type something new
- **Windows Search integration** - uses the built-in Windows index for instant results and falls back to a manual scan if needed
- **Directory cache and prefetch** - folders you've visited are cached and likely next folders are preloaded in the background; file watchers keep everything up to date
- **Thumbnail cache** - image previews are generated off the UI thread, cached, and reused so you're never waiting twice
- **Preview pane** - shows text files, images, and file info inline; hit Space for a full Quick Look overlay
- **Tabs** - open multiple folders at once, each with its own back/forward history
- **Tags** - color-coded labels (Urgent, Important, Review, Done, Personal, Code) saved locally per file
- **9 themes** - Mica Dark, Mica Light, Warm, Flat, Terminal, Paper, Retro, Fantasy, Cyberpunk
- **Command palette** - hit Ctrl+P and type anything
- **Rubber-band selection** - click and drag to select multiple files
- **AI features** (requires NPU) - semantic search, file summaries, image classification, and local embeddings; automatically disabled if no supported NPU is detected

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

**Prerequisites:** [Rust](https://rustup.rs) (stable) and [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) (WebView2)

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
| Shell | [Tauri 2](https://v2.tauri.app) |
| Backend | Rust (Rayon, walkdir, notify, image) |
| Frontend | Vanilla JS and CSS, no framework or bundler |
| Installer | NSIS / WiX MSI via Tauri bundler |

## AI Features

Pathfinder checks for NPU hardware on startup using `Get-PnpDevice`. If something is found, you can enable AI features by setting this environment variable:

```powershell
$env:PATHFINDER_LOCAL_AI_RUNTIME = "path\to\runtime"
```

Without it, the AI tab in Settings will tell you what was detected and why the features are off.

## License

MIT
