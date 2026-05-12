//! Windows shell HDROP drag-and-drop: IDataObject + IDropSource (outgoing)
//! and IDropTarget (incoming — registered on the app HWND).
//!
//! Diagnostic events (registration result, every DragEnter/Drop) are written
//! to `%TEMP%\pathfinder_dragdrop.log` since stderr is not always captured.
//! Inspect that file when troubleshooting.
use std::ffi::OsStr;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, HGLOBAL, HWND, POINTL, S_OK, WPARAM};
use windows::Win32::System::Com::{
    FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
    STGMEDIUM, STGMEDIUM_0,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{
    DoDragDrop, OleInitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE,
    IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, MSGFLT_ALLOW, WM_DROPFILES,
};
use windows::core::{implement, Ref, BOOL, HRESULT};

const DRAGDROP_S_DROP: HRESULT = HRESULT(0x00040100_u32 as i32);
const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x00040101_u32 as i32);
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x00040102_u32 as i32);

const CF_HDROP: u16 = 15;
const MK_CONTROL: u32 = 0x0008; // Ctrl key in MODIFIERKEYS_FLAGS

// Undocumented but widely used: WM_COPYGLOBALDATA must be allowed through UIPI
// for OLE drag-drop to work across processes at different integrity levels
// (e.g. dropping from a non-elevated Explorer into an elevated app).
const WM_COPYGLOBALDATA: u32 = 0x0049;
const WM_COPYDATA: u32 = 0x004A;

// ── Diagnostic logging ────────────────────────────────────────────────────────

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("pathfinder_dragdrop.log")
}

fn log(msg: &str) {
    let _ = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        writeln!(f, "[{}] {}", now, msg)?;
        Ok(())
    })();
    // Also emit to stderr so people running from a console see it live.
    eprintln!("[file_drag] {}", msg);
}

// Matches Win32 DROPFILES struct layout.
#[repr(C)]
struct DropFiles {
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    f_nc: i32,
    f_wide: i32,
}

// ── HGLOBAL helpers ───────────────────────────────────────────────────────────

unsafe fn build_hdrop(paths: &[String]) -> windows::core::Result<HGLOBAL> {
    unsafe {
        let wide_paths: Vec<Vec<u16>> = paths
            .iter()
            .map(|p| OsStr::new(p).encode_wide().chain(Some(0u16)).collect())
            .collect();

        let total_wchars: usize = wide_paths.iter().map(|p| p.len()).sum::<usize>() + 1;
        let header_size = std::mem::size_of::<DropFiles>();
        let total_bytes = header_size + total_wchars * 2;

        let hglobal = GlobalAlloc(GMEM_MOVEABLE, total_bytes)?;
        let base = GlobalLock(hglobal) as *mut u8;
        if base.is_null() {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }

        let hdr = base as *mut DropFiles;
        (*hdr).p_files = header_size as u32;
        (*hdr).pt_x = 0;
        (*hdr).pt_y = 0;
        (*hdr).f_nc = 0;
        (*hdr).f_wide = 1;

        let mut dst = base.add(header_size) as *mut u16;
        for wide in &wide_paths {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            dst = dst.add(wide.len());
        }
        *dst = 0u16;

        let _ = GlobalUnlock(hglobal);
        Ok(hglobal)
    }
}

