# Pathfinder 1.0.31

- **Folders with commas open normally.** Pathfinder no longer mistakes a real folder name such as `RP-008099-DD-1-CM5 IO Board, revision 2, KiCAD files` for a comma-joined shell argument and redirects to its parent.
- **Clearer errors for invalid paths.** A missing comma-containing folder shows an error toast without replacing the current folder's empty-state text. Comma-joined file arguments still open the first file's parent when that file exists.

Download `Pathfinder_1.0.31_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.31_x64_en-US.msi` for Windows Installer deployment.
