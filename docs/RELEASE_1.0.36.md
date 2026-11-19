# Pathfinder 1.0.36

- **Date-group headers stay put on click.** Selecting a file in Documents (or any folder sorted by Date modified) no longer rebuilds the row via `file_item`, which was clearing Explorer-style Today/Yesterday/This week headers and shrinking rows by ~26px so the list jumped.
- **Header clicks no longer clear selection.** The Today/Yesterday strip absorbs pointer hits instead of falling through to empty-space clear (which rebuilt the listing and made groups flicker away).
- **Scroll-into-view ignores the header strip.** A fully visible file row no longer nudges the list just because its date label was 1px clipped.
- **Secondary pane gets the same date groups** when sorted by Date modified (or Home recent buckets).
- **Clear selection is lighter.** Deselect no longer forces a full listing rebuild unless cut-ghosting needs to clear.

Download `Pathfinder_1.0.36_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.36_x64_en-US.msi` for Windows Installer deployment.
