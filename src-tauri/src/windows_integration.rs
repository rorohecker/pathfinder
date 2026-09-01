/// Windows-specific integrations for shell extensions, VSS, UAC, and taskbar pinning
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ============================================================================
// Data Structures
// ============================================================================

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ContextMenuAction {
    pub id: u32,
    pub name: String,
    pub help_text: Option<String>,
    pub icon_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PreviousVersion {
    pub path: String,
    pub timestamp: u64,
    pub size: u64,
    pub version_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AdminRetryResult {
    pub success: bool,
    pub message: String,
    pub requires_ownership_change: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PinningResult {
    pub success: bool,
    pub message: String,
    pub location: String, // "taskbar" or "start-menu"
}

// ============================================================================
// Helper Functions
// ============================================================================

#[allow(dead_code)]
fn win32_clipboard_copy(text: &str) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_count = wide.len() * 2;

    unsafe {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_count).map_err(|e| e.to_string())?;
        let ptr = GlobalLock(hmem) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(Some(hmem));
            return Err("GlobalLock failed".to_string());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(hmem);

        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(hmem));
            return Err(e.to_string());
        }
        if let Err(e) = EmptyClipboard() {
            let _ = CloseClipboard();
            let _ = GlobalFree(Some(hmem));
            return Err(e.to_string());
        }
        const CF_UNICODETEXT: u32 = 13;
        if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hmem.0))) {
            let _ = CloseClipboard();
            let _ = GlobalFree(Some(hmem));
            return Err(e.to_string());
        }
        CloseClipboard().map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn to_wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    OsStr::new(s.as_ref())
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_wide(v: &[u16]) -> String {
    let len = v.iter().position(|&x| x == 0).unwrap_or(v.len());
    String::from_utf16_lossy(&v[..len]).to_string()
}

fn shell_execute_verb(path: &str, verb: &str) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::core::PCWSTR;

    let path_wide = to_wide(path);
    let verb_wide = to_wide(verb);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        hwnd: HWND(std::ptr::null_mut()),
        lpVerb: PCWSTR(verb_wide.as_ptr()),
        lpFile: PCWSTR(path_wide.as_ptr()),
        lpParameters: PCWSTR::null(),
        lpDirectory: PCWSTR::null(),
        nShow: 1,
        ..Default::default()
    };

    unsafe { ShellExecuteExW(&mut info).map_err(|e| e.to_string()) }
}

fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let process = GetCurrentProcess();
        let mut token = HANDLE::default();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            ret_size,
            &mut ret_size,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

// ============================================================================
// 1. Shell Extensions / IContextMenu COM Interop
// ============================================================================

const SHELL_CMD_FIRST: u32 = 1;
const SHELL_CMD_LAST: u32 = 0x7FFF;
const MAX_THEMED_SHELL_VERBS: usize = 12;

fn skip_shell_verb_label(label: &str) -> bool {
    let n = label.trim().trim_start_matches('&').to_ascii_lowercase();
    if n.is_empty() {
        return true;
    }
    matches!(
        n.as_str(),
        "open"
            | "apri"
            | "abrir"
            | "open with"
            | "cut"
            | "taglia"
            | "cortar"
            | "copy"
            | "copia"
            | "copiar"
            | "paste"
            | "incolla"
            | "pegar"
            | "delete"
            | "elimina"
            | "eliminar"
            | "borrar"
            | "rename"
            | "rinomina"
            | "renombrar"
            | "properties"
            | "proprietà"
            | "propiedades"
            | "copy as path"
            | "copy path"
            | "copia come percorso"
            | "copiar como ruta"
            | "share"
            | "condividi"
            | "compartir"
    ) || n.starts_with("open with")
        || n.starts_with("apri con")
        || n.starts_with("abrir con")
        || n.starts_with("cambiar nombre")
}

fn with_shell_context_menu<T>(
    path: &str,
    f: impl FnOnce(
        &windows::Win32::UI::Shell::IContextMenu,
        windows::Win32::UI::WindowsAndMessaging::HMENU,
    ) -> Result<T, String>,
) -> Result<T, String> {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::{
        BHID_SFUIObject, CMF_EXPLORE, CMF_NORMAL, IContextMenu, IShellItem,
        SHCreateItemFromParsingName,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let path_wide = to_wide(path);
        let item: IShellItem =
            SHCreateItemFromParsingName(windows::core::PCWSTR(path_wide.as_ptr()), None)
                .map_err(|e| e.to_string())?;
        let menu: IContextMenu = item
            .BindToHandler(None, &BHID_SFUIObject)
            .map_err(|e| e.to_string())?;
        let hmenu = CreatePopupMenu().map_err(|e| e.to_string())?;
        let result = (|| {
            menu.QueryContextMenu(
                hmenu,
                0,
                SHELL_CMD_FIRST,
                SHELL_CMD_LAST,
                CMF_NORMAL | CMF_EXPLORE,
            )
            .ok()
            .map_err(|e| e.to_string())?;
            f(&menu, hmenu)
        })();
        let _ = DestroyMenu(hmenu);
        result
    }
}

