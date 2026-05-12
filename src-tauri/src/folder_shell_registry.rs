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
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let nbytes = (wide.len() * std::mem::size_of::<u16>()) as u32;
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), nbytes as usize) };
    let err = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(bytes)) };
    if err != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW failed: {:?}", err));
    }
    Ok(())
}

/// Writes `Folder` and `Directory` open verbs so double-clicking a folder uses Pathfinder.
pub fn set_pathfinder_as_default_folder_handler() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe = exe.to_string_lossy().into_owned();
    let cmd = format!("\"{exe}\" --path \"%1\"");

    const PATHS: [&str; 2] = [
        r"Software\Classes\Folder\shell\open\command",
        r"Software\Classes\Directory\shell\open\command",
    ];

    for rel in PATHS {
        let key = hkcu_open_create_leaf(rel)?;
        let r = set_key_default_string(key, &cmd);
        unsafe {
            let _ = RegCloseKey(key);
        }
        r?;
    }
    Ok(())
}

/// True if the HKCU folder/directory open command points at the current pathfinder.exe.
/// Used by the first-run welcome dialog so we can mark step 1 as already done.
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
pub fn restore_windows_default_folder_handler() -> Result<(), String> {
    const PATHS: [&str; 2] = [
        r"Software\Classes\Folder\shell\open\command",
        r"Software\Classes\Directory\shell\open\command",
    ];
    for rel in PATHS {
        let wide = to_wide_nul(rel);
        let err = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr())) };
        if err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND {
            return Err(format!("RegDeleteTreeW({rel}) failed: {:?}", err));
        }
    }
    Ok(())
}
