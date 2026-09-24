# Pathfinder — feature ideas & known bugs

Updated with Settings View label fix + auto-refresh on folder change (v1.0.21).

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
6. **Pause folder watchers while minimized** — drop notify watchers when occluded; re-arm on restore.
9. **Recents grouped by day** — Home + Recent Locations overlay use day buckets; visits store timestamps.
10. **Auto-refresh on folder change** — watched-directory notify events soft-refresh the listing (debounced); banner only while inline rename is active.

## Still open / later

5. Dual-pane parity extras (folder filter on secondary).
7. Defer git status on battery / low power.
8. User-pinned smart folders on Home.
10. (Done via bugs 5–6) Stable Recycle Bin + undo by trash id.