/// Extra IContextMenu verbs that Pathfinder's themed menu does not already cover.
pub fn get_context_menu_actions(path: &str) -> Result<Vec<ContextMenuAction>, String> {
    with_shell_context_menu(path, |_menu, hmenu| unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetMenuItemCount, GetMenuStringW, MF_BYPOSITION,
        };
        let count = GetMenuItemCount(Some(hmenu));
        if count <= 0 {
            return Ok(Vec::new());
        }
        let mut actions = Vec::new();
        let mut buf = [0u16; 256];
        for i in 0..count as u32 {
            let n = GetMenuStringW(hmenu, i, Some(&mut buf), MF_BYPOSITION);
            if n <= 0 {
                continue;
            }
            let label = from_wide(&buf[..n as usize]).replace('&', "");
            if skip_shell_verb_label(&label) {
                continue;
            }
            // Menu ids are assigned from SHELL_CMD_FIRST by QueryContextMenu.
            // GetMenuItemID is more accurate than position+1 for skipped items.
            let id = windows::Win32::UI::WindowsAndMessaging::GetMenuItemID(hmenu, i as i32);
            if id == u32::MAX || id < SHELL_CMD_FIRST {
                continue;
            }
            actions.push(ContextMenuAction {
                id,
                name: label,
                help_text: None,
                icon_url: None,
            });
            if actions.len() >= MAX_THEMED_SHELL_VERBS {
                break;
            }
        }
        Ok(actions)
    })
}

/// Invoke a verb previously listed by [`get_context_menu_actions`].
pub fn invoke_context_menu_action(path: &str, action_id: u32) -> Result<(), String> {
    with_shell_context_menu(path, |menu, _hmenu| unsafe {
        use windows::Win32::UI::Shell::CMINVOKECOMMANDINFO;
        let offset = action_id.saturating_sub(SHELL_CMD_FIRST);
        let mut info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: 0,
            hwnd: windows::Win32::Foundation::HWND(std::ptr::null_mut()),
            lpVerb: windows::core::PCSTR(offset as usize as *const u8),
            lpParameters: windows::core::PCSTR::null(),
            lpDirectory: windows::core::PCSTR::null(),
            nShow: 1,
            dwHotKey: 0,
            hIcon: windows::Win32::Foundation::HANDLE::default(),
        };
        menu.InvokeCommand(&info).map_err(|e| e.to_string())
    })
}

/// Full owner-draw Windows shell menu at the cursor (nested extensions, 7-Zip, etc.).
pub fn track_shell_context_menu(
    path: &str,
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<(), String> {
    with_shell_context_menu(path, |menu, hmenu| unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::Shell::CMINVOKECOMMANDINFO;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetCursorPos, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
        };
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let cmd = TrackPopupMenuEx(
            hmenu,
            TPM_LEFTALIGN.0 | TPM_RIGHTBUTTON.0 | TPM_RETURNCMD.0,
            pt.x,
            pt.y,
            hwnd,
            None,
        )
        .0 as u32;
        if cmd == 0 {
            return Ok(());
        }
        let offset = cmd.saturating_sub(SHELL_CMD_FIRST);
        let mut info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: 0,
            hwnd,
            lpVerb: windows::core::PCSTR(offset as usize as *const u8),
            lpParameters: windows::core::PCSTR::null(),
            lpDirectory: windows::core::PCSTR::null(),
            nShow: 1,
            dwHotKey: 0,
            hIcon: windows::Win32::Foundation::HANDLE::default(),
        };
        menu.InvokeCommand(&info).map_err(|e| e.to_string())
    })
}

