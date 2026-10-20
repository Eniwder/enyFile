//! Windows integration. Clipboard writes run on the UI thread (memory only, no I/O);
//! everything that touches the file system or another process runs on its own STA thread.
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{Com::*, DataExchange::*, Memory::*, Ole::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::SW_SHOWNORMAL},
    },
    core::{Interface, PCWSTR, w},
};

/// OLE (not plain COM) is required for the OLE clipboard; each worker thread owns one.
pub struct Apartment;
impl Apartment {
    pub fn new() -> Result<Self, String> {
        unsafe { OleInitialize(None) }.map_err(|e| e.to_string())?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

/// The OS path form the Shell understands: strips the `\\?\` prefix `canonicalize` adds.
pub fn shell_path(path: &Path) -> PathBuf {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    if !wide.starts_with(&prefix) {
        return path.to_path_buf();
    }
    let unc: Vec<u16> = r"UNC\".encode_utf16().collect();
    if wide[4..].starts_with(&unc) {
        let mut out = vec![92, 92];
        out.extend_from_slice(&wide[8..]);
        return PathBuf::from(OsString::from_wide(&out));
    }
    PathBuf::from(OsString::from_wide(&wide[4..]))
}
fn wide(path: &Path) -> Vec<u16> {
    shell_path(path)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect()
}
fn format(name: PCWSTR) -> u32 {
    unsafe { RegisterClipboardFormatW(name) }
}
fn preferred_effect() -> u32 {
    format(w!("Preferred DropEffect"))
}
fn etc(cf: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf as u16,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    }
}
fn global(bytes: &[u8]) -> Result<HGLOBAL, String> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| e.to_string())?;
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            let _ = GlobalFree(Some(h));
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

/// DROPFILES header (pFiles=20, fWide=1) followed by double-NUL-terminated UTF-16 paths.
pub fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    let mut bytes = vec![0u8; 20];
    bytes[..4].copy_from_slice(&20u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in paths {
        for unit in shell_path(path).as_os_str().encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

struct Clipboard;
impl Clipboard {
    /// Another process may hold the clipboard for a moment; retry briefly instead of failing.
    fn open(owner: Option<HWND>, attempts: u32) -> Result<Self, String> {
        for attempt in 0..attempts {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(Self);
            }
            if attempt + 1 < attempts {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Err("クリップボードを開けません（他のアプリが使用中）".into())
    }
    fn put(&self, cf: u32, bytes: &[u8]) -> Result<(), String> {
        let h = global(bytes)?;
        // On success the system owns the memory.
        if let Err(e) = unsafe { SetClipboardData(cf, Some(HANDLE(h.0))) } {
            unsafe {
                let _ = GlobalFree(Some(h));
            }
            return Err(e.to_string());
        }
        Ok(())
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Copy/Cut as Explorer expects: CF_HDROP plus "Preferred DropEffect" (COPY|LINK or MOVE).
/// The data is fully rendered, so it outlives this process and needs no COM callbacks.
pub fn set_clipboard(owner: HWND, paths: &[PathBuf], cut: bool) -> Result<(), String> {
    let clipboard = Clipboard::open(Some(owner), 5)?;
    unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
    clipboard.put(CF_HDROP.0 as u32, &hdrop_bytes(paths))?;
    let effect = if cut {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_LINK.0)
    };
    clipboard.put(preferred_effect(), &effect.0.to_le_bytes())
}

/// Plain text on the clipboard (path bar > パスのコピー).
pub fn set_clipboard_text(owner: HWND, text: &str) -> Result<(), String> {
    let clipboard = Clipboard::open(Some(owner), 5)?;
    unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
    let bytes: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
    clipboard.put(CF_UNICODETEXT.0 as u32, &bytes)
}

/// Whole-window opacity (0.3..=1.0) through a layered window; 1.0 removes the layer.
pub fn set_opacity(hwnd: HWND, opacity: f32) {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if opacity >= 0.995 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
        } else {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            let alpha = (opacity.clamp(0.3, 1.0) * 255.0).round() as u8;
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        }
    }
}

fn read_dword(data: &IDataObject, cf: u32) -> Option<u32> {
    unsafe {
        let mut medium = data.GetData(&etc(cf)).ok()?;
        let value = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
            let h = medium.u.hGlobal;
            let ptr = GlobalLock(h);
            let value = (!ptr.is_null() && GlobalSize(h) >= 4)
                .then(|| u32::from_le_bytes(*ptr.cast::<[u8; 4]>()));
            let _ = GlobalUnlock(h);
            value
        } else {
            None
        };
        ReleaseStgMedium(&mut medium);
        value
    }
}
fn write_dword(data: &IDataObject, cf: u32, value: u32) {
    let Ok(h) = global(&value.to_le_bytes()) else {
        return;
    };
    let medium = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: h },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    };
    // fRelease=true hands the memory to the data object only when the call succeeds.
    if unsafe { data.SetData(&etc(cf), &medium, true) }.is_err() {
        unsafe {
            let _ = GlobalFree(Some(h));
        }
    }
}

