# Pathfinder

A fast, modern file explorer for Windows built with Tauri 2 and a vanilla JS frontend.

## Features

- **Virtual scroll** — renders only visible rows/cards; handles folders with thousands of files without slowdown
- **Parallel search** — Rayon-powered multithreaded recursive search with live cancellation when you type a new query
- **Windows Search integration** — queries the Windows Search index for instant results; falls back to manual scan
- **Directory cache + prefetch** — visited folders are cached (20 s TTL) and likely-next subdirs are preloaded in the background; file watchers keep the cache coherent
- **Thumbnail cache** — image thumbnails generated in parallel off the UI thread, cached and reused across navigations
- **Preview pane** — text files, images, and metadata shown inline; Space bar for Quick Look overlay
- **Tabs** — multiple independent navigation sessions with full back/forward history each
- **Tags** — color-coded labels (Urgent, Important, Review, Done, Personal, Code) stored locally per path
- **9 themes** — Mica Dark, Mica Light, Warm, Flat, Terminal, Paper, Retro, Fantasy, Cyberpunk
- **Command palette** — `Ctrl+P` to jump to any action
- **Rubber-band selection** — drag to select multiple files
- **AI features** (NPU required) — semantic search, automatic summaries, image classification, local embeddings; gracefully disabled when no supported NPU is detected

## Download

Go to the [**Releases**](../../releases) page and download the installer for your system:

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

**Prerequisites:** [Rust](https://rustup.rs) (stable) · [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) (WebView2)

```powershell
git clone https://github.com/rorohecker/pathfinder.git
cd pathfinder/src-tauri
cargo tauri build
```

The installer is output to `src-tauri/target/release/bundle/`.

To run in development mode:

```powershell
cargo tauri dev
```

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Shell | [Tauri 2](https://v2.tauri.app) |
| Backend | Rust — Rayon, walkdir, notify, image |
| Frontend | Vanilla JS + CSS (no framework, no bundler) |
| Installer | NSIS / WiX MSI via Tauri bundler |

## AI Features

Pathfinder detects NPU hardware at startup using `Get-PnpDevice`. If a supported NPU is found, AI features can be enabled by setting:

```powershell
$env:PATHFINDER_LOCAL_AI_RUNTIME = "path\to\runtime"
```

Without this, the AI tab in Settings will show the detection result and explain why features are disabled.

## License

MIT
