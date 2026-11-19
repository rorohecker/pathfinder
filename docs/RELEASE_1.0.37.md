# Pathfinder 1.0.37

- **Windows ancestry matching is separator-safe.** `path_is_within` normalizes `/` to `\` before the prefix check, so delete-watch release and related folder checks stay correct when paths mix separators (and Windows CI stays green).
- Includes **v1.0.36**: date-group headers in Documents no longer vanish or jump the list on click.

Download `Pathfinder_1.0.37_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.37_x64_en-US.msi` for Windows Installer deployment.