fn hdrop_paths(data: &IDataObject) -> Option<Vec<PathBuf>> {
    unsafe {
        let mut medium = data.GetData(&etc(CF_HDROP.0 as u32)).ok()?;
        let drop = HDROP(medium.u.hGlobal.0);
        let mut paths = Vec::new();
        for i in 0..DragQueryFileW(drop, u32::MAX, None) {
            let mut buffer = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
            let len = DragQueryFileW(drop, i, Some(&mut buffer)) as usize;
            paths.push(PathBuf::from(OsString::from_wide(&buffer[..len])));
        }
        ReleaseStgMedium(&mut medium);
        Some(paths)
    }
}

/// Reads file paths and the preferred effect from the clipboard (diagnostics and tests).
pub fn read_clipboard() -> Result<(Vec<PathBuf>, u32), String> {
    let data = unsafe { OleGetClipboard() }.map_err(|e| e.to_string())?;
    let paths = hdrop_paths(&data).ok_or("クリップボードにファイルがありません")?;
    let effect = read_dword(&data, preferred_effect()).unwrap_or(DROPEFFECT_COPY.0);
    Ok((paths, effect))
}

/// Files on the clipboard. `paths` is None for Shell items without file system paths.
pub struct Clip {
    pub paths: Option<Vec<PathBuf>>,
    pub moved: bool,
}
/// Call on a thread that owns an [`Apartment`].
pub fn read_clip() -> Result<Clip, String> {
    unsafe {
        let data = OleGetClipboard().map_err(|e| e.to_string())?;
        let paths = hdrop_paths(&data);
        let has_items = paths.is_some()
            || data
                .QueryGetData(&etc(format(w!("Shell IDList Array"))))
                .is_ok();
        if !has_items {
            return Err("クリップボードにファイルがありません".into());
        }
        let effect = read_dword(&data, preferred_effect()).unwrap_or(DROPEFFECT_COPY.0);
        Ok(Clip {
            paths,
            moved: effect & DROPEFFECT_MOVE.0 != 0 && effect & DROPEFFECT_COPY.0 == 0,
        })
    }
}

fn operation(flags: FILEOPERATION_FLAGS) -> Result<IFileOperation, String> {
    unsafe {
        let op: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        op.SetOperationFlags(flags).map_err(|e| e.to_string())?;
        Ok(op)
    }
}
fn item(path: &Path) -> Result<IShellItem, String> {
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide(path).as_ptr()), None) }
        .map_err(|e| format!("{}: {e}", shell_path(path).display()))
}
fn finish(op: &IFileOperation) -> Result<bool, String> {
    unsafe {
        let performed = op.PerformOperations();
        let aborted = op
            .GetAnyOperationsAborted()
            .map(|b| b.as_bool())
            .unwrap_or(true);
        match performed {
            Err(e) if !aborted => Err(e.to_string()),
            _ => Ok(aborted),
        }
    }
}

