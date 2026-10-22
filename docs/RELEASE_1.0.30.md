# Pathfinder 1.0.30

- **Extracted folders open correctly after 0-byte stubs.** A leftover empty file named like the archive (from older extracts) no longer blocks Extract Here or steals double-clicks while Explorer opens the real `Name (1)` folder.
- **Directory junctions navigate like Explorer.** Symlink/junction folders are treated as folders again (not “0 B” files that ShellExecute).
- **Extract Here opens the new folder.** After a successful extract, Pathfinder navigates into the destination instead of leaving you on the parent listing.
- **Clearer empty-file warning.** Opening a true 0-byte file shows a short toast so incomplete extracts are obvious.

Download `Pathfinder_1.0.30_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.30_x64_en-US.msi` for Windows Installer deployment.
