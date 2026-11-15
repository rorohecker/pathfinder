# Pathfinder 1.0.33

- **Documents folder clicks no longer shift the layout.** Selecting a fully visible folder (including GitHub under Documents) no longer nudges the list/grid by the old “edge margin” scroll. Scroll-into-view only runs when the selection is actually clipped.
- **Icon grids keep a stable width.** Primary and secondary grid/gallery views reserve the same scrollbar gutter as list view and disable horizontal scrollbars, so opening a busy folder no longer reflows columns when the vertical bar appears.
- **Marquee matches the grid.** Rubber-band selection uses `AppMetrics.scroll_gutter` instead of a hard-coded 12px inset, so hit-testing lines up with the icons you see.
- **Grid keyboard navigation.** Up/Down move by a full row in grid/gallery/compact; Left/Right step one icon. List view Up/Down stay one row at a time.

Download `Pathfinder_1.0.33_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.33_x64_en-US.msi` for Windows Installer deployment.
