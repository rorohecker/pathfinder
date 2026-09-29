//! Per-user (HKCU) overrides so Explorer opens folders with Pathfinder via `--path \"%1\"`.
//! Does not touch `HKCU\\...\\file\\shell\\open` - that would hijack all file opens.
//!
//! Safety contract:
//! - Only writes `HKEY_CURRENT_USER` (no admin, no HKLM, never replaces
//!   `C:\\Windows\\explorer.exe`).
//! - App Paths redirects bare `explorer.exe` name lookups to Pathfinder; full-path
//!   shell restarts (`C:\\Windows\\explorer.exe`) still hit the real binary.
//! - Unhandled Explorer verbs (`shell:`, CLSIDs, unknown switches) are forwarded
//!   to the system explorer via [`spawn_system_explorer`] so the desktop never breaks.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Opens or creates `HKCU\\<relative>` one segment at a time. Returns a handle to the leaf key
/// (caller must `RegCloseKey` - never close `HKEY_CURRENT_USER`).
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
            return Err(format!("RegCreateKeyExW failed for {:?}: {:?}", seg, err));
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RegistryValueBackup {
    kind: u32,
    bytes: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
struct OwnedValueBackup {
    path: String,
    name: Option<String>,
    owned: String,
    previous: Option<RegistryValueBackup>,
}

#[derive(Default, Serialize, Deserialize)]
struct ShellRegistryBackup {
    values: Vec<OwnedValueBackup>,
}

const SHELL_BACKUP_FILE: &str = "shell_registry_backup.json";

fn read_registry_value(
    path: &str,
    name: Option<&str>,
) -> Result<Option<RegistryValueBackup>, String> {
    let wide_path = to_wide_nul(path);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_path.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if opened != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW({path}) failed: {opened:?}"));
    }
    let name_wide = name.map(to_wide_nul);
    let name_ptr = name_wide
        .as_ref()
        .map(|wide| PCWSTR(wide.as_ptr()))
        .unwrap_or(PCWSTR::null());
    let result = (|| {
        let mut kind = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        let queried = unsafe {
            RegQueryValueExW(key, name_ptr, None, Some(&mut kind), None, Some(&mut size))
        };
        if queried == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if queried != ERROR_SUCCESS {
            return Err(format!("RegQueryValueExW({path}) failed: {queried:?}"));
        }
        let mut bytes = vec![0u8; size as usize];
        let data = if bytes.is_empty() {
            None
        } else {
            Some(bytes.as_mut_ptr())
        };
        let read = unsafe {
            RegQueryValueExW(key, name_ptr, None, Some(&mut kind), data, Some(&mut size))
        };
        if read != ERROR_SUCCESS {
            return Err(format!("RegQueryValueExW({path}) read failed: {read:?}"));
        }
        bytes.truncate(size as usize);
        Ok(Some(RegistryValueBackup {
            kind: kind.0,
            bytes,
        }))
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn string_value(value: &RegistryValueBackup) -> Option<String> {
    if value.kind != REG_SZ.0 || !value.bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = value
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .collect();
    Some(
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_string(),
    )
}

/// `None` means someone else owns the current value; `Some(None)` means
/// delete only this value; `Some(Some(..))` restores the saved raw value.
fn restore_decision(
    current: Option<&RegistryValueBackup>,
    expected: &str,
    previous: Option<&RegistryValueBackup>,
) -> Option<Option<RegistryValueBackup>> {
    (current.and_then(string_value).as_deref() == Some(expected)).then(|| previous.cloned())
}

fn write_registry_value(
    path: &str,
    name: Option<&str>,
    value: &RegistryValueBackup,
) -> Result<(), String> {
    let key = hkcu_open_create_leaf(path)?;
    let name_wide = name.map(to_wide_nul);
    let name_ptr = name_wide
        .as_ref()
        .map(|wide| PCWSTR(wide.as_ptr()))
        .unwrap_or(PCWSTR::null());
    let err = unsafe {
        RegSetValueExW(
            key,
            name_ptr,
            None,
            REG_VALUE_TYPE(value.kind),
            Some(&value.bytes),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if err != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW({path}) failed: {err:?}"));
    }
    Ok(())
}

fn delete_registry_value(path: &str, name: Option<&str>) -> Result<(), String> {
    let wide_path = to_wide_nul(path);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_path.as_ptr()),
            None,
            KEY_WRITE,
            &mut key,
        )
    };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if opened != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW({path}) failed: {opened:?}"));
    }
    let name_wide = name.map(to_wide_nul);
    let name_ptr = name_wide
        .as_ref()
        .map(|wide| PCWSTR(wide.as_ptr()))
        .unwrap_or(PCWSTR::null());
    let err = unsafe { RegDeleteValueW(key, name_ptr) };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND {
        return Err(format!("RegDeleteValueW({path}) failed: {err:?}"));
    }
    Ok(())
}

