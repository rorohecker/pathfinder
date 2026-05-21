//! Per-user (HKCU) overrides so Explorer opens folders with Pathfinder via `--path \"%1\"`.
//! Does not touch `HKCU\\...\\file\\shell\\open` — that would hijack all file opens.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE,
    REG_SZ, HKEY,
};
use windows::core::PCWSTR;

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Opens or creates `HKCU\\<relative>` one segment at a time. Returns a handle to the leaf key
/// (caller must `RegCloseKey` — never close `HKEY_CURRENT_USER`).
fn hkcu_open_create_leaf(relative_path: &str) -> Result<HKEY, String> {
    let segments: Vec<&str> = relative_path
        .split('\\')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return Err("empty registry path".into());
    }

    let mut parent = HKEY_CURRENT_USER;
    for seg in &segments {
        let wide = to_wide_nul(seg);
        let mut sub = HKEY::default();
        let err = unsafe {
            RegCreateKeyExW(
                parent,
                PCWSTR(wide.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut sub,
                None,
            )
        };
        if err != ERROR_SUCCESS {
            if parent != HKEY_CURRENT_USER {
                unsafe {
                    let _ = RegCloseKey(parent);
                }
            }
            return Err(format!(
                "RegCreateKeyExW failed for {:?}: {:?}",
                seg, err
            ));
        }
        if parent != HKEY_CURRENT_USER {
            unsafe {
                let _ = RegCloseKey(parent);
            }
        }
        parent = sub;
    }
    Ok(parent)
}

fn set_key_default_string(key: HKEY, value: &str) -> Result<(), String> {
    set_key_string(key, None, value)
}

fn set_key_string(key: HKEY, name: Option<&str>, value: &str) -> Result<(), String> {
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let nbytes = (wide.len() * std::mem::size_of::<u16>()) as u32;
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), nbytes as usize) };
    let name_wide = name.map(to_wide_nul);
    let name_ptr = name_wide
        .as_ref()
        .map(|w| PCWSTR(w.as_ptr()))
        .unwrap_or(PCWSTR::null());
    let err = unsafe { RegSetValueExW(key, name_ptr, None, REG_SZ, Some(bytes)) };
    if err != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW failed: {:?}", err));
    }
    Ok(())
}

/// Registry paths that drive folder navigation in Windows. Setting all of
/// them at HKCU level routes every Windows-native "open this folder" code
/// path (folder shortcuts, Chrome's "Show in folder", "Open file location"
/// in Start menu, double-clicked drives in This PC, anything that calls
/// ShellExecute on a directory) through Pathfinder.
///
///   - `Folder\shell\open\command`     — generic folder class, picked up by
///     ShellExecute("open", "C:\..."). The Folder class is what most apps
///     trigger when they want to reveal a directory.
///   - `Folder\shell\explore\command`  — same class, "explore" verb. Some
///     Win32 apps explicitly invoke this verb instead of "open".
///   - `Directory\shell\open\command`  — file-system directory class. Many
///     apps target this directly because the "Folder" alias can resolve to
///     virtual shell folders (Control Panel, etc) that we don't want to host.
///   - `Directory\shell\explore\command` — same as above for "explore".
///   - `Drive\shell\open\command`      — what double-clicking a drive in
///     This PC triggers. Without this entry, drives still open in Explorer
///     even when every folder above opens in Pathfinder.
///   - `Drive\shell\explore\command`   — same for "explore" verb on drives.
const FOLDER_HANDLER_PATHS: [&str; 6] = [
    r"Software\Classes\Folder\shell\open\command",
    r"Software\Classes\Folder\shell\explore\command",
    r"Software\Classes\Directory\shell\open\command",
    r"Software\Classes\Directory\shell\explore\command",
    r"Software\Classes\Drive\shell\open\command",
    r"Software\Classes\Drive\shell\explore\command",
];

/// Per-user redirect so `explorer.exe` (taskbar/desktop shortcut, Chrome
/// "Show in folder" via `/select`, etc.) launches Pathfinder with the same args.
/// HKCU only — no admin rights. Removed by [`restore_windows_default_folder_handler`].
const EXPLORER_APP_PATH_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\App Paths\explorer.exe";

fn folder_open_command(exe: &str) -> String {
    format!("\"{exe}\" --path \"%1\"")
}

fn set_explorer_app_path_redirect(exe: &str) -> Result<(), String> {
    let install_dir = std::path::Path::new(exe)
        .parent()
        .ok_or_else(|| "could not resolve Pathfinder install directory".to_string())?;
    let install_dir = install_dir.to_string_lossy().into_owned();

    let key = hkcu_open_create_leaf(EXPLORER_APP_PATH_KEY)?;
    let r = set_key_default_string(key, exe);
    let r2 = set_key_string(key, Some("Path"), &install_dir);
    unsafe {
        let _ = RegCloseKey(key);
    }
    r?;
    r2
}

fn clear_explorer_app_path_redirect() -> Result<(), String> {
    let wide = to_wide_nul(EXPLORER_APP_PATH_KEY);
    let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr())) };
    if err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND {
        return Err(format!(
            "RegDeleteTreeW({EXPLORER_APP_PATH_KEY}) failed: {:?}",
            err
        ));
    }
    Ok(())
}

fn explorer_redirect_points_at_current_exe() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().to_ascii_lowercase(),
        Err(_) => return false,
    };
    let wide_path = to_wide_nul(EXPLORER_APP_PATH_KEY);
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_path.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if err != ERROR_SUCCESS {
        return false;
    }
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let q = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR::null(),
            None,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if q != ERROR_SUCCESS {
        return false;
    }
    let chars = (size as usize / 2).saturating_sub(1);
    let value = String::from_utf16_lossy(&buf[..chars]).to_ascii_lowercase();
    value.contains(&exe)
}

