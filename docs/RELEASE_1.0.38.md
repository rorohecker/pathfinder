# Pathfinder 1.0.38

- **Documents → GitHub double-click stays stable.** Thumbnail/git enrichment no longer rebuilds the whole file model (which recycled list rows and cleared date-group layout between click 1 and click 2). Icons/badges patch in place instead.
- **No false scroll on visible folders.** Selection scroll-into-view skips rows already in the virtualized window and refuses the old `index × row height` guess when date-group offsets are missing.
- **Soft folder refresh waits out a double-click** so a progressive Documents listing cannot swap under the cursor mid-open.
- **Selected folders arm drag after 550ms** so DoDragDrop does not steal a Documents folder double-click.

Download `Pathfinder_1.0.38_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.38_x64_en-US.msi` for Windows Installer deployment.
