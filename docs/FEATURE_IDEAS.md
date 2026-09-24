# Pathfinder — feature ideas

Ideas identified while working on low-power mode (v1.0.20). Not commitments.

## Near-term (fit current chrome)

1. **Low power mode** — shipped in this pass (Auto / On / Off, status pill, resume indexing).
2. **List column show/hide persistence** — remember Name/Size/Modified/Type visibility per view (Phase F).
3. **In-app Properties sheet** — themed sheet instead of (or ahead of) the native dialog for common fields.
4. **Open With → Set default** — wire the shell default-app flow from the existing overlay.
5. **Shortcut Editor that binds chords** — Phase B: store real accelerators, not display hints only.
6. **Shell context verbs in Pathfinder menu** — Phase C: append capped `IContextMenu` verbs.

## Responsiveness / resources

7. **Adaptive thumbnail budget by GPU load** — extend low-power budgets when femtovg frame time spikes.
8. **Defer git status on battery** — even outside Saver, skip git badges until idle on AC.
9. **Pause folder watchers when occluded** — drop notify handles while minimized; re-arm on restore.
10. **Search result progressive ranking** — show first page before semantic/AI re-rank finishes.

## Discovery / organize

11. **Pinned smart folders on Home** — one-click saved searches on the landing view.
12. **Recents grouped by day** — Explorer-style date groups in the Recent overlay.
13. **Conflict policy preset** — remember last Skip/Replace/Keep Both choice for the session.

## Out of 1.x (still noted)

- CLIP visual search, multi-window shared session, Linux/macOS app, plugin host.
