# Pathfinder — feature ideas & known bugs

Updated with bug fixes + features 1/2/3/4/6/9 (v1.0.20+). Not commitments.

## Known bugs (addressed this pass)

1. Dual-pane keyboard drives the active pane (Arrow/Enter).
2. Secondary pane respects Show Hidden.
3. Secondary navigate opens `home://` / `recycle://` / archives.
4–5. Recycle Bin identity uses `recycle://id/<b64>/o/<b64>`; delete undo records `trash_id`.
6. `home://` is a virtual nav path; F5 refreshes Home.
7. Recycle formatting keys off entry path (works in either pane).
8. Settings/session JSON queue flushes on quit.
9. Shortcut dispatch uses in-memory `shortcut_draft` (no disk read per key).
10. High-traffic Rust toasts go through `i18n::t`.

## Features shipped this pass

1. **List column show/hide persistence** — Size/Modified/Type toggles in Settings → View.
2. **In-app Properties sheet** — tool overlay with size, dates, tag, copy path (+ Windows Properties).
3. **Open With → Set as default** — overlay offers choose-once vs register-as-default.
4. **Session conflict policy** — “Remember for this session” on Skip/Replace/Keep Both.
6. **Pause folder watchers while minimized** — drop notify watchers when occluded; re-arm on restore.
9. **Recents grouped by day** — Home + Recent Locations overlay use day buckets; visits store timestamps.

## Still open / later

5. Dual-pane parity extras (folder filter on secondary).
7. Defer git status on battery / low power.
8. User-pinned smart folders on Home.
10. (Done via bugs 4–5) Stable Recycle Bin + undo by trash id.
