# Pathfinder 1.0.28

- **Archive extract no longer leaves 0-byte files.** ZIP members are written to a hidden staging file, size-checked, then renamed into place. A failed decrypt, cancel, or corrupt entry no longer publishes an empty final file you can open by mistake.
- **Open files from inside an archive.** Double-click extracts a temporary copy and opens it with the default app (password prompt when needed). The secondary pane no longer tries to ShellExecute `archive://` virtual paths.
- **Extraction hardening.** Encrypted ZIPs are refused on the pure-Rust path without a password; unsafe entry paths are skipped and reported; 7-Zip progress uses uncompressed sizes when available.

Download `Pathfinder_1.0.28_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.28_x64_en-US.msi` for Windows Installer deployment.
