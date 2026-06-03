//! Windows shell HDROP drag-and-drop: IDataObject + IDropSource (outgoing)
//! and IDropTarget (incoming - registered on the app HWND).
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

// -- Diagnostic logging --------------------------------------------------------

fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("pathfinder_dragdrop.log")
}

pub(crate) fn log(msg: &str) {
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

// -- HGLOBAL helpers -----------------------------------------------------------

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
                .is_some_and(|hi| hi <= end_usize)
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

// -- IDataObject (outgoing drag source) ---------------------------------------

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

// -- IDropSource ---------------------------------------------------------------

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

// -- IDropTarget (incoming drops onto Pathfinder) ------------------------------

thread_local! {
    // (paths, is_move, screen_x, screen_y) - coordinates are screen pixels
    // (HIDPI raw units), so the receiver must convert to client/logical.
    static DROP_HANDLER: std::cell::RefCell<Option<Box<dyn Fn(Vec<String>, bool, i32, i32)>>> =
        std::cell::RefCell::new(None);

    // Called on DragEnter / DragOver / DragLeave with screen coordinates so
    // the UI can highlight the destination pane during the drag. The bool
    // is `is_active` - false on DragLeave to clear the highlight.
    static DRAG_OVER_HANDLER: std::cell::RefCell<Option<Box<dyn Fn(bool, i32, i32)>>> =
        std::cell::RefCell::new(None);

    // Called once on DragEnter with the list of dragged paths so the UI can
    // seed the ghost-overlay label (e.g. "Photo.png + 4 more"). Cleared on
    // DragLeave with an empty Vec so the label disappears immediately.
    static DRAG_PATHS_HANDLER: std::cell::RefCell<Option<Box<dyn Fn(Vec<String>)>>> =
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
        // Surface the dragged file names so the slint ghost overlay can
        // display them while the drag is in flight. We do this once on
        // DragEnter rather than every DragOver to avoid re-extracting
        // CF_HDROP per cursor pixel.
        if accepts {
            if let Some(ref data) = *pdataobj {
                let paths = unsafe { paths_from_data_object(data) };
                DRAG_PATHS_HANDLER.with(|h| {
                    if let Some(cb) = &*h.borrow() {
                        cb(paths);
                    }
                });
            }
        }
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
        DRAG_PATHS_HANDLER.with(|h| {
            if let Some(cb) = &*h.borrow() {
                cb(Vec::new());
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

// -- Public API ----------------------------------------------------------------

/// Allow OLE drag-drop messages through UIPI so drops from lower-integrity
/// processes (e.g. non-elevated Explorer into an elevated Pathfinder) reach us.
/// Safe to call even when not elevated - no-op in that case.
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
///   3. Calls `RevokeDragDrop` first to clear winit's IDropTarget - winit
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
/// is `is_active` - true while dragging over the window, false on DragLeave or
/// after Drop completes.
pub fn register_drag_over_handler(handler: impl Fn(bool, i32, i32) + 'static) {
    DRAG_OVER_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
}

/// Install a callback that fires once with the list of dragged paths whenever
/// a drag enters the window (and again with an empty list on DragLeave). The
/// UI uses this to seed the ghost-overlay label so the user can see what they
/// are moving without waiting for the drop.
pub fn register_drag_paths_handler(handler: impl Fn(Vec<String>) + 'static) {
    DRAG_PATHS_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
}

/// Unregister drop target on app shutdown.
pub fn unregister_drop_target(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

// -- Shell file clipboard (Cut/Copy/Paste with Explorer) ---------------------

fn preferred_drop_effect_format() -> u32 {
    use std::sync::OnceLock;
    static FORMAT: OnceLock<u32> = OnceLock::new();
    *FORMAT.get_or_init(|| unsafe {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        RegisterClipboardFormatW(windows::core::w!("Preferred DropEffect"))
    })
}

/// Place file paths on the Windows clipboard (CF_HDROP + Preferred DropEffect).
pub fn set_shell_files_clipboard(paths: &[String], cut: bool) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    unsafe {
        use windows::Win32::Foundation::{GlobalFree, HANDLE};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
        };

        let hdrop = build_hdrop(paths).map_err(|e| e.to_string())?;
        let effect = if cut {
            DROPEFFECT_MOVE.0
        } else {
            DROPEFFECT_COPY.0
        };
        let effect_h = GlobalAlloc(GMEM_MOVEABLE, 4).map_err(|e| e.to_string())?;
        let effect_ptr = GlobalLock(effect_h) as *mut u32;
        if effect_ptr.is_null() {
            let _ = GlobalFree(Some(hdrop));
            let _ = GlobalFree(Some(effect_h));
            return Err("GlobalLock failed for drop effect".to_string());
        }
        *effect_ptr = effect;
        let _ = GlobalUnlock(effect_h);

        OpenClipboard(None).map_err(|e| e.to_string())?;
        if let Err(e) = EmptyClipboard() {
            let _ = CloseClipboard();
            let _ = GlobalFree(Some(hdrop));
            let _ = GlobalFree(Some(effect_h));
            return Err(e.to_string());
        }
        if let Err(e) = SetClipboardData(u32::from(CF_HDROP), Some(HANDLE(hdrop.0))) {
            let _ = CloseClipboard();
            let _ = GlobalFree(Some(hdrop));
            let _ = GlobalFree(Some(effect_h));
            return Err(e.to_string());
        }
        let fmt = preferred_drop_effect_format();
        if let Err(e) = SetClipboardData(fmt, Some(HANDLE(effect_h.0))) {
            let _ = CloseClipboard();
            let _ = GlobalFree(Some(effect_h));
            return Err(e.to_string());
        }
        CloseClipboard().map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Read CF_HDROP paths and whether the shell marked the operation as a move (cut).
pub fn try_read_shell_files_clipboard() -> Option<(Vec<String>, bool)> {
    unsafe {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

        OpenClipboard(None).ok()?;
        let hdrop = GetClipboardData(u32::from(CF_HDROP)).ok()?;
        let paths = paths_from_hglobal(HGLOBAL(hdrop.0));
        if paths.is_empty() {
            let _ = CloseClipboard();
            return None;
        }
        let cut = match GetClipboardData(preferred_drop_effect_format()) {
            Ok(effect_h) => {
                let ptr = GlobalLock(HGLOBAL(effect_h.0)) as *const u32;
                if ptr.is_null() {
                    false
                } else {
                    let effect = *ptr;
                    let _ = GlobalUnlock(HGLOBAL(effect_h.0));
                    effect == DROPEFFECT_MOVE.0
                }
            }
            Err(_) => false,
        };
        let _ = CloseClipboard();
        Some((paths, cut))
    }
}

pub fn clear_shell_files_clipboard() -> Result<(), String> {
    unsafe {
        use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};
        OpenClipboard(None).map_err(|e| e.to_string())?;
        if let Err(e) = EmptyClipboard() {
            let _ = CloseClipboard();
            return Err(e.to_string());
        }
        CloseClipboard().map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Start a shell drag operation (blocks until drop or cancel).
/// Returns the DROPEFFECT that the target performed (NONE = cancelled/rejected).
///
/// IDragSourceHelper is attached to the IDataObject before DoDragDrop so the
/// Windows shell renders a real drag image at the cursor - same mechanism
/// Explorer uses. This means the drag visual works even though slint may not
/// repaint our window during DoDragDrop's modal message pump.
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

        // Attach a drag image via IDragSourceHelper so the Windows shell paints
        // a real preview at the cursor. Failure is non-fatal - DoDragDrop still
        // runs and the OS falls back to the default cursor change.
        match attach_drag_image(&data, &paths) {
            Ok(()) => log("IDragSourceHelper attached"),
            Err(e) => log(&format!("IDragSourceHelper failed (non-fatal): {e}")),
        }

        let r = DoDragDrop(&data, &src, DROPEFFECT_COPY | DROPEFFECT_MOVE, &mut effect);
        log(&format!("DoDragDrop -> {:?}, effect=0x{:x}", r, effect.0));
        effect
    }
}

/// Build a File-Explorer-style drag image: the actual shell icon of the first
/// dragged file, a count badge if multi-file, and the file name label. Pinned
/// to IDataObject via IDragSourceHelper so the Windows shell renders it at the
/// cursor for the whole drag, regardless of whether our app repaints.
///
/// Returns Err on any COM/GDI failure; the caller treats that as non-fatal so
/// DoDragDrop still runs and the OS falls back to the default cursor change.
unsafe fn attach_drag_image(
    data: &IDataObject,
    paths: &[String],
) -> Result<(), String> {
    use windows::Win32::Foundation::{COLORREF, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CreateCompatibleDC, CreateDIBSection,
        CreateFontW, CreateSolidBrush, DEFAULT_PITCH, DEFAULT_QUALITY, DeleteDC,
        DeleteObject, DIB_RGB_COLORS, DrawTextW, DT_END_ELLIPSIS, DT_LEFT,
        DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FONT_CHARSET, FW_BOLD,
        FillRect, FrameRect, GetDC, HBITMAP, HGDIOBJ, OUT_DEFAULT_PRECIS,
        ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
        CLIP_DEFAULT_PRECIS,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        CLSID_DragDropHelper, IDragSourceHelper, SHDRAGIMAGE, SHFILEINFOW,
        SHGetFileInfoW, SHGFI_ICON, SHGFI_LARGEICON,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, DestroyIcon, HICON};
    use windows::core::PCWSTR;
    unsafe {
        // Layout: 320 wide x 80 tall. 48 px icon on the left with 12 px padding,
        // file name + count badge on the right. Large enough that even longer
        // file names read clearly; ellipsised by DrawText if they overflow.
        const W: i32 = 320;
        const H: i32 = 80;
        const ICON_SIZE: i32 = 48;
        const ICON_X: i32 = 14;
        const ICON_Y: i32 = (H - ICON_SIZE) / 2;
        const TEXT_X: i32 = ICON_X + ICON_SIZE + 14;

        // Compose label.
        let first_name = std::path::Path::new(&paths[0])
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| paths[0].clone());
        let label_wide: Vec<u16> = std::ffi::OsStr::new(&first_name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // Second line: optional file count.
        let count_text = if paths.len() > 1 {
            format!("+{} more file{}", paths.len() - 1, if paths.len() == 2 { "" } else { "s" })
        } else {
            String::new()
        };
        let count_wide: Vec<u16> = std::ffi::OsStr::new(&count_text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // Pull the actual shell icon for the first file so the drag preview
        // matches the file type the user is moving. Falls through to a default
        // generic icon if SHGetFileInfo fails.
        let path_wide: Vec<u16> = std::ffi::OsStr::new(&paths[0])
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut sfi = SHFILEINFOW::default();
        let _ = SHGetFileInfoW(
            PCWSTR(path_wide.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        let hicon: HICON = sfi.hIcon;

        // Build the bitmap.
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = W;
        bmi.bmiHeader.biHeight = -H; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let screen_dc = GetDC(None);
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbm: HBITMAP = CreateDIBSection(
            Some(screen_dc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
        .map_err(|e| format!("CreateDIBSection: {e}"))?;
        let memdc = CreateCompatibleDC(Some(screen_dc));
        if memdc.is_invalid() {
            let _ = ReleaseDC(None, screen_dc);
            let _ = DeleteObject(HGDIOBJ::from(hbm));
            if !hicon.is_invalid() { let _ = DestroyIcon(hicon); }
            return Err("CreateCompatibleDC failed".into());
        }
        let old_obj = SelectObject(memdc, HGDIOBJ::from(hbm));

        // Fill with magenta (color key) - the shell treats this colour as
        // fully transparent at draw time, so we get a non-rectangular drag
        // image with a rounded panel behind the icon and text.
        const KEY_COLORREF: u32 = 0x00_FF_00_FF; // BGR 0xFF00FF == magenta
        let key_brush = CreateSolidBrush(COLORREF(KEY_COLORREF));
        let full = RECT { left: 0, top: 0, right: W, bottom: H };
        FillRect(memdc, &full, key_brush);
        let _ = DeleteObject(HGDIOBJ::from(key_brush));

        // Dark translucent panel under the icon + text, rounded corners.
        let panel = CreateSolidBrush(COLORREF(0x00_30_30_30));
        let old_panel = SelectObject(memdc, HGDIOBJ::from(panel));
        let _ = RoundRect(memdc, 0, 0, W, H, 14, 14);
        SelectObject(memdc, old_panel);
        let _ = DeleteObject(HGDIOBJ::from(panel));

        // Thin accent frame around the panel for definition.
        let frame = CreateSolidBrush(COLORREF(0x00_AA_AA_AA));
        let frame_rect = RECT { left: 0, top: 0, right: W, bottom: H };
        FrameRect(memdc, &frame_rect, frame);
        let _ = DeleteObject(HGDIOBJ::from(frame));

        // Draw the shell icon on the left.
        if !hicon.is_invalid() {
            let _ = DrawIconEx(
                memdc,
                ICON_X,
                ICON_Y,
                hicon,
                ICON_SIZE,
                ICON_SIZE,
                0,
                None,
                DI_NORMAL,
            );
            let _ = DestroyIcon(hicon);
        }

        // File name label, bold.
        let face_w: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        let name_font = CreateFontW(
            18,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            FONT_CHARSET(0),
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            (FF_DONTCARE.0 as u32) | (DEFAULT_PITCH.0 as u32),
            PCWSTR(face_w.as_ptr()),
        );
        let old_font = SelectObject(memdc, HGDIOBJ::from(name_font));
        SetBkMode(memdc, TRANSPARENT);
        SetTextColor(memdc, COLORREF(0x00_F8_F8_F8));
        let mut name_rect = RECT {
            left: TEXT_X,
            top: if count_text.is_empty() { 0 } else { 14 },
            right: W - 12,
            bottom: if count_text.is_empty() { H } else { 44 },
        };
        let mut name_mut = label_wide.clone();
        let _ = DrawTextW(
            memdc,
            &mut name_mut,
            &mut name_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        // Optional count line, smaller, lighter.
        if !count_text.is_empty() {
            SelectObject(memdc, old_font);
            let _ = DeleteObject(HGDIOBJ::from(name_font));
            let count_font = CreateFontW(
                14,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                FONT_CHARSET(0),
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                DEFAULT_QUALITY,
                (FF_DONTCARE.0 as u32) | (DEFAULT_PITCH.0 as u32),
                PCWSTR(face_w.as_ptr()),
            );
            let old_count = SelectObject(memdc, HGDIOBJ::from(count_font));
            SetTextColor(memdc, COLORREF(0x00_BB_BB_BB));
            let mut count_rect = RECT {
                left: TEXT_X,
                top: 44,
                right: W - 12,
                bottom: H - 8,
            };
            let mut count_mut = count_wide.clone();
            let _ = DrawTextW(
                memdc,
                &mut count_mut,
                &mut count_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            SelectObject(memdc, old_count);
            let _ = DeleteObject(HGDIOBJ::from(count_font));
        } else {
            SelectObject(memdc, old_font);
            let _ = DeleteObject(HGDIOBJ::from(name_font));
        }

        SelectObject(memdc, old_obj);
        let _ = DeleteDC(memdc);
        let _ = ReleaseDC(None, screen_dc);

        // Pass HBITMAP to IDragSourceHelper. Windows takes ownership and
        // releases it once the drag completes. crColorKey makes magenta
        // pixels transparent so the rounded panel reads correctly.
        let helper: IDragSourceHelper =
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("CoCreateInstance: {e}"))?;
        let drag_image = SHDRAGIMAGE {
            sizeDragImage: windows::Win32::Foundation::SIZE { cx: W, cy: H },
            ptOffset: POINT { x: ICON_X + ICON_SIZE / 2, y: ICON_Y + ICON_SIZE / 2 },
            hbmpDragImage: hbm,
            crColorKey: COLORREF(KEY_COLORREF),
        };
        helper
            .InitializeFromBitmap(&drag_image, data)
            .map_err(|e| format!("InitializeFromBitmap: {e}"))?;
        Ok(())
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
