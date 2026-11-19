# Pathfinder 1.0.35

- **Folders delete like Explorer.** Recycle and permanent delete now drop Pathfinder’s own directory watchers on the target first (those handles blocked folders Explorer could remove), clear read-only attributes the way Explorer does, and retry once if the shell skipped the item under `FOF_NO_UI`.
- **Safer delete when a pane is inside the folder.** If a tab or pane is viewing the folder being deleted (or a child of it), Pathfinder navigates to the parent before the shell op so the delete is not fighting an open view.
- **Clearer delete UX.** Context-menu Delete sits with Cut/Copy again (not buried under Advanced), confirm copy distinguishes folders, and failed deletes name the item instead of a vague abort.
- **UI contrast across themes.** Shared `hover` / `hover_strong` / `pressed` tokens replace hard-coded white overlays that vanished on light themes; secondary dialog buttons get a border; faint text and borders are stronger on mica-light, warm, paper, flat, terminal, sunset, and mica-dark; custom themes derive a real `panel_alt` and keep opaque borders opaque.

Download `Pathfinder_1.0.35_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.35_x64_en-US.msi` for Windows Installer deployment.