fn desired_values(exe: &str) -> Result<Vec<(String, Option<String>, String)>, String> {
    let mut values: Vec<_> = FOLDER_HANDLER_PATHS
        .iter()
        .map(|path| (path.to_string(), None, folder_open_command(exe)))
        .collect();
    let install_dir = std::path::Path::new(exe)
        .parent()
        .ok_or("Could not resolve Pathfinder install directory")?
        .to_string_lossy()
        .into_owned();
    values.push((EXPLORER_APP_PATH_KEY.to_string(), None, exe.to_string()));
    values.push((
        EXPLORER_APP_PATH_KEY.to_string(),
        Some("Path".to_string()),
        install_dir,
    ));
    Ok(values)
}

/// Registry paths that drive folder navigation in Windows. Setting all of
/// them at HKCU level routes every Windows-native "open this folder" code
/// path (folder shortcuts, Chrome's "Show in folder", "Open file location"
/// in Start menu, double-clicked drives in This PC, anything that calls
/// ShellExecute on a directory) through Pathfinder.
///
///   - `Folder\shell\open\command`     - generic folder class, picked up by
///     ShellExecute("open", "C:\..."). The Folder class is what most apps
///     trigger when they want to reveal a directory.
///   - `Folder\shell\explore\command`  - same class, "explore" verb. Some
///     Win32 apps explicitly invoke this verb instead of "open".
///   - `Directory\shell\open\command`  - file-system directory class. Many
///     apps target this directly because the "Folder" alias can resolve to
///     virtual shell folders (Control Panel, etc) that we don't want to host.
///   - `Directory\shell\explore\command` - same as above for "explore".
///   - `Drive\shell\open\command`      - what double-clicking a drive in
///     This PC triggers. Without this entry, drives still open in Explorer
///     even when every folder above opens in Pathfinder.
///   - `Drive\shell\explore\command`   - same for "explore" verb on drives.
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
/// HKCU only - no admin rights. Removed by [`restore_windows_default_folder_handler`].
///
/// This does **not** replace the system binary. Processes that launch
/// `C:\Windows\explorer.exe` by full path (including desktop shell restart)
/// bypass App Paths and keep working.
const EXPLORER_APP_PATH_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\App Paths\explorer.exe";