/// Writes every folder/directory/drive verb so double-clicking a folder,
/// drive, or shortcut routes through Pathfinder.
pub fn set_pathfinder_as_default_folder_handler() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe = exe.to_string_lossy().into_owned();
    let cmd = folder_open_command(&exe);

    for rel in FOLDER_HANDLER_PATHS {
        let key = hkcu_open_create_leaf(rel)?;
        let r = set_key_default_string(key, &cmd);
        unsafe {
            let _ = RegCloseKey(key);
        }
        r?;
    }
    set_explorer_app_path_redirect(&exe)?;
    Ok(())
}

/// True if the HKCU folder/directory open command points at the current pathfinder.exe.
/// Used by the first-run welcome dialog so we can mark step 1 as already done.
/// Only checks the Folder/open command (not the Explorer App Path redirect) so
/// plans set up before v0.8 still register as "already done."
pub fn pathfinder_is_default_folder_handler() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().to_ascii_lowercase(),
        Err(_) => return false,
    };
    let rel = r"Software\Classes\Folder\shell\open\command";
    let wide_path = to_wide_nul(rel);
    let mut hkey = HKEY::default();
    let err = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_path.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if err != ERROR_SUCCESS {
        return false;
    }
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let q = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR::null(),
            None,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if q != ERROR_SUCCESS {
        return false;
    }
    let chars = (size as usize / 2).saturating_sub(1);
    let value = String::from_utf16_lossy(&buf[..chars]).to_ascii_lowercase();
    value.contains(&exe)
}

/// Removes HKCU overrides created by [`set_pathfinder_as_default_folder_handler`].
/// Mirrors FOLDER_HANDLER_PATHS so a Restore goes back to Explorer defaults.
pub fn restore_windows_default_folder_handler() -> Result<(), String> {
    for rel in FOLDER_HANDLER_PATHS {
        let wide = to_wide_nul(rel);
        let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr())) };
        if err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND {
            return Err(format!("RegDeleteTreeW({rel}) failed: {:?}", err));
        }
    }
    clear_explorer_app_path_redirect()
}

/// Verifies all shell handler registry entries are properly configured.
/// Returns (properly_configured_count, total_count).
/// Useful for diagnostics and validation.
pub fn verify_shell_handler_entries() -> Result<(usize, usize), String> {
    // 6 folder/directory/drive keys + 1 explorer App Path redirect = 7 entries.
    let total = FOLDER_HANDLER_PATHS.len() + 1;
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().to_ascii_lowercase(),
        Err(_) => return Ok((0, total)),
    };

    let mut valid_count = 0;
    for rel in FOLDER_HANDLER_PATHS {
        let wide_path = to_wide_nul(rel);
        let mut hkey = HKEY::default();
        let err = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_path.as_ptr()),
                None,
                KEY_READ,
                &mut hkey,
            )
        };
        if err != ERROR_SUCCESS {
            continue;
        }

        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        let q = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR::null(),
                None,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        unsafe {
            let _ = RegCloseKey(hkey);
        }

        if q == ERROR_SUCCESS {
            let chars = (size as usize / 2).saturating_sub(1);
            let value = String::from_utf16_lossy(&buf[..chars]).to_ascii_lowercase();
            if value.contains(&exe) {
                valid_count += 1;
            }
        }
    }

    if explorer_redirect_points_at_current_exe() {
        valid_count += 1;
    }
    Ok((valid_count, total))
}

/// Generates a complete .reg file content with the current Pathfinder executable path.
/// This file can be double-clicked to apply the settings (safer than manual registry editing).
///
/// # Returns
/// A formatted Windows Registry Editor V5.00 format string with all folder handler paths configured.
pub fn generate_registry_file_content() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe = exe.to_string_lossy().into_owned();

    // Escape backslashes for .reg file format (needs doubled backslashes)
    let exe_escaped = exe.replace('\\', "\\\\");

    // Build the command: quoted executable + space + arguments
    let cmd = format!("\\\"{exe_escaped}\\\" --path \\\"%1\\\"");
    let exe_quoted = format!("\\\"{exe_escaped}\\\"");
    let install_dir_escaped = std::path::Path::new(&exe)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "\\\\"))
        .unwrap_or_default();

    let content = format!(
        "Windows Registry Editor Version 5.00\n\
         ; Pathfinder — per-user default folder handler\n\
         ; Generated automatically with the current Pathfinder path.\n\
         ; Safe to import: only affects HKCU (per-user), not system registry.\n\
         ;\n\
         ; To apply: double-click this file, or use:\n\
         ;   reg import pathfinder-folder-handler.reg\n\
         ;\n\
         ; Pathfinder is set as the default for:\n\
         ; - Double-clicking folders on desktop\n\
         ; - Double-clicking drives in \"This PC\"\n\
         ; - \"Open\" and \"Explore\" context menu verbs\n\
         ; - File Explorer / Chrome \"Show in folder\" (explorer.exe redirect)\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\Folder\\shell\\open\\command]\n\
         @=\"{}\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\Folder\\shell\\explore\\command]\n\
         @=\"{}\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\open\\command]\n\
         @=\"{}\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\Directory\\shell\\explore\\command]\n\
         @=\"{}\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\Drive\\shell\\open\\command]\n\
         @=\"{}\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\Drive\\shell\\explore\\command]\n\
         @=\"{}\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\App Paths\\explorer.exe]\n\
         @=\"{}\"\n\
         \"Path\"=\"{}\"\n",
        cmd, cmd, cmd, cmd, cmd, cmd, exe_quoted, install_dir_escaped
    );

    Ok(content)
}