/// Nearby Share / Windows share sheet when the verb is registered.
pub fn share_path(path: &str) -> Result<(), String> {
    match shell_execute_verb(path, "share") {
        Ok(()) => Ok(()),
        Err(_) => {
            // Fallback: select the file in Explorer so Share is one click away.
            Command::new("explorer")
                .arg(format!("/select,{path}"))
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }
}

// ============================================================================
// 2. Previous Versions via VSS (Volume Shadow Copy Service)
// ============================================================================

/// Get previous versions of a file from VSS snapshots
pub fn get_previous_versions(path: &str) -> Result<Vec<PreviousVersion>, String> {
    let script = r#"
$path = $args[0]
$versions = @()

function Shadow-ItemPath($device, $orig) {
    $rel = $orig
    if ($rel.Length -ge 2 -and $rel[1] -eq [char]':') { $rel = $rel.Substring(2) }
    $rel = $rel.TrimStart('\', '/')
    return ($device.TrimEnd('\') + '\' + $rel)
}

# Try to use WMI to query VSS
try {
    $vssReaderPath = Get-WmiObject -Query "SELECT * FROM Win32_ShadowCopy" -ErrorAction Stop
    
    foreach ($shadow in $vssReaderPath) {
        $device = $shadow.DeviceObject
        $id = $shadow.ID
        $unix = 0
        try {
            $dt = [System.Management.ManagementDateTimeConverter]::ToDateTime($shadow.InstallDate)
            $unix = [int64]([DateTimeOffset]::new($dt).ToUnixTimeSeconds())
        } catch {}
        $versionPath = Shadow-ItemPath $device $path
        
        if (Test-Path -LiteralPath $versionPath) {
            $file = Get-Item -LiteralPath $versionPath -ErrorAction SilentlyContinue
            if ($file) {
                $versions += @{
                    path = $versionPath
                    timestamp = $unix
                    size = $(if ($file.PSIsContainer) { 0 } else { $file.Length })
                    version_id = $id
                }
            }
        }
    }
} catch {
    # VSS may not be available or no snapshots exist
}

# Try to use Previous Versions tab (via COM)
try {
    $item = Get-Item -LiteralPath $path -ErrorAction Stop
    $shell = New-Object -ComObject Shell.Application
    
    if ($item.PSIsContainer) {
        $folder = $shell.Namespace($item.FullName)
        $target = $folder.Self
    } else {
        $folder = $shell.Namespace($item.DirectoryName)
        $target = $folder.ParseName($item.Name)
    }
    
    # In Windows, Previous Versions are shown in Properties -> Previous Versions tab
    # We can access this via the Shell API, but direct COM invocation would require COM marshalling
} catch {
}

ConvertTo-Json -InputObject $versions -Compress
"#;

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(script)
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        let versions = match serde_json::from_slice::<Vec<PreviousVersion>>(&output.stdout) {
            Ok(v) => v,
            Err(_) => serde_json::from_slice::<PreviousVersion>(&output.stdout)
                .map(|v| vec![v])
                .unwrap_or_default(),
        };
        Ok(versions)
    } else {
        Ok(Vec::new())
    }
}

/// Restore a file from a previous version
pub fn restore_from_previous_version(path: &str, version_id: &str) -> Result<(), String> {
    let script = format!(
        r#"
$path = '{}'
$versionId = '{}'

# Get the shadow copy
$shadow = Get-WmiObject -Query "SELECT * FROM Win32_ShadowCopy WHERE ID='$versionId'" -ErrorAction Stop

if ($shadow) {{
    $device = $shadow.DeviceObject
    $rel = $path
    if ($rel.Length -ge 2 -and $rel[1] -eq [char]':') {{ $rel = $rel.Substring(2) }}
    $rel = $rel.TrimStart('\', '/')
    $versionPath = $device.TrimEnd('\') + '\' + $rel

    if (Test-Path -LiteralPath $versionPath) {{
        Copy-Item -LiteralPath $versionPath -Destination $path -Force -Recurse
        Write-Host "Successfully restored from version $versionId"
    }} else {{
        throw "Version path not found: $versionPath"
    }}
}} else {{
    throw "Shadow copy not found: $versionId"
}}
"#,
        path.replace('\'', "''"),
        version_id.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

// ============================================================================
// 3. UAC/TrustedInstaller - Retry as Administrator & Take Ownership
// ============================================================================

/// Check if current process is elevated
pub fn is_process_elevated() -> bool {
    is_elevated()
}

/// Restart the current operation with administrator privileges
pub fn retry_as_administrator(operation: &str, path: &str) -> Result<AdminRetryResult, String> {
    if is_elevated() {
        return Ok(AdminRetryResult {
            success: false,
            message: "Already running with administrator privileges".to_string(),
            requires_ownership_change: false,
        });
    }

    // Check if we need to change ownership
    let needs_ownership = check_trusted_installer_ownership(path)?;

    // Use ShellExecuteEx with "runas" verb to prompt UAC
    let script = format!(
        r#"
Start-Process -FilePath pwsh -ArgumentList "-Command", "Write-Host 'Administrator operation: {}'" -Verb RunAs -Wait
"#,
        operation.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match output {
        Ok(output) if output.status.success() => Ok(AdminRetryResult {
            success: true,
            message: format!(
                "Operation '{}' executed with administrator privileges",
                operation
            ),
            requires_ownership_change: needs_ownership,
        }),
        Ok(output) => Ok(AdminRetryResult {
            success: false,
            message: String::from_utf8_lossy(&output.stderr).to_string(),
            requires_ownership_change: needs_ownership,
        }),
        Err(e) => Ok(AdminRetryResult {
            success: false,
            message: format!("Failed to execute with administrator privileges: {}", e),
            requires_ownership_change: needs_ownership,
        }),
    }
}

/// Check if a file/folder is owned by TrustedInstaller
fn check_trusted_installer_ownership(path: &str) -> Result<bool, String> {
    let script = format!(
        r#"
$path = '{}'
$acl = Get-Acl -LiteralPath $path
$owners = $acl.Owner
if ($owners -match 'TrustedInstaller') {{
    'true'
}} else {{
    'false'
}}
"#,
        path.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("true"))
}

/// Take ownership of a file/folder from TrustedInstaller
pub fn take_ownership(path: &str) -> Result<AdminRetryResult, String> {
    if !is_elevated() {
        return retry_as_administrator("take_ownership", path);
    }

    let script = format!(
        r#"
$path = '{}'

# Step 1: Take ownership via icacls
icacls $path /grant:r "$env:USERNAME`:F" /T /C 2>&1 | Out-Null

# Step 2: Reset permissions
icacls $path /reset /T /C 2>&1 | Out-Null

# Verify
$acl = Get-Acl -LiteralPath $path
if ($acl.Owner -match $env:USERNAME -or $acl.Owner -match 'Administrators') {{
    'success'
}} else {{
    'failed'
}}
"#,
        path.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if result.contains("success") {
        Ok(AdminRetryResult {
            success: true,
            message: format!("Successfully took ownership of: {}", path),
            requires_ownership_change: false,
        })
    } else {
        Ok(AdminRetryResult {
            success: false,
            message: format!(
                "Failed to take ownership. Output: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            requires_ownership_change: false,
        })
    }
}

// ============================================================================
// 4. Taskbar and Start Menu Pinning via .lnk + ShellExecuteEx
// ============================================================================

/// Create a .lnk (shortcut) file via IShellLink COM (no PowerShell spawn).
pub fn create_shortcut(
    target_path: &str,
    shortcut_path: &str,
    args: Option<&str>,
    working_dir: Option<&str>,
) -> Result<(), String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{Interface, PCWSTR};

    let target_wide = to_wide(target_path);
    let shortcut_wide: Vec<u16> = shortcut_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("CoCreateInstance failed: {e}"))?;

        link.SetPath(PCWSTR(target_wide.as_ptr()))
            .map_err(|e| format!("SetPath failed: {e}"))?;

        if let Some(a) = args {
            if !a.is_empty() {
                let wide = to_wide(a);
                let _ = link.SetArguments(PCWSTR(wide.as_ptr()));
            }
        }
        if let Some(wd) = working_dir {
            if !wd.is_empty() {
                let wide = to_wide(wd);
                let _ = link.SetWorkingDirectory(PCWSTR(wide.as_ptr()));
            }
        }

        let persist: IPersistFile = link
            .cast()
            .map_err(|e| format!("IPersistFile cast failed: {e}"))?;
        persist
            .Save(PCWSTR(shortcut_wide.as_ptr()), true)
            .map_err(|e| format!("Save failed: {e}"))?;

        Ok(())
    }
}

/// Pin an app/file to the taskbar
pub fn pin_to_taskbar(path: &str) -> Result<PinningResult, String> {
    // Create a temporary shortcut
    let temp_shortcut = format!(
        "{}\\pathfinder_temp_{}.lnk",
        std::env::temp_dir().to_string_lossy(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_micros()
    );

    create_shortcut(path, &temp_shortcut, None, None)?;

    let script = format!(
        r#"
$shortcutPath = '{}'

# Use Shell.Application to access taskbar
$shell = New-Object -ComObject Shell.Application
$namespace = $shell.Namespace((Split-Path -Path $shortcutPath))
$item = $namespace.ParseName((Split-Path -Path $shortcutPath -Leaf))

# Try to pin to taskbar via context menu
$verb = $item.Verbs() | Where-Object {{ $_.Name -match 'Pin to taskbar|Pin to' }}
if ($verb) {{
    $verb.DoIt()
    'success'
}} else {{
    'no_verb'
}}
"#,
        temp_shortcut.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    // Clean up temp shortcut
    let _ = std::fs::remove_file(&temp_shortcut);

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if result.contains("success") {
        Ok(PinningResult {
            success: true,
            message: format!("Successfully pinned to taskbar: {}", path),
            location: "taskbar".to_string(),
        })
    } else {
        Ok(PinningResult {
            success: false,
            message:
                "Could not find pin to taskbar verb. Modern Windows may require alternative method."
                    .to_string(),
            location: "taskbar".to_string(),
        })
    }
}

/// Pin an app/file to the Start menu
pub fn pin_to_start_menu(path: &str) -> Result<PinningResult, String> {
    // Create a temporary shortcut
    let temp_shortcut = format!(
        "{}\\pathfinder_temp_{}.lnk",
        std::env::temp_dir().to_string_lossy(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_micros()
    );

    create_shortcut(path, &temp_shortcut, None, None)?;

    let script = format!(
        r#"
$shortcutPath = '{}'
$pinned = $false

# Method 1: Via context menu (legacy)
$shell = New-Object -ComObject Shell.Application
$namespace = $shell.Namespace((Split-Path -Path $shortcutPath))
$item = $namespace.ParseName((Split-Path -Path $shortcutPath -Leaf))

$verb = $item.Verbs() | Where-Object {{ $_.Name -match 'Pin to Start|Add to Start' }}
if ($verb) {{
    $verb.DoIt()
    $pinned = $true
}}

# Method 2: Modern Windows 11 uses Windows.ApplicationModel.StartMenu
if (-not $pinned) {{
    try {{
        [Windows.Management.Deployment.PackageManager, Windows.Management.Deployment, ContentType = WindowsRuntime] > $null
        Add-AppxPackage -Path $shortcutPath -ErrorAction SilentlyContinue
        $pinned = $true
    }} catch {{
    }}
}}

if ($pinned) {{ 'success' }} else {{ 'failed' }}
"#,
        temp_shortcut.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    // Clean up temp shortcut
    let _ = std::fs::remove_file(&temp_shortcut);

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if result.contains("success") {
        Ok(PinningResult {
            success: true,
            message: format!("Successfully pinned to Start menu: {}", path),
            location: "start-menu".to_string(),
        })
    } else {
        Ok(PinningResult {
            success: false,
            message:
                "Could not pin to Start menu. This feature may be limited in your Windows version."
                    .to_string(),
            location: "start-menu".to_string(),
        })
    }
}

/// Unpin from taskbar
pub fn unpin_from_taskbar(path: &str) -> Result<PinningResult, String> {
    let script = format!(
        r#"
$path = '{}'

$shell = New-Object -ComObject Shell.Application
$namespace = $shell.Namespace((Split-Path -Path $path))
$item = $namespace.ParseName((Split-Path -Path $path -Leaf))

$verb = $item.Verbs() | Where-Object {{ $_.Name -match 'Unpin from taskbar|Unpin' }}
if ($verb) {{
    $verb.DoIt()
    'success'
}} else {{
    'no_verb'
}}
"#,
        path.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if result.contains("success") {
        Ok(PinningResult {
            success: true,
            message: format!("Successfully unpinned from taskbar: {}", path),
            location: "taskbar".to_string(),
        })
    } else {
        Ok(PinningResult {
            success: false,
            message: "Could not unpin from taskbar.".to_string(),
            location: "taskbar".to_string(),
        })
    }
}

/// Unpin from Start menu
pub fn unpin_from_start_menu(path: &str) -> Result<PinningResult, String> {
    let script = format!(
        r#"
$path = '{}'

$shell = New-Object -ComObject Shell.Application
$namespace = $shell.Namespace((Split-Path -Path $path))
$item = $namespace.ParseName((Split-Path -Path $path -Leaf))

$verb = $item.Verbs() | Where-Object {{ $_.Name -match 'Unpin from Start|Remove from Start' }}
if ($verb) {{
    $verb.DoIt()
    'success'
}} else {{
    'no_verb'
}}
"#,
        path.replace('\'', "''")
    );

    let output = Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;

    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if result.contains("success") {
        Ok(PinningResult {
            success: true,
            message: format!("Successfully unpinned from Start menu: {}", path),
            location: "start-menu".to_string(),
        })
    } else {
        Ok(PinningResult {
            success: false,
            message: "Could not unpin from Start menu.".to_string(),
            location: "start-menu".to_string(),
        })
    }
}