unsafe fn clone_hglobal(src: HGLOBAL) -> windows::core::Result<HGLOBAL> {
    unsafe {
        let size = GlobalSize(src);
        let src_ptr = GlobalLock(src);
        if src_ptr.is_null() {
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        let dst_hg = GlobalAlloc(GMEM_MOVEABLE, size)?;
        let dst_ptr = GlobalLock(dst_hg);
        if dst_ptr.is_null() {
            let _ = GlobalUnlock(src);
            return Err(windows::core::Error::from_hresult(E_FAIL));
        }
        if size != 0 {
            std::ptr::copy_nonoverlapping(src_ptr as *const u8, dst_ptr as *mut u8, size);
        }
        let _ = GlobalUnlock(src);
        let _ = GlobalUnlock(dst_hg);
        Ok(dst_hg)
    }
}

/// Extract file paths from the DROPFILES memory block in an HGLOBAL.
///
/// All reads are bounded by [`GlobalSize`]. Malformed or hostile drag payloads
/// must not read past the allocation (would crash or worse).
unsafe fn paths_from_hglobal(hglobal: HGLOBAL) -> Vec<String> {
    unsafe {
        let total_bytes = GlobalSize(hglobal) as usize;
        let header_size = std::mem::size_of::<DropFiles>();
        if total_bytes < header_size {
            return vec![];
        }
        let base = GlobalLock(hglobal) as *const u8;
        if base.is_null() {
            return vec![];
        }
        let hdr = std::ptr::read(base as *const DropFiles);
        let offset = hdr.p_files as usize;
        if offset > total_bytes || offset.saturating_add(2) > total_bytes {
            let _ = GlobalUnlock(hglobal);
            return vec![];
        }
        let is_wide = hdr.f_wide != 0;
        let mut paths = Vec::new();
        let end_usize = (base as usize).saturating_add(total_bytes);

        #[inline]
        fn can_read_u8(p: *const u8, end_usize: usize) -> bool {
            (p as usize) < end_usize
        }

        #[inline]
        fn can_read_u16(p: *const u16, end_usize: usize) -> bool {
            (p as usize)
                .checked_add(std::mem::size_of::<u16>())
                .map_or(false, |hi| hi <= end_usize)
        }

        if is_wide {
            let mut ptr = base.add(offset) as *const u16;
            loop {
                if !can_read_u16(ptr, end_usize) {
                    break;
                }
                if *ptr == 0 {
                    break;
                }
                let str_start = ptr;
                let mut len = 0usize;
                loop {
                    let q = ptr.add(len);
                    if !can_read_u16(q, end_usize) {
                        let _ = GlobalUnlock(hglobal);
                        return paths;
                    }
                    if *q == 0 {
                        break;
                    }
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(str_start, len);
                if let Ok(s) = String::from_utf16(slice) {
                    paths.push(s);
                }
                ptr = ptr.add(len + 1);
            }
        } else {
            // ANSI fallback (rarely used in modern apps but defensive).
            let mut ptr = base.add(offset);
            loop {
                if !can_read_u8(ptr, end_usize) {
                    break;
                }
                if *ptr == 0 {
                    break;
                }
                let str_start = ptr;
                let mut len = 0usize;
                loop {
                    let q = ptr.add(len);
                    if !can_read_u8(q, end_usize) {
                        let _ = GlobalUnlock(hglobal);
                        return paths;
                    }
                    if *q == 0 {
                        break;
                    }
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(str_start, len);
                if let Ok(s) = std::str::from_utf8(slice) {
                    paths.push(s.to_string());
                }
                ptr = ptr.add(len + 1);
            }
        }
        let _ = GlobalUnlock(hglobal);
        paths
    }
}

/// Extract CF_HDROP paths from an IDataObject.
unsafe fn paths_from_data_object(data: &IDataObject) -> Vec<String> {
    unsafe {
        let fmt = FORMATETC {
            cfFormat: CF_HDROP,
            ptd: std::ptr::null_mut(),
            dwAspect: 1, // DVASPECT_CONTENT
            lindex: -1,
            tymed: 1, // TYMED_HGLOBAL
        };
        let mut medium = match data.GetData(&fmt) {
            Ok(m) => m,
            Err(e) => {
                log(&format!("paths_from_data_object: GetData failed: {:?}", e));
                return vec![];
            }
        };
        let hglobal = medium.u.hGlobal;
        let paths = paths_from_hglobal(hglobal);
        ReleaseStgMedium(&mut medium as *mut _);
        paths
    }
}

// ── IDataObject (outgoing drag source) ───────────────────────────────────────

#[implement(IDataObject)]
struct HDropData {
    hglobal_raw: usize,
}

#[allow(non_snake_case)]
impl IDataObject_Impl for HDropData_Impl {
    #[allow(clippy::field_reassign_with_default)]
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        unsafe {
            if (*pformatetcin).cfFormat != CF_HDROP {
                return Err(windows::core::Error::new(E_FAIL, "format not supported"));
            }
            let src = HGLOBAL(self.hglobal_raw as *mut _);
            let new_hg = clone_hglobal(src)?;
            let med = STGMEDIUM {
                tymed: 1u32,
                u: STGMEDIUM_0 { hGlobal: new_hg },
                ..Default::default()
            };
            Ok(med)
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(windows::core::Error::new(E_NOTIMPL, ""))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        unsafe {
            if (*pformatetc).cfFormat == CF_HDROP { S_OK } else { E_FAIL }
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: BOOL,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(E_NOTIMPL, ""))
    }

    fn EnumFormatEtc(&self, _: u32) -> windows::core::Result<IEnumFORMATETC> {
        Err(windows::core::Error::new(E_NOTIMPL, ""))
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(windows::core::Error::new(E_NOTIMPL, ""))
    }

    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(windows::core::Error::new(E_NOTIMPL, ""))
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(windows::core::Error::new(E_NOTIMPL, ""))
    }
}

// ── IDropSource ───────────────────────────────────────────────────────────────

#[implement(IDropSource)]
struct DropSrc;

#[allow(non_snake_case)]
impl IDropSource_Impl for DropSrc_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if (grfkeystate.0 & 0x0001) == 0 {
            // MK_LBUTTON released
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// ── IDropTarget (incoming drops onto Pathfinder) ──────────────────────────────

thread_local! {
    // (paths, is_move, screen_x, screen_y) — coordinates are screen pixels
    // (HIDPI raw units), so the receiver must convert to client/logical.
    static DROP_HANDLER: std::cell::RefCell<Option<Box<dyn Fn(Vec<String>, bool, i32, i32)>>> =
        std::cell::RefCell::new(None);

    // Called on DragEnter / DragOver / DragLeave with screen coordinates so
    // the UI can highlight the destination pane during the drag. The bool
    // is `is_active` — false on DragLeave to clear the highlight.
    static DRAG_OVER_HANDLER: std::cell::RefCell<Option<Box<dyn Fn(bool, i32, i32)>>> =
        std::cell::RefCell::new(None);
}

#[implement(IDropTarget)]
struct PathfinderDropTarget;

unsafe impl Send for PathfinderDropTarget {}
unsafe impl Sync for PathfinderDropTarget {}

#[allow(non_snake_case)]
impl IDropTarget_Impl for PathfinderDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        log("DragEnter");
        let (sx, sy) = (pt.x, pt.y);
        DRAG_OVER_HANDLER.with(|h| {
            if let Some(cb) = &*h.borrow() {
                cb(true, sx, sy);
            }
        });
        let accepts = if let Some(ref data) = *pdataobj {
            let fmt = FORMATETC {
                cfFormat: CF_HDROP,
                ptd: std::ptr::null_mut(),
                dwAspect: 1,
                lindex: -1,
                tymed: 1,
            };
            unsafe { data.QueryGetData(&fmt) == S_OK }
        } else {
            false
        };
        unsafe {
            *pdweffect = if accepts {
                if (grfkeystate.0 & MK_CONTROL) != 0 { DROPEFFECT_COPY } else { DROPEFFECT_MOVE }
            } else {
                DROPEFFECT_NONE
            };
        }
        Ok(())
    }

    fn DragOver(
        &self,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let (sx, sy) = (pt.x, pt.y);
        DRAG_OVER_HANDLER.with(|h| {
            if let Some(cb) = &*h.borrow() {
                cb(true, sx, sy);
            }
        });
        unsafe {
            let cur = *pdweffect;
            *pdweffect = if cur == DROPEFFECT_NONE {
                DROPEFFECT_NONE
            } else if (grfkeystate.0 & MK_CONTROL) != 0 {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_MOVE
            };
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        log("DragLeave");
        DRAG_OVER_HANDLER.with(|h| {
            if let Some(cb) = &*h.borrow() {
                cb(false, 0, 0);
            }
        });
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        log(&format!("Drop at screen ({}, {})", pt.x, pt.y));
        let Some(ref data) = *pdataobj else {
            log("Drop: pdataobj is None");
            unsafe { *pdweffect = DROPEFFECT_NONE; }
            return Ok(());
        };
        let is_move = (grfkeystate.0 & MK_CONTROL) == 0;
        let paths = unsafe { paths_from_data_object(data) };
        log(&format!("Drop: {} path(s), is_move={}", paths.len(), is_move));
        // Always clear the drag-over highlight when the drop completes (success or not).
        DRAG_OVER_HANDLER.with(|h| {
            if let Some(cb) = &*h.borrow() {
                cb(false, 0, 0);
            }
        });
        if paths.is_empty() {
            unsafe { *pdweffect = DROPEFFECT_NONE; }
            return Ok(());
        }
        unsafe { *pdweffect = if is_move { DROPEFFECT_MOVE } else { DROPEFFECT_COPY }; }
        let (sx, sy) = (pt.x, pt.y);
        DROP_HANDLER.with(|h| {
            if let Some(cb) = &*h.borrow() {
                cb(paths, is_move, sx, sy);
            } else {
                log("Drop: no DROP_HANDLER installed");
            }
        });
        Ok(())
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Allow OLE drag-drop messages through UIPI so drops from lower-integrity
/// processes (e.g. non-elevated Explorer into an elevated Pathfinder) reach us.
/// Safe to call even when not elevated — no-op in that case.
unsafe fn enable_uipi_dragdrop(hwnd: HWND) {
    unsafe {
        let _ = ChangeWindowMessageFilterEx(hwnd, WM_DROPFILES, MSGFLT_ALLOW, None);
        let _ = ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, None);
        let _ = ChangeWindowMessageFilterEx(hwnd, WM_COPYGLOBALDATA, MSGFLT_ALLOW, None);
    }
}

/// Register an IDropTarget on the given HWND. The returned IDropTarget must
/// be kept alive for the entire window lifetime (store it in a local in `run()`).
///
/// This:
///   1. Calls `OleInitialize` (REQUIRED for `RegisterDragDrop`; CoInitializeEx
///      alone does not set up the OLE drag-drop subsystem).
///   2. Bypasses UIPI so cross-IL drops aren't silently filtered.
///   3. Calls `RevokeDragDrop` first to clear winit's IDropTarget — winit
///      registers its own to surface DroppedFile events, and a window can
///      only have one IDropTarget. Without revoking, our RegisterDragDrop
///      returns DRAGDROP_E_ALREADYREGISTERED and our handler never fires.
pub fn register_drop_target(
    hwnd: HWND,
    handler: impl Fn(Vec<String>, bool, i32, i32) + 'static,
) -> Option<IDropTarget> {
    log(&format!("register_drop_target: HWND {:?}", hwnd.0));

    DROP_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
    unsafe {
        let ole = OleInitialize(None);
        log(&format!("OleInitialize -> {:?}", ole));

        enable_uipi_dragdrop(hwnd);
        log("UIPI filters allowed for WM_DROPFILES/WM_COPYDATA/WM_COPYGLOBALDATA");

        let revoke = RevokeDragDrop(hwnd);
        log(&format!("RevokeDragDrop (clearing winit) -> {:?}", revoke));

        let target: IDropTarget = PathfinderDropTarget.into();
        match RegisterDragDrop(hwnd, &target) {
            Ok(()) => {
                log("RegisterDragDrop OK");
                Some(target)
            }
            Err(e) => {
                log(&format!("RegisterDragDrop FAILED: {:?}", e));
                None
            }
        }
    }
}

/// Install a callback that fires on every DragEnter/DragOver/DragLeave so the
/// UI can highlight the destination pane during a drag-drop. The bool argument
/// is `is_active` — true while dragging over the window, false on DragLeave or
/// after Drop completes.
pub fn register_drag_over_handler(handler: impl Fn(bool, i32, i32) + 'static) {
    DRAG_OVER_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
}

/// Unregister drop target on app shutdown.
pub fn unregister_drop_target(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

/// Start a shell drag operation (blocks until drop or cancel).
/// Returns the DROPEFFECT that the target performed (NONE = cancelled/rejected).
pub fn start(paths: Vec<String>) -> DROPEFFECT {
    log(&format!("start: {} path(s)", paths.len()));
    unsafe {
        let _ = OleInitialize(None);

        let hglobal = match build_hdrop(&paths) {
            Ok(h) => h,
            Err(e) => {
                log(&format!("build_hdrop failed: {:?}", e));
                return DROPEFFECT_NONE;
            }
        };

        let data: IDataObject = HDropData {
            hglobal_raw: hglobal.0 as usize,
        }
        .into();
        let src: IDropSource = DropSrc.into();
        let mut effect = DROPEFFECT_NONE;

        let r = DoDragDrop(&data, &src, DROPEFFECT_COPY | DROPEFFECT_MOVE, &mut effect);
        log(&format!("DoDragDrop -> {:?}, effect=0x{:x}", r, effect.0));
        effect
    }
}

// silence unused warning for the WPARAM import (kept available for future
// message-filter helpers that need wparam values)
#[allow(dead_code)]
fn _wparam_unused(_: WPARAM) {}

#[cfg(all(test, target_os = "windows"))]
mod paths_from_hglobal_tests {
    use super::{paths_from_hglobal, DropFiles};
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    unsafe fn free_hglobal(h: windows::Win32::Foundation::HGLOBAL) {
        let _ = unsafe { GlobalFree(Some(h)) };
    }

    #[test]
    fn oversized_p_files_returns_empty_without_panic() {
        unsafe {
            let header_size = std::mem::size_of::<DropFiles>();
            let total = header_size + 4;
            let h = GlobalAlloc(GMEM_MOVEABLE, total).unwrap();
            let base = GlobalLock(h) as *mut u8;
            let hdr = base as *mut DropFiles;
            (*hdr).p_files = 0xffff_fff0;
            (*hdr).pt_x = 0;
            (*hdr).pt_y = 0;
            (*hdr).f_nc = 0;
            (*hdr).f_wide = 1;
            let _ = GlobalUnlock(h);
            let paths = paths_from_hglobal(h);
            assert!(paths.is_empty());
            free_hglobal(h);
        }
    }

    #[test]
    fn valid_unicode_hdrop_parses_paths() {
        unsafe {
            let header_size = std::mem::size_of::<DropFiles>();
            let path = "C:\\test\\file.txt";
            let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
            let tail_bytes = wide.len() * 2 + 2; // paths + list terminator
            let total = header_size + tail_bytes;
            let h = GlobalAlloc(GMEM_MOVEABLE, total).unwrap();
            let base = GlobalLock(h) as *mut u8;
            let hdr = base as *mut DropFiles;
            (*hdr).p_files = header_size as u32;
            (*hdr).pt_x = 0;
            (*hdr).pt_y = 0;
            (*hdr).f_nc = 0;
            (*hdr).f_wide = 1;
            let mut dst = base.add(header_size) as *mut u16;
            for wc in &wide {
                *dst = *wc;
                dst = dst.add(1);
            }
            *dst = 0u16;
            let _ = GlobalUnlock(h);
            let paths = paths_from_hglobal(h);
            assert_eq!(paths, vec![path.to_string()]);
            free_hglobal(h);
        }
    }

    #[test]
    fn unterminated_wide_string_returns_partial_paths() {
        unsafe {
            let header_size = std::mem::size_of::<DropFiles>();
            // One wchar 'A' (0x41) with no trailing null within allocation
            let total = header_size + 2;
            let h = GlobalAlloc(GMEM_MOVEABLE, total).unwrap();
            let base = GlobalLock(h) as *mut u8;
            let hdr = base as *mut DropFiles;
            (*hdr).p_files = header_size as u32;
            (*hdr).pt_x = 0;
            (*hdr).pt_y = 0;
            (*hdr).f_nc = 0;
            (*hdr).f_wide = 1;
            let w = base.add(header_size) as *mut u16;
            *w = 0x0041;
            let _ = GlobalUnlock(h);
            let paths = paths_from_hglobal(h);
            assert!(paths.is_empty());
            free_hglobal(h);
        }
    }
}
