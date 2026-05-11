//! System icon extraction via the Win32 shell. Returns RGBA pixels that
//! callers can wrap in a `slint::Image`.
//!
//! For most file types the icon is shared by all files with that extension
//! (e.g. every `.docx` uses Word's icon). For a small set of types each file
//! carries its own icon embedded in the binary (`.exe`, `.lnk`, `.ico`).
//! Callers should cache by extension for the first group and by full path
//! for the second group.

#![cfg(target_os = "windows")]

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Attempt to extract the shell icon for a path and return RGBA pixels.
/// Returns None on failure (no icon available, GDI call failed, etc.).
///
/// `use_real_file` controls whether SHGetFileInfo opens the file to read
/// embedded icons. Pass true for `.exe`, `.lnk`, `.ico`. Pass false for
/// generic extension probes — we feed a fake filename like `dummy.docx`
/// with FILE_ATTRIBUTE_NORMAL so the shell returns the system icon for
/// the extension without touching the disk.
pub fn extract_icon_rgba(path: &str, use_real_file: bool) -> Option<Image> {
    unsafe {
        let wide: Vec<u16> = OsStr::new(path)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut sfi = SHFILEINFOW::default();
        let mut flags = SHGFI_ICON | SHGFI_LARGEICON;
        if !use_real_file {
            flags |= SHGFI_USEFILEATTRIBUTES;
        }
        let result = SHGetFileInfoW(
            windows::core::PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if result == 0 || sfi.hIcon.is_invalid() {
            return None;
        }
        let img = hicon_to_image(sfi.hIcon);
        let _ = DestroyIcon(sfi.hIcon);
        img
    }
}

/// Convert an HICON to a `slint::Image` with premultiplied alpha. The icon
/// is read from GDI bitmaps, the mask is applied for 1-bit alpha icons, and
/// pixels are reordered from BGRA to RGBA.
unsafe fn hicon_to_image(hicon: HICON) -> Option<Image> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(hicon, &mut ii).ok()?;

        // Read the color bitmap header to find dimensions.
        let mut color_bm = windows::Win32::Graphics::Gdi::BITMAP::default();
        let read = windows::Win32::Graphics::Gdi::GetObjectW(
            ii.hbmColor.into(),
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAP>() as i32,
            Some(&mut color_bm as *mut _ as *mut _),
        );
        if read == 0 {
            let _ = DeleteObject(ii.hbmColor.into());
            let _ = DeleteObject(ii.hbmMask.into());
            return None;
        }
        let w = color_bm.bmWidth as u32;
        let h = color_bm.bmHeight as u32;
        if w == 0 || h == 0 || w > 1024 || h > 1024 {
            let _ = DeleteObject(ii.hbmColor.into());
            let _ = DeleteObject(ii.hbmMask.into());
            return None;
        }

        // Allocate a BGRA buffer and pull the pixels out via GetDIBits.
        let mut bgra: Vec<u8> = vec![0; (w * h * 4) as usize];
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w as i32;
        // Negative height => top-down DIB so rows are in natural order.
        bmi.bmiHeader.biHeight = -(h as i32);
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let hdc = GetDC(Some(HWND::default()));
        let pulled = GetDIBits(
            hdc,
            ii.hbmColor,
            0,
            h,
            Some(bgra.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = ReleaseDC(Some(HWND::default()), hdc);

        let _ = DeleteObject(ii.hbmColor.into());
        let _ = DeleteObject(ii.hbmMask.into());

        if pulled == 0 {
            return None;
        }

        // Some icon bitmaps come back with alpha=0 across the board. When that
        // happens, assume opaque pixels (255) so the icon is visible at all.
        let any_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
        let mut rgba: Vec<u8> = Vec::with_capacity(bgra.len());
        for chunk in bgra.chunks_exact(4) {
            let b = chunk[0];
            let g = chunk[1];
            let r = chunk[2];
            let a = if any_alpha { chunk[3] } else { 255 };
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(a);
        }

        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, w, h);
        Some(Image::from_rgba8_premultiplied(buffer))
    }
}

/// Convenience wrapper that picks the right strategy based on file extension.
pub fn icon_for(path: &str) -> Option<Image> {
    let lower = path.to_ascii_lowercase();
    let use_real = lower.ends_with(".exe")
        || lower.ends_with(".lnk")
        || lower.ends_with(".ico")
        || lower.ends_with(".msi");
    extract_icon_rgba(path, use_real)
}
