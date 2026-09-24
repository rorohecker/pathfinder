# Pathfinder — feature ideas & known bugs

Updated with the opt-in Low power toggle (v1.0.20). Not commitments.

## Known bugs to fix (priority)

1. **Dual-pane keyboard always drives the primary pane** — Arrow/Enter ignore `active_pane` and call primary `file_*` handlers.
2. **Secondary pane ignores Show Hidden** — only primary `apply_filter` respects `show_hidden`.
3. **Secondary navigate cannot open virtual locations** — `recycle://` / `home://` / `storage://` / archives early-return in `secondary_navigate_impl`.
4. **Recycle Bin identity is original-path only** — colliding trash entries; folders listed as files; undo/restore ambiguous.
5. **Delete undo never records `trash_id`** — Ctrl+Z after recreate-and-delete can restore the wrong item.
6. **`home://` omitted from `is_virtual_nav_path`** — F5 on Home silently fails through a useless `read_dir`.
7. **Recycle formatting keyed off primary path only** — wrong metadata when painting secondary rows.
8. **Settings/session writes can be lost on quit** — async writer every 250ms with no flush-on-exit.
9. **Shortcut dispatch re-reads `shortcuts.json` on every key** — disk I/O on the UI hot path.
10. **Most Rust toasts stay English** — Slint `@tr` works; everyday `show_toast` strings often skip `i18n::t`.

## Features worth adding

1. **List column show/hide persistence** — Size/Modified/Type visibility (and widths) in settings / `folder_views`.
2. **In-app Properties sheet** — themed overlay for size, dates, tags, path copy.
3. **Open With → Set as default** — wire shell default-app from the existing overlay.
4. **Session conflict policy** — remember Skip/Replace/Keep Both (optional apply-to-all).
5. **Dual-pane parity** — virtual locations, keyboard, show-hidden, folder filter (ties to bugs 1–3).
6. **Pause folder watchers while minimized** — drop/re-arm notify when occluded.
7. **Defer git status on battery / low power** — skip or idle-queue badges when low power is on.
8. **User-pinned smart folders on Home** — custom saved searches on the landing view.
9. **Recents grouped by day** — Explorer-style day groups.
10. **Stable Recycle Bin + undo by trash id** — encode `TrashItem.id` in virtual paths (bugs 4–5).

## Shipped this pass

- Opt-in **Low power mode** toggle (Settings → Performance; status-bar pill toggles it off/on).
