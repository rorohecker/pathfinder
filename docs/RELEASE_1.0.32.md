# Pathfinder 1.0.32

- **Storage paths open correctly.** Largest-item rows keep the real absolute path for Open / Reveal / Open with, and show a shortened path only as a label. Clicking large files (and tips like `pagefile.sys`) no longer navigates to a mangled `…\…` summary.
- **Windows folder noise.** The opaque `C:\Windows` rollup is hidden from Largest items; individual large system files remain findable with working paths.
- **Clicks no longer jostle icons.** Drag arming waits longer and needs more movement before DoDragDrop or marquee starts, and selected icons no longer grow/shift while a drag is active.
- **Storage QoL.** Profile buckets match case and localized folder names; tip markers match folder rollups; dominant-category tips drill into the matching bucket; Explorer reveal quotes paths with spaces.

Download `Pathfinder_1.0.32_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.32_x64_en-US.msi` for Windows Installer deployment.