/// Runs a plan whose conflicts the user already resolved: the copy engine never prompts.
/// Items being replaced are deleted first, without the Recycle Bin (the user's choice).
/// Returns whether anything was aborted. Call on a thread that owns an [`Apartment`].
pub fn transfer(plan: &crate::paste::Plan) -> Result<bool, String> {
    // The last check before anything is deleted or overwritten.
    plan.verify()?;
    if !plan.remove.is_empty() {
        let op = operation(FOF_NOCONFIRMATION)?;
        for path in &plan.remove {
            unsafe { op.DeleteItem(&item(path)?, None) }.map_err(|e| e.to_string())?;
        }
        if finish(&op)? {
            return Ok(true);
        }
    }
    if plan.ops.is_empty() {
        return Ok(false);
    }
    let op = operation(FILEOPERATION_FLAGS(
        FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOCONFIRMMKDIR.0,
    ))?;
    let mut folders = std::collections::HashMap::new();
    unsafe {
        for step in &plan.ops {
            if !folders.contains_key(&step.dir) {
                folders.insert(step.dir.clone(), item(&step.dir)?);
            }
            let name: Option<Vec<u16>> =
                step.name.as_ref().map(|n| n.encode_utf16().chain(Some(0)).collect());
            let name = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
            let source = item(&step.source)?;
            if plan.moved {
                op.MoveItem(&source, &folders[&step.dir], name, None)
            } else {
                op.CopyItem(&source, &folders[&step.dir], name, None)
            }
            .map_err(|e| e.to_string())?;
        }
    }
    finish(&op)
}
/// Ctrl+V into the items' own folder: Explorer-style "name - コピー" duplicates, no prompt.
pub fn duplicate(paths: &[PathBuf], destination: &Path) -> Result<bool, String> {
    let op = operation(FILEOPERATION_FLAGS(
        FOF_ALLOWUNDO.0 | FOF_RENAMEONCOLLISION.0,
    ))?;
    let target = item(destination)?;
    unsafe {
        for path in paths {
            op.CopyItem(&item(path)?, &target, PCWSTR::null(), None)
                .map_err(|e| e.to_string())?;
        }
    }
    finish(&op)
}
/// After moving cut items: tell the source (delete-on-paste protocol) and clear the clipboard
/// so a second paste cannot refer to files that are gone. Skipped if the clipboard changed.
pub fn finish_cut(sources: &[PathBuf]) {
    unsafe {
        let Ok(data) = OleGetClipboard() else { return };
        if hdrop_paths(&data).as_deref() != Some(sources) {
            return;
        }
        // We did an optimized move, so the source must not delete anything.
        write_dword(&data, format(w!("Performed DropEffect")), DROPEFFECT_NONE.0);
        write_dword(&data, format(w!("Paste Succeeded")), DROPEFFECT_MOVE.0);
        if let Ok(_clipboard) = Clipboard::open(None, 50) {
            let _ = EmptyClipboard();
        }
    }
}
/// Fallback for clipboard items without file system paths (e.g. some virtual folders):
/// hands the data object to the copy engine, which then shows its own Windows dialogs.
/// Call on a thread that owns an [`Apartment`].
pub fn paste_shell_items(destination: &Path, moved: bool) -> Result<bool, String> {
    unsafe {
        let data = OleGetClipboard().map_err(|e| e.to_string())?;
        let op = operation(FOF_ALLOWUNDO)?;
        let target = item(destination)?;
        let items: windows::core::IUnknown = data.cast().map_err(|e| e.to_string())?;
        if moved {
            op.MoveItems(&items, &target)
        } else {
            op.CopyItems(&items, &target)
        }
        .map_err(|e| e.to_string())?;
        finish(&op)
    }
}

/// F2 rename through the Shell (undoable from Explorer like any Shell rename).
pub fn rename(path: &Path, name: &str) -> Result<(), String> {
    let _ole = Apartment::new()?;
    let op = operation(FOF_ALLOWUNDO)?;
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { op.RenameItem(&item(path)?, PCWSTR(name.as_ptr()), None) }.map_err(|e| e.to_string())?;
    match finish(&op)? {
        false => Ok(()),
        true => Err("名前の変更は中止されました".into()),
    }
}

/// Recycle Bin by default; `permanent` matches Shift+Delete. Windows shows its own confirmation.
pub fn delete(paths: &[PathBuf], permanent: bool) -> Result<bool, String> {
    let _ole = Apartment::new()?;
    let flags = if permanent {
        FOF_WANTNUKEWARNING
    } else {
        FILEOPERATION_FLAGS(FOF_ALLOWUNDO.0 | FOFX_RECYCLEONDELETE.0)
    };
    let op = operation(flags)?;
    unsafe {
        for path in paths {
            op.DeleteItem(&item(path)?, None)
                .map_err(|e| e.to_string())?;
        }
    }
    finish(&op)
}

pub fn open(path: &Path) -> Result<(), String> {
    let _ole = Apartment::new()?;
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide(path).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        Err(format!("ShellExecute error {}", result.0 as isize))
    } else {
        Ok(())
    }
}

/// Windows folder picker. Blocks until closed: call on a worker thread. None if cancelled.
pub fn pick_folder(start: Option<&Path>) -> Option<PathBuf> {
    let _ole = Apartment::new().ok()?;
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        dialog
            .SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .ok()?;
        if let Some(folder) = start.and_then(|s| item(s).ok()) {
            let _ = dialog.SetFolder(&folder);
        }
        dialog.Show(None).ok()?;
        let name = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as *const _));
        path
    }
}

