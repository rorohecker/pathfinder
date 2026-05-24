# Making Pathfinder Your Default File Manager

This guide explains how to configure Pathfinder as your default file manager across Windows and common applications.

## Windows Desktop Integration

### Method 1: GUI Settings (Easiest)
1. Open Pathfinder
2. Press `Ctrl+,` to open Settings
3. Click the "Windows" tab
4. Click "Set as default folder handler"
5. Pathfinder is now your default file manager

This handles:
- Double-clicking folders from the desktop
- Double-clicking drives from "This PC"
- "Open folder" from File Explorer shortcuts
- Folder shortcuts (.lnk files)

### Method 2: Registry File
1. Download or create `set-pathfinder-default-folder-handler.reg`
2. Double-click to import (Windows will prompt for confirmation)
3. Click "Yes" to merge into the registry

See `extras/set-pathfinder-default-folder-handler.reg` for the pre-made file.

### Method 3: Manual Registry Edits
Advanced users can edit the registry directly:
- **Path**: `HKEY_CURRENT_USER\Software\Classes\Folder\shell\open\command`
- **Value**: `"C:\path\to\pathfinder.exe" --path "%1"`

Repeat for these registry paths:
- `Folder\shell\open\command`
- `Folder\shell\explore\command`
- `Directory\shell\open\command`
- `Directory\shell\explore\command`
- `Drive\shell\open\command`
- `Drive\shell\explore\command`

## Browser Integration

### Chrome / Edge / Chromium-based Browsers

#### "Show in folder" Support
Pathfinder automatically works with "Show in folder" context menu in Chrome and Edge when set as your default folder handler.

**Requirements:**
- Set Pathfinder as the default folder handler (see Windows Desktop Integration above)
- No additional configuration needed

**How it works:**
- When you right-click a download and select "Show in folder", Chrome invokes Pathfinder with `/select <file>` command
- Pathfinder opens the file's parent directory and highlights the file

#### File Picker Integration (Advanced)

To make Pathfinder the default file picker for upload dialogs:

1. **Via Windows File Dialog API:**
   - Set Pathfinder as default folder handler
   - When Chrome uses the native file picker, it uses your system default

2. **Via Browser Extension (Future):**
   - A custom Pathfinder browser extension could replace the standard file picker
   - Currently, users should use the native file dialog

### Firefox

Firefox uses the GTK file dialog on Linux and NSFilePanel on macOS, but on Windows it can be configured to use the system file picker.

**Enable system file picker in Firefox:**
1. Type `about:config` in the address bar
2. Search for `widget.windows.use-native-filepicker`
3. Set it to `true` (it may already be enabled)
4. Restart Firefox

Firefox will now use the Windows file picker, which routes through Pathfinder when it's your default handler.

## Application Integration

### Support for "Open Folder" / "Show in Folder"

Pathfinder supports the standard Windows conventions that many applications use:

**Command Line Arguments:**
```
pathfinder.exe --path "C:\path\to\folder"     # Open a specific folder
pathfinder.exe /select "C:\path\to\file.txt"  # Open folder and select file
pathfinder.exe "C:\path\to\item"               # Auto-detect (file = select parent, folder = open)
```

**Applications using these patterns:**
- Visual Studio Code: "Reveal in Explorer" 
- JetBrains IDEs: "Show in Explorer"
- Sublime Text: "Open Folder"
- 7-Zip: "Open archive folder"
- WinRAR: "Open containing folder"
- Steam: "Show in folder"
- Discord: "Show in folder"
- Slack: "Show in folder"
- Notepad++: "Open containing folder"
- Total Commander: Custom integration (see below)

### Visual Studio & JetBrains IDEs

**VS Code:**
1. Install the "Open Folder with Pathfinder" extension (if available)
2. Or configure in `settings.json`:
```json
{
  "explorer.openEditors.defaultOrder": "editorOrder",
  "terminal.external.windowsExec": "C:\\path\\to\\pathfinder.exe"
}
```

