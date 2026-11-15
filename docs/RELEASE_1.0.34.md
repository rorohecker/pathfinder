# Pathfinder 1.0.34

- **Tab titles follow the UI language.** Known folders no longer keep the OS leaf in the tab strip — English UI shows “Documents” even when the on-disk folder is `Documentos` / `Documenti`, and Spanish/Italian tabs use their own labels.
- **Virtual roots stay translated.** Home, Recycle Bin, and Storage in the address chrome use the active language instead of hard-coded English.
- **Slint catalogs filled in.** Spanish and Italian `.po` files now cover the remaining `@tr()` strings (filters, search help, privacy, Documents/Home, and related chrome) so es/it UI no longer falls back to English mid-screen.
- **Italian Downloads leaf.** On-disk `Scaricati` / `Scaricare` map to the Downloads known-folder label.

Download `Pathfinder_1.0.34_x64-setup.exe` for the standard Windows installer, or `Pathfinder_1.0.34_x64_en-US.msi` for Windows Installer deployment.