/// Removes the file-drop target winit registers by default. Slint ignores those events,
/// so leaving it would show a copy cursor over the window and then do nothing.
pub fn revoke_drop_target(hwnd: HWND) {
    let _ = unsafe { RevokeDragDrop(hwnd) };
}
/// Whether the top strip of a window at (x, y) with this width lies on some monitor, so a
/// saved position from an unplugged display does not reopen the window out of reach.
pub fn on_screen(x: i32, y: i32, width: u32) -> bool {
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
    let strip = RECT { left: x, top: y, right: x.saturating_add(width as i32), bottom: y.saturating_add(32) };
    !unsafe { MonitorFromRect(&strip, MONITOR_DEFAULTTONULL) }.is_invalid()
}
/// Dark title bar to match the dark theme (ignored on Windows versions without it).
pub fn dark_title_bar(hwnd: HWND) {
    use windows::{
        Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        core::BOOL,
    };
    let on = BOOL(1);
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &on as *const BOOL as *const _,
            std::mem::size_of::<BOOL>() as u32,
        )
    };
    // The window already exists: redraw the frame so the change shows immediately.
    use windows::Win32::UI::WindowsAndMessaging::*;
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

/// Slint only sets the small icon (scaled from a PNG); use the exe's multi-size icon
/// resource (build.rs) for both so the taskbar and Alt+Tab get crisp sizes too.
pub fn window_icon(hwnd: HWND) {
    use windows::Win32::{System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*};
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else { return };
    for (kind, cx, cy) in [(ICON_BIG, SM_CXICON, SM_CYICON), (ICON_SMALL, SM_CXSMICON, SM_CYSMICON)] {
        let icon = unsafe {
            LoadImageW(
                Some(module.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                GetSystemMetrics(cx),
                GetSystemMetrics(cy),
                LR_DEFAULTCOLOR,
            )
        };
        // Leaving the handle alive is fine: the window lives until the process exits.
        if let Ok(icon) = icon {
            unsafe { SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon.0 as isize))) };
        }
    }
}

struct Event(HANDLE);
unsafe impl Send for Event {}
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
/// Watches one folder (not its subtree). Dropping it stops the watcher thread.
pub struct Watch(Arc<Event>);
impl Drop for Watch {
    fn drop(&mut self) {
        let _ = unsafe { SetEvent(self.0.0) };
    }
}
/// Calls `changed` after a burst of changes settles (250 ms quiet, at most 1 s while busy).
/// The handle is opened on the watcher thread, so a slow share never blocks the UI.
pub fn watch(dir: PathBuf, changed: impl Fn() + Send + 'static) -> Option<Watch> {
    let stop = Arc::new(Event(unsafe { CreateEventW(None, true, false, None) }.ok()?));
    let thread_stop = stop.clone();
    std::thread::spawn(move || {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE;
        let Ok(change) =
            (unsafe { FindFirstChangeNotificationW(PCWSTR(wide(&dir).as_ptr()), false, filter) })
        else {
            return;
        };
        let handles = [change, thread_stop.0];
        'outer: loop {
            if unsafe { WaitForMultipleObjects(&handles, false, INFINITE) } != WAIT_OBJECT_0 {
                break;
            }
            let first = Instant::now();
            loop {
                if unsafe { FindNextChangeNotification(change) }.is_err() {
                    break 'outer;
                }
                let waited = unsafe { WaitForMultipleObjects(&handles, false, 250) };
                if waited == WAIT_TIMEOUT || first.elapsed() > Duration::from_secs(1) {
                    break;
                }
                if waited != WAIT_OBJECT_0 {
                    break 'outer;
                }
            }
            changed();
        }
        let _ = unsafe { FindCloseChangeNotification(change) };
    });
    Some(Watch(stop))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hdrop_layout_and_prefix_stripping() {
        let bytes = hdrop_bytes(&[
            PathBuf::from(r"\\?\C:\demo\日本語_😀.txt"),
            PathBuf::from(r"\\?\UNC\server\share\a.txt"),
        ]);
        let wide: Vec<u16> = bytes[20..]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let text = String::from_utf16(&wide).unwrap();
        assert_eq!(text, "C:\\demo\\日本語_😀.txt\0\\\\server\\share\\a.txt\0\0");
    }
}