**JetBrains (IntelliJ, PyCharm, etc.):**
1. Settings -> Tools -> External Tools
2. Add a new tool:
   - Name: "Open in Pathfinder"
   - Program: `C:\path\to\pathfinder.exe`
   - Arguments: `--path "$ProjectFileDir$"`
   - Working directory: `$ProjectFileDir$`
3. Or install the "Open in Terminal" plugin and configure it

### Windows Terminal

To open a folder in Pathfinder from Windows Terminal:

**Add to `settings.json`:**
```json
{
  "actions": [
    {
      "command": {
        "action": "openSettings",
        "target": "settingsui"
      }
    },
    {
      "command": "openNewTabProfile",
      "id": "open-pathfinder",
      "keybindings": ["ctrl+shift+p"]
    }
  ]
}
```

Or create a quick launch alias:
```powershell
# In your PowerShell profile
function pathfinder {
  & "C:\path\to\pathfinder.exe" --path (Get-Location).Path
}
```

## Troubleshooting

### "Set as default folder handler" button does nothing
1. Check that Pathfinder has permission to write to HKEY_CURRENT_USER registry
2. Try the registry file method (`extras/set-pathfinder-default-folder-handler.reg`)
3. Restart Windows or log off and back on

### Applications still open in File Explorer
1. Verify the registry settings were applied:
   - Press `Win+R`, type `regedit`
   - Navigate to `HKEY_CURRENT_USER\Software\Classes\Folder\shell\open\command`
   - Check that the value points to your Pathfinder installation
2. Restart the application
3. Some applications may have their own hardcoded folder handler - this limitation is app-specific

### "Show in folder" doesn't work in Chrome
1. Ensure Pathfinder is set as the default folder handler
2. Restart Chrome completely (check Task Manager - kill all Chrome processes)
3. Try right-clicking a download again

## Advanced: Command Line Reference

Full list of command line arguments Pathfinder recognizes:

```
pathfinder.exe --path "C:\Users\Documents"              # Open folder
pathfinder.exe --path=C:\Users\Documents                # Same, with = syntax
pathfinder.exe /select "C:\Users\file.txt"              # Open folder, select file
pathfinder.exe /select,"C:\Users\file.txt"              # Same, comma variant
pathfinder.exe "C:\Users\file.txt"                      # Auto (bare path)
```

## Technical Details

### Windows Shell Integration
When Pathfinder is set as the default folder handler, it intercepts these shell verbs:
- `Folder\shell\open\command` - Generic folder open (double-click)
- `Folder\shell\explore\command` - Folder explore verb
- `Directory\shell\open\command` - File system folder open
- `Directory\shell\explore\command` - File system folder explore
- `Drive\shell\open\command` - Drive open (double-click in This PC)
- `Drive\shell\explore\command` - Drive explore verb

### Architecture
- **Entry point**: Windows calls `pathfinder.exe --path "%1"` with the folder path
- **Argument parsing**: Multi-format parser accepts `--path`, `/select,`, bare paths
- **Fallback**: If Pathfinder can't read a path, it gracefully defaults to the home directory

### Performance
- Cold start: ~150-300ms depending on system
- Directory listing: First 2,500 entries in <100ms
- Full index: Lazy-loaded in background, doesn't block UI

## Limitations and Known Issues

1. **Windows Sandbox / Virtual Machines**: Some virtua machines may have file system integration issues
2. **Network paths**: UNC paths like `\\server\share` work but may be slower
3. **Special folders**: Virtual shell folders (Control Panel, etc.) open in Explorer (by design)
4. **System Dialogs**: Some system file picker dialogs still use Explorer (app-specific)

## Contributing Integrations

If you've successfully integrated Pathfinder with an application not listed here, please contribute:
1. Open an issue describing the integration
2. Include the command-line syntax or registry changes used
3. Submit a PR to update this documentation

## Future Enhancements

Planned features for better integration:
- [ ] Shell context menu items (Copy path, Open in terminal, etc.)
- [ ] Custom file picker extension for browsers
- [ ] Plugin system for IDE integration
- [ ] Drag-drop to web browser address bar
- [ ] Cross-platform (Linux, macOS) file manager on Wayland/X11
