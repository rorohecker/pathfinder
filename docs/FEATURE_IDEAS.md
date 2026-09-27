# Pathfinder — feature ideas & known bugs

Updated with dual-pane filter, low-power git deferral, Home smart pins, and safe Explorer bypass (v1.0.22).

## Known bugs (addressed this pass)

1. Settings → View OPTIONS nested LIST COLUMNS inside Theme animations (garbled overlapping labels) — fixed.
2. Dual-pane keyboard drives the active pane (Arrow/Enter).
3. Secondary pane respects Show Hidden.
4. Secondary navigate opens `home://` / `recycle://` / archives.
5–6. Recycle Bin identity uses `recycle://id/<b64>/o/<b64>`; delete undo records `trash_id`.
7. `home://` is a virtual nav path; F5 refreshes Home.
8. Recycle formatting keys off entry path (works in either pane).
9. Settings/session JSON queue flushes on quit.
10. Shortcut dispatch uses in-memory `shortcut_draft` (no disk read per key).
11. High-traffic Rust toasts go through `i18n::t`.

## Features shipped this pass

1. **List column show/hide persistence** — Size/Modified/Type toggles in Settings → View.
2. **In-app Properties sheet** — tool overlay with size, dates, tag, copy path (+ Windows Properties).
3. **Open With → Set as default** — overlay offers choose-once vs register-as-default.
4. **Session conflict policy** — “Remember for this session” on Skip/Replace/Keep Both.
5. **Dual-pane folder filter** — secondary pane has its own filter row; primary filter bar wired from the toolbar.
6. **Pause folder watchers while minimized** — drop notify watchers when occluded; re-arm on restore.
7. **Defer git status on low power** — no porcelain spawn / badges while Saver is active; resumes on toggle off.
8. **User-pinned smart folders on Home** — Pin/Home toggle in Smart Folders overlay; persisted in `home_smart_pins.json`.
9. **Recents grouped by day** — Home + Recent Locations overlay use day buckets; visits store timestamps.
10. **Auto-refresh on folder change** — watched-directory notify events soft-refresh the listing (debounced); banner only while inline rename is active.
11. **Safe Explorer bypass** — HKCU folder/drive verbs + App Paths redirect; unhandled `shell:` / CLSID / unknown explorer verbs forward to `C:\Windows\explorer.exe` (never replaces the system binary).

## Shipped in follow-up (v1.0.22+)

- **Sticky selection** — soft refresh / F5 remaps selection by path (also remaps across folder-filter edits).
- **Workspace layouts** — save/restore dual pane, secondary path, splitter, active tab (missing secondary falls back).
- **Undo history overlay** — clickable stack undoes down to the chosen step + Clear.
- **Flat view** — recursive listing under the current folder (capped at 8k), toolbar + Ctrl+Shift+L; async walk; F5/watch keep flat mode.
- **Home smart pins** — open from Home navigates to a real scope (home/Downloads) before searching.
- **Secondary sticky selection** — soft F5/watch refresh remaps secondary selection by path (no focus steal).
- **Operation queue overlay** — clickable tool overlay with pause/resume/cancel + reveal source (replaces preview dump).

## Still open / later

- Code-signing NSIS/MSI in CI (needs cert).
- Skia renderer (ICU clash with windows-rs).
- Optional Win+E hotkey (shell-level; separate from App Paths).
- Custom list columns (tags / notes / git).
- Multi-window shared session (new window already spawns a process).
