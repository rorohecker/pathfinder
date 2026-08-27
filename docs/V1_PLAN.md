# Pathfinder 1.0.0 implementation plan

## Status

Phases A–E are implemented on this branch. Version stays **0.9.61** until Windows CI is green and the 1.0 bar is signed off. Authenticode is not faked.

Local working branch only. Do not push until the user asks.

This plan turns the 1.0 readiness audit into sequenced work. The product contract stays **Windows 11 file manager**. Linux/macOS, CLIP search, and a plugin host stay post-1.0.

## UX rules (non-negotiable)

- Reuse existing chrome: `ChoiceMenuRow`, tool overlay, confirm/prompt dialogs, settings cards, status bar, command palette.
- Do not add a second visual language (native Win32 popups only as a last-resort "Windows menu").
- Destructive actions keep Cancel as the left/safe action. Permanent delete uses a distinct title and "Delete forever" — not the Recycle Bin wording.
- Overlays that used to dump text into Preview become clickable lists in the existing tool overlay.
- Keyboard, density, themes, simple vs full mode, and Italian/Spanish must keep working.
- Prefer one more settings row over a new window.

## Already in good shape (do not rebuild)

Tabs, history, dual pane, grid virtualization, Recycle Bin, conflict Skip/Replace/Keep Both, indexed search, storage analyzer, archives, updater download, Local AI install, default folder handler, first-run simple/full.

## Phase A — Safety and everyday UX

1. Confirm dialog: `confirm_title` + action label. Recycle vs permanent vs empty-bin copy.
2. Block destructive shortcuts while any modal/overlay is open (extend the existing confirm/settings/welcome guards to tool overlay, compare, image tools).
3. Clickable tool overlays: Libraries, Recent locations, Breadcrumb siblings, Previous versions, Cloud state, Privacy.
4. Privacy + cache actions on Settings → Performance (clear thumbnails, clear caches, rebuild index, network-download policy).
5. Search/git truncation called out in the status bar ("first 25,000 matches").
6. Default index mode `balanced` for new installs (Max remains a choice).

## Phase B — Shortcuts that actually bind

Command palette "Shortcut Editor" currently edits display hints only.

- Store chords in `shortcuts.json` (`Ctrl+Shift+N` form).
- Record UI: click a row, press a chord, Save.
- Runtime dispatcher on the window FocusScope; defaults match today's KeyBindings.
- Escape / arrows / Return stay in Slint because they depend on overlay state.
- Palette hints come from the live map.

## Phase C — Windows shell honesty

Keep Pathfinder's themed menu for Open/Copy/Cut/Tags/Delete.

- Query `IContextMenu` and append extra verbs (7-Zip, Share, …) as `ChoiceMenuRow`s.
- Cap extra verbs (~12) so the menu stays scrollable, not a novel.
- "Windows menu" runs `TrackPopupMenu` for owner-draw / nested extensions.
- "Show more options" remains Explorer fallback.
- Share uses the shell `share` verb when present.
- Previous versions: overlay list + Restore (background, then refresh).
- UAC/pin: keep best-effort; do not block the UI thread with PowerShell.

## Phase D — Preview and i18n

- Video/audio: shell thumbnail (`IShellItemImageFactory`) in the preview pane + existing "Play in default app".
- i18n phase 2: `GetUserDefaultUILanguage` for System; leftover Slint English; more `i18n::t` for toasts/overlays.
- Close GitHub issue #3 once translations cover Rust-fed chrome.

## Phase E — Performance and release trust

- Details/list view: windowed rows (same pattern as the grid) so huge folders do not mount every `FileRow`.
- Gallery already uses the grid window; keep it.
- Updater: verify GitHub asset `digest` (SHA-256) when present, plus existing size checks. Authenticode signing needs a cert — document, do not fake.
- CI: `cargo test --locked` after clippy.
- Tests: shortcut parse/dispatch, installer digest parse, overlay path helpers.

## Phase F — After this branch (still 1.0, separate passes)

- In-app Properties sheet (or faster native sheet).
- Set-default-app from Open With.
- List column show/hide persistence.
- Skia renderer (ICU clash) — do not attempt here.
- Split `lib.rs` — maintainer-only, after tests exist.
- Code-signing the NSIS/MSI in GitHub Actions.

## Out of 1.0

Linux/macOS app, CLIP visual search, browser file-picker plugin, Directory Opus-level automation, true multi-window shared session.