/// Absolute path to the real Windows Explorer binary. Used when Pathfinder is
/// invoked via App Paths with verbs it cannot host (CLSID, shell:, etc.).
pub fn system_explorer_exe() -> std::path::PathBuf {
    std::env::var_os("WINDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("explorer.exe")
}

/// Launch the real system Explorer with the given args and return. Never used
/// for Pathfinder's own UI — only as a safe fallback for unhandled shell verbs.
pub fn spawn_system_explorer(args: &[String]) -> Result<(), String> {
    let exe = system_explorer_exe();
    if !exe.is_file() {
        return Err(format!("system explorer not found at {}", exe.display()));
    }
    std::process::Command::new(&exe)
        .args(args)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn {}: {e}", exe.display()))
}

fn folder_open_command(exe: &str) -> String {
    format!("\"{exe}\" --path \"%1\"")
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
    let desired = desired_values(&exe)?;
    let old_backup: ShellRegistryBackup =
        crate::read_native_json(SHELL_BACKUP_FILE, ShellRegistryBackup::default());
    let mut backup = ShellRegistryBackup::default();
    for (path, name, owned) in &desired {
        let current = read_registry_value(path, name.as_deref())?;
        let prior = old_backup.values.iter().find(|item| {
            item.path == *path
                && item.name == *name
                && current.as_ref().and_then(string_value).as_deref() == Some(item.owned.as_str())
        });
        let previous = if let Some(prior) = prior {
            prior.previous.clone()
        } else if current.as_ref().and_then(string_value).as_deref() == Some(owned.as_str()) {
            // Older Pathfinder builds did not save the overwritten value.
            // Never record our own registration as the value to restore.
            None
        } else {
            current
        };
        backup.values.push(OwnedValueBackup {
            path: path.clone(),
            name: name.clone(),
            owned: owned.clone(),
            previous,
        });
    }
    // Save all old values before changing any registration. A partial failure
    // can then be restored without deleting another application's subkeys.
    crate::write_native_json(SHELL_BACKUP_FILE, &backup)?;
    for (path, name, value) in desired {
        let key = hkcu_open_create_leaf(&path)?;
        let result = set_key_string(key, name.as_deref(), &value);
        unsafe {
            let _ = RegCloseKey(key);
        }
        result?;
    }
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
    let exe = std::env::current_exe()
        .map_err(|e| format!("current_exe: {e}"))?
        .to_string_lossy()
        .into_owned();
    let desired = desired_values(&exe)?;
    let backup: ShellRegistryBackup =
        crate::read_native_json(SHELL_BACKUP_FILE, ShellRegistryBackup::default());
    let app_path_owned = {
        let saved = backup
            .values
            .iter()
            .find(|item| item.path == EXPLORER_APP_PATH_KEY && item.name.is_none());
        let expected = saved
            .map(|item| item.owned.as_str())
            .unwrap_or(exe.as_str());
        read_registry_value(EXPLORER_APP_PATH_KEY, None)?
            .as_ref()
            .and_then(string_value)
            .as_deref()
            == Some(expected)
    };
    for (path, name, owned) in desired {
        if path == EXPLORER_APP_PATH_KEY && name.as_deref() == Some("Path") && !app_path_owned {
            continue;
        }
        let saved = backup
            .values
            .iter()
            .find(|item| item.path == path && item.name == name);
        let expected = saved
            .map(|item| item.owned.as_str())
            .unwrap_or(owned.as_str());
        let current = read_registry_value(&path, name.as_deref())?;
        match restore_decision(
            current.as_ref(),
            expected,
            saved.and_then(|item| item.previous.as_ref()),
        ) {
            None => continue,
            Some(Some(previous)) => write_registry_value(&path, name.as_deref(), &previous)?,
            Some(None) => delete_registry_value(&path, name.as_deref())?,
        }
    }
    let path = crate::native_data_file(SHELL_BACKUP_FILE);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_file_name(format!("{SHELL_BACKUP_FILE}.bak")));
    Ok(())
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
         ; Pathfinder - per-user default folder handler\n\
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

#[cfg(test)]
mod ownership_tests {
    use super::*;

    fn string_raw(text: &str) -> RegistryValueBackup {
        RegistryValueBackup {
            kind: REG_SZ.0,
            bytes: text
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }

    #[test]
    fn restore_never_overwrites_a_new_owner() {
        let ours = string_raw("Pathfinder command");
        let new_owner = string_raw("Another manager command");
        let previous = string_raw("Original command");
        assert_eq!(
            restore_decision(Some(&new_owner), "Pathfinder command", Some(&previous)),
            None
        );
        assert_eq!(
            restore_decision(Some(&ours), "Pathfinder command", Some(&previous)),
            Some(Some(previous))
        );
        assert_eq!(
            restore_decision(Some(&ours), "Pathfinder command", None),
            Some(None)
        );
    }
}
