//! Windows integration. Clipboard writes run on the UI thread (memory only, no I/O);
//! everything that touches the file system or another process runs on its own STA thread.
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{Com::*, DataExchange::*, Memory::*, Ole::*, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::SW_SHOWNORMAL},
    },
    core::{HRESULT, Interface, PCWSTR, w},
};

/// DLLs loaded by name come from System32 only, not from the exe's folder (often Downloads,
/// where a planted DLL would run as us) or the working folder. Shell extensions still find
/// their own DLLs: COM loads them by full path with the altered search order.
/// The exe's static imports are covered by /DEPENDENTLOADFLAG (build.rs).
pub fn harden_dll_search() {
    use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories};
    let _ = unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) };
}
/// System32, for starting Windows' own programs by full path: a bare name is looked up in
/// the exe's folder first.
pub fn system_dir() -> PathBuf {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buffer = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if len == 0 || len >= buffer.len() {
        return PathBuf::from(r"C:\Windows\System32");
    }
    PathBuf::from(OsString::from_wide(&buffer[..len]))
}

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
/// [`shell_path`] for paths handed to the Shell or Win32. Refused when dropping `\\?\` makes
/// Win32 normalization point elsewhere (`victim.` becomes `victim`, `nul.txt` a device):
/// failing is better than touching a different file.
pub fn os_path(path: &Path) -> Result<PathBuf, String> {
    let plain = shell_path(path);
    if plain.as_os_str() != path.as_os_str() {
        let odd = path.components().find_map(|c| match c {
            std::path::Component::Normal(name) => win32_alters(&name.to_string_lossy()).then_some(name),
            std::path::Component::CurDir | std::path::Component::ParentDir => Some(c.as_os_str()),
            _ => None,
        });
        if let Some(name) = odd {
            return Err(format!("Windows の通常のパスでは扱えない名前です: {}", name.to_string_lossy()));
        }
    }
    Ok(plain)
}
/// Win32 strips trailing dots and spaces, and maps reserved device names even with an extension.
fn win32_alters(name: &str) -> bool {
    if name.ends_with(['.', ' ']) {
        return true;
    }
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_uppercase();
    let numbered = |prefix: &str| {
        base.strip_prefix(prefix).is_some_and(|n| {
            let mut chars = n.chars();
            matches!((chars.next(), chars.next()), (Some('0'..='9' | '¹' | '²' | '³'), None))
        })
    };
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || numbered("COM")
        || numbered("LPT")
}
fn wide(path: &Path) -> Result<Vec<u16>, String> {
    Ok(os_path(path)?.as_os_str().encode_wide().chain(Some(0)).collect())
}
/// The `\\?\` form, for Win32 calls on paths that may exceed MAX_PATH (the manifest's
/// long-path awareness only counts where the system enables it). A plain path is normalized
/// first, as Win32 would.
fn verbatim(path: &Path) -> PathBuf {
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    if text.starts_with(&r"\\?\".encode_utf16().collect::<Vec<_>>()) {
        return path.to_path_buf();
    }
    let full: Vec<u16> = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).as_os_str().encode_wide().collect();
    let out: Vec<u16> = match full.strip_prefix(&[92, 92][..]) {
        Some(unc) => r"\\?\UNC\".encode_utf16().chain(unc.iter().copied()).collect(),
        None => r"\\?\".encode_utf16().chain(full).collect(),
    };
    PathBuf::from(OsString::from_wide(&out))
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
pub fn hdrop_bytes(paths: &[PathBuf]) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; 20];
    bytes[..4].copy_from_slice(&20u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in paths {
        for unit in os_path(path)?.as_os_str().encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&[0, 0]);
    Ok(bytes)
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
    // Before emptying: a refused path leaves the clipboard as it was.
    let hdrop = hdrop_bytes(paths)?;
    let clipboard = Clipboard::open(Some(owner), 5)?;
    unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
    clipboard.put(CF_HDROP.0 as u32, &hdrop)?;
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

/// DragQueryFileW trusts the block: the list must start inside it and end with a double
/// NUL before its end. Another process supplies it, so check first.
fn hdrop_valid(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..20) else { return false };
    let at = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let wide = header[16..20] != [0; 4];
    let Some(list) = bytes.get(at..).filter(|_| at >= 20) else { return false };
    // The list ends at the first empty name.
    let ends = |units: &[u16]| units.first() == Some(&0) || units.windows(2).any(|w| w == [0, 0]);
    if wide {
        ends(&list.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>())
    } else {
        ends(&list.iter().map(|&b| u16::from(b)).collect::<Vec<_>>())
    }
}
fn hdrop_paths(data: &IDataObject) -> Option<Vec<PathBuf>> {
    unsafe {
        let mut medium = data.GetData(&etc(CF_HDROP.0 as u32)).ok()?;
        let valid = medium.tymed == TYMED_HGLOBAL.0 as u32 && {
            let h = medium.u.hGlobal;
            let ptr = GlobalLock(h);
            let valid = !ptr.is_null() && hdrop_valid(std::slice::from_raw_parts(ptr.cast::<u8>(), GlobalSize(h)));
            let _ = GlobalUnlock(h);
            valid
        };
        if !valid {
            // Left to the copy engine, like Shell items without paths.
            ReleaseStgMedium(&mut medium);
            return None;
        }
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
    let wide = wide(path)?;
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
        .map_err(|e| format!("{}: {e}", shell_path(path).display()))
}
/// Items from several folders (search results) as one Shell selection.
pub fn item_array(paths: &[PathBuf]) -> Result<IShellItemArray, String> {
    let mut pidls: Vec<*const windows::Win32::UI::Shell::Common::ITEMIDLIST> = Vec::new();
    let mut failed = None;
    for path in paths {
        let wide = match wide(path) {
            Ok(wide) => wide,
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        let mut pidl = std::ptr::null_mut();
        match unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) } {
            Ok(()) => pidls.push(pidl),
            Err(e) => {
                failed = Some(format!("{}: {e}", shell_path(path).display()));
                break;
            }
        }
    }
    let array = match failed {
        None => unsafe { SHCreateShellItemArrayFromIDLists(&pidls) }.map_err(|e| e.to_string()),
        Some(e) => Err(e),
    };
    for pidl in pidls {
        unsafe { CoTaskMemFree(Some(pidl as *const _)) };
    }
    array
}
/// Drags `paths` out to another app (a browser's upload field, Explorer, ...) as Explorer
/// does: the Shell's own data object, copy / move / link left to the drop target, which
/// also carries out a move. Blocks in the system's drag loop; call on the window's thread
/// with nothing borrowed. Returns the effect the target chose.
pub fn drag_out(paths: &[PathBuf]) -> Result<DROPEFFECT, String> {
    let data: IDataObject = unsafe { item_array(paths)?.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.to_string())?;
    let allowed = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0);
    unsafe { SHDoDragDrop(None, &data, None::<&IDropSource>, allowed) }.map_err(|e| e.to_string())
}
/// The mouse position in `hwnd`'s client area, in physical pixels.
pub fn cursor_in(hwnd: HWND) -> (i32, i32) {
    let mut point = POINT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
        let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point);
    }
    (point.x, point.y)
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

/// Notes items the copy engine left out: an error the user skipped in its own dialog, which
/// it does not count as aborting. The callback runs before each item is deleted: Windows asks
/// its own confirmation first, so the first call means the user said yes.
#[windows::core::implement(IFileOperationProgressSink)]
struct Missed(Arc<AtomicBool>, Option<Box<dyn Fn()>>);
impl Missed {
    fn note(&self, hr: HRESULT) -> windows::core::Result<()> {
        if hr.is_err() || hr == COPYENGINE_S_USER_IGNORED {
            self.0.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}
type Item<'a> = windows::core::Ref<'a, IShellItem>;
impl IFileOperationProgressSink_Impl for Missed_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Item, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(&self, _: u32, _: Item, _: &PCWSTR, hr: HRESULT, _: Item) -> windows::core::Result<()> {
        self.note(hr)
    }
    fn PreMoveItem(&self, _: u32, _: Item, _: Item, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(&self, _: u32, _: Item, _: Item, _: &PCWSTR, hr: HRESULT, _: Item) -> windows::core::Result<()> {
        self.note(hr)
    }
    fn PreCopyItem(&self, _: u32, _: Item, _: Item, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(&self, _: u32, _: Item, _: Item, _: &PCWSTR, hr: HRESULT, _: Item) -> windows::core::Result<()> {
        self.note(hr)
    }
    fn PreDeleteItem(&self, _: u32, _: Item) -> windows::core::Result<()> {
        if let Some(deleting) = &self.1 {
            deleting();
        }
        Ok(())
    }
    fn PostDeleteItem(&self, _: u32, _: Item, hr: HRESULT, _: Item) -> windows::core::Result<()> {
        self.note(hr)
    }
    fn PreNewItem(&self, _: u32, _: Item, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(&self, _: u32, _: Item, _: &PCWSTR, _: &PCWSTR, _: u32, hr: HRESULT, _: Item) -> windows::core::Result<()> {
        self.note(hr)
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Transferred {
    pub aborted: bool,
    /// The copy engine left some items out (the user skipped an error in its dialog). The
    /// replaced items are kept: one may hold the only copy of what was left out.
    pub missed: bool,
    /// Replaced items still under their temporary name: the new item already took the
    /// name when the paste stopped, or deleting the old one failed.
    pub kept: Vec<PathBuf>,
}
/// Runs a plan whose conflicts the user already resolved: the copy engine never prompts.
/// Items being replaced are renamed aside first, and deleted (without the Recycle Bin, the
/// user's choice) only once the paste succeeded; otherwise they get their names back.
/// Call on a thread that owns an [`Apartment`].
pub fn transfer(plan: &crate::paste::Plan) -> Result<Transferred, String> {
    // The last check before anything is renamed or overwritten.
    let inside = plan.verify()?;
    let direct = beyond_shell(plan, &inside)?;
    let aside = set_aside(&plan.remove)?;
    // A source inside a replaced item (Folder\Folder replacing Folder) moved aside with it.
    let sources: Vec<PathBuf> = plan
        .ops
        .iter()
        .zip(inside)
        .map(|(op, inside)| inside.map_or_else(|| op.source.clone(), |(i, rest)| aside[i].1.join(rest)))
        .collect();
    match paste_ops(plan, &sources, &direct) {
        Ok((false, false)) => Ok(Transferred { kept: delete_aside(&aside), ..Default::default() }),
        Ok((aborted, missed)) => Ok(Transferred { aborted, missed, kept: restore(&aside) }),
        Err(e) => Err(with_kept(e, &restore(&aside))),
    }
}
/// Appended to a failure: where replaced items were left, so the user can find them.
pub fn with_kept(message: String, kept: &[PathBuf]) -> String {
    if kept.is_empty() {
        return message;
    }
    let names: Vec<_> = kept.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy()).collect();
    format!("{message}（置換前のフォルダが残っています: {}）", names.join("、"))
}
/// Same-folder renames: instant, and they fail rather than replace an existing item.
/// A virus scanner or the indexer may hold something inside open for a moment (the copy
/// engine's own deletes wait that out too), so a busy item is retried for about a second.
fn rename_plain(from: &Path, to: &Path) -> windows::core::Result<()> {
    move_plain(from, to, MOVE_FILE_FLAGS(0))
}
/// A move within one volume (no copy fallback), for renames and for items the copy engine
/// cannot reach. A folder moves as one rename, however deep it goes.
fn move_plain(from: &Path, to: &Path, flags: MOVE_FILE_FLAGS) -> windows::core::Result<()> {
    let wide = |p: &Path| -> Vec<u16> { verbatim(p).as_os_str().encode_wide().chain(Some(0)).collect() };
    let (from, to) = (wide(from), wide(to));
    let busy = [ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION, ERROR_ACCESS_DENIED];
    for _ in 0..10 {
        match unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) } {
            Err(e) if busy.iter().any(|c| e.code() == c.to_hresult()) => std::thread::sleep(Duration::from_millis(100)),
            done => return done,
        }
    }
    unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) }
}
/// Longest path (UTF-16 units) the copy engine handles. It has no long-path support, even
/// with long paths enabled in Windows: past this it fails partway, leaving a split tree.
const SHELL_MAX: usize = 259;
/// CreateDirectory keeps room for an 8.3 file name inside a new folder.
const FOLDER_ROOM: usize = 12;
/// How far the deepest item below folder `dir` reaches past its path, counting
/// [`FOLDER_ROOM`] for folders. Links are not followed. Stops once past `limit`.
fn reach(dir: &Path, limit: usize) -> usize {
    use std::os::windows::fs::MetadataExt;
    let mut deepest = FOLDER_ROOM;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if deepest > limit {
            break;
        }
        let attributes = entry.metadata().map_or(0, |m| m.file_attributes());
        let here = 1 + entry.file_name().encode_wide().count();
        let below = if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            0
        } else if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            FOLDER_ROOM
        } else {
            reach(&entry.path(), limit.saturating_sub(here))
        };
        deepest = deepest.max(here + below);
    }
    deepest
}
/// For each op, whether it goes past [`SHELL_MAX`] and is moved here instead of by the copy
/// engine. Checked before anything changes: a long copy, or a long move to another volume,
/// refuses the whole paste. `inside` is from [`crate::paste::Plan::verify`].
fn beyond_shell(plan: &crate::paste::Plan, inside: &[Option<(usize, PathBuf)>]) -> Result<Vec<bool>, String> {
    use std::os::windows::fs::MetadataExt;
    let len = |p: &Path| shell_path(p).as_os_str().encode_wide().count();
    let mut direct = Vec::new();
    for (op, inside) in plan.ops.iter().zip(inside) {
        // A source inside a replaced folder is reached through its set-aside name.
        let aside = if inside.is_some() { "~enyfile-old-99".len() } else { 0 };
        let base = (len(&op.source) + aside).max(len(&op.target()));
        let attributes = std::fs::symlink_metadata(&op.source).map_or(0, |m| m.file_attributes());
        let extent = if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
            0
        } else if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            FOLDER_ROOM
        } else {
            reach(&op.source, SHELL_MAX.saturating_sub(base))
        };
        let long = base + extent > SHELL_MAX;
        if long {
            let volume = |p: &Path| crate::pathid::volume(&verbatim(p));
            let from = op.source.parent().and_then(volume);
            if !plan.moved || from.is_none() || from != volume(&op.dir) {
                return Err(format!(
                    "パスが {} 文字を超える項目があるため中止しました（Windows のコピーエンジンが扱えません。同じドライブ内の移動はできます）: {}",
                    SHELL_MAX + 1,
                    shell_path(&op.source).display()
                ));
            }
        }
        direct.push(long);
    }
    Ok(direct)
}
/// `name~enyfile-old` (then `-2`, `-3`...): recognizable if a crash leaves it behind.
fn set_aside(paths: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut aside = Vec::new();
    for path in paths {
        let mut renamed = Err(String::new());
        for n in 1..100 {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(if n == 1 { "~enyfile-old".to_string() } else { format!("~enyfile-old-{n}") });
            let temp = path.with_file_name(name);
            match rename_plain(path, &temp) {
                Ok(()) => {
                    renamed = Ok(temp);
                    break;
                }
                Err(e) if [ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS].iter().any(|c| e.code() == c.to_hresult()) => {}
                Err(e) => {
                    renamed = Err(e.message());
                    break;
                }
            }
        }
        match renamed {
            Ok(temp) => aside.push((path.clone(), temp)),
            Err(e) => {
                let e = format!("置換するフォルダを退避できません: {}: {e}", shell_path(path).display());
                return Err(with_kept(e, &restore(&aside)));
            }
        }
    }
    Ok(aside)
}
/// Back to the original names. One the new item already took stays aside: after a move
/// it holds the moved data, so neither side may be deleted. Returns what stayed aside.
fn restore(aside: &[(PathBuf, PathBuf)]) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = aside
        .iter()
        .rev()
        .filter(|(path, temp)| rename_plain(temp, path).is_err())
        .map(|(_, temp)| temp.clone())
        .collect();
    kept.reverse();
    kept
}
/// Returns what could not be deleted.
fn delete_aside(aside: &[(PathBuf, PathBuf)]) -> Vec<PathBuf> {
    if aside.is_empty() {
        return Vec::new();
    }
    let deleted = operation(FOF_NOCONFIRMATION).and_then(|op| {
        for (_, temp) in aside {
            unsafe { op.DeleteItem(&item(temp)?, None) }.map_err(|e| e.to_string())?;
        }
        finish(&op)
    });
    if deleted == Ok(false) {
        return Vec::new();
    }
    aside.iter().map(|(_, temp)| temp.clone()).filter(|temp| std::fs::symlink_metadata(temp).is_ok()).collect()
}
/// The copies and moves themselves: whether aborted, and whether any item was left out.
/// `direct` ops (see [`beyond_shell`]) are moved here, after the copy engine's, unless the
/// user aborted those.
fn paste_ops(plan: &crate::paste::Plan, sources: &[PathBuf], direct: &[bool]) -> Result<(bool, bool), String> {
    let steps = || plan.ops.iter().zip(sources).zip(direct);
    let (aborted, missed) = shell_ops(plan, steps().filter(|(_, d)| !**d).map(|(s, _)| s))?;
    if !aborted {
        for ((step, source), _) in steps().filter(|(_, d)| **d) {
            let flags = if step.overwrites.is_some() { MOVEFILE_REPLACE_EXISTING } else { MOVE_FILE_FLAGS(0) };
            move_plain(source, &step.target(), flags).map_err(|e| format!("{}: {}", shell_path(source).display(), e.message()))?;
        }
    }
    Ok((aborted, missed))
}
fn shell_ops<'a>(
    plan: &crate::paste::Plan,
    steps: impl Iterator<Item = (&'a crate::paste::Op, &'a PathBuf)>,
) -> Result<(bool, bool), String> {
    let mut steps = steps.peekable();
    if steps.peek().is_none() {
        return Ok((false, false));
    }
    let op = operation(FILEOPERATION_FLAGS(
        FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOCONFIRMMKDIR.0,
    ))?;
    let missed = Arc::new(AtomicBool::new(false));
    let sink: IFileOperationProgressSink = Missed(missed.clone(), None).into();
    let mut folders = std::collections::HashMap::new();
    unsafe {
        op.Advise(&sink).map_err(|e| e.to_string())?;
        for (step, source) in steps {
            if !folders.contains_key(&step.dir) {
                folders.insert(step.dir.clone(), item(&step.dir)?);
            }
            let name: Option<Vec<u16>> =
                step.name.as_ref().map(|n| n.encode_utf16().chain(Some(0)).collect());
            let name = name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()));
            let source = item(source)?;
            if plan.moved {
                op.MoveItem(&source, &folders[&step.dir], name, None)
            } else {
                op.CopyItem(&source, &folders[&step.dir], name, None)
            }
            .map_err(|e| e.to_string())?;
        }
    }
    let aborted = finish(&op)?;
    Ok((aborted, missed.load(Ordering::Relaxed)))
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

/// Recycle Bin by default; `permanent` matches Shift+Delete. Windows shows its own confirmation;
/// `deleting` is called once it is answered yes and the first item is about to go.
pub fn delete(paths: &[PathBuf], permanent: bool, deleting: impl Fn() + 'static) -> Result<bool, String> {
    let _ole = Apartment::new()?;
    let flags = if permanent {
        FOF_WANTNUKEWARNING
    } else {
        FILEOPERATION_FLAGS(FOF_ALLOWUNDO.0 | FOFX_RECYCLEONDELETE.0)
    };
    let op = operation(flags)?;
    let once = std::cell::Cell::new(false);
    let deleting = move || {
        if !once.replace(true) {
            deleting();
        }
    };
    let sink: IFileOperationProgressSink = Missed(Arc::new(AtomicBool::new(false)), Some(Box::new(deleting))).into();
    unsafe {
        op.Advise(&sink).map_err(|e| e.to_string())?;
        for path in paths {
            op.DeleteItem(&item(path)?, None)
                .map_err(|e| e.to_string())?;
        }
    }
    finish(&op)
}

/// Starts in the file's own folder, as from Explorer: without it a .bat or .exe would run in
/// enyFile's working folder and miss the files next to it.
pub fn open(path: &Path) -> Result<(), String> {
    execute(w!("open"), path, path.parent())
}
/// The "explore" verb, not "open": a folder's default verb may belong to another file manager.
pub fn explore(dir: &Path) -> Result<(), String> {
    execute(w!("explore"), dir, None)
}
fn execute(verb: PCWSTR, path: &Path, folder: Option<&Path>) -> Result<(), String> {
    let folder = folder.map(wide).transpose()?;
    let path = wide(path)?;
    let _ole = Apartment::new()?;
    let result = unsafe {
        ShellExecuteW(
            None,
            verb,
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            folder.as_ref().map_or(PCWSTR::null(), |f| PCWSTR(f.as_ptr())),
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
thread_local! {
    // Alt went down and nothing else has happened since.
    static ALT_ALONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    // ...and then came up: a lone Alt tap, taken by take_alt_tap.
    static ALT_TAP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
/// Whether the Alt release being handled ends a lone Alt tap. Slint cannot tell: winit also
/// sends a synthetic Alt release when the window loses focus (Alt+Tab away).
pub fn take_alt_tap() -> bool {
    ALT_TAP.with(|t| t.replace(false))
}
/// Watches the main window's messages: calls `on_click` when a mouse button goes down on the
/// window frame (title bar, borders), which Slint never sees, and notes lone Alt taps.
/// Installed once for the main window, so `on_click` is never freed.
pub fn hook_window(hwnd: HWND, on_click: Box<dyn Fn()>) {
    use windows::Win32::UI::WindowsAndMessaging::*;
    const VK_MENU: usize = 0x12;
    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, data: usize) -> LRESULT {
        let repeat = lparam.0 & (1 << 30) != 0;
        match msg {
            WM_SYSKEYDOWN if wparam.0 == VK_MENU => {
                if !repeat {
                    ALT_ALONE.set(true);
                }
            }
            WM_SYSKEYUP if wparam.0 == VK_MENU => ALT_TAP.set(ALT_ALONE.replace(false)),
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_KILLFOCUS => {
                ALT_ALONE.set(false);
                ALT_TAP.set(false);
            }
            // Before the default handling: a title bar press enters the modal move loop there.
            WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN | WM_NCMBUTTONDOWN => {
                ALT_ALONE.set(false);
                unsafe { (*(data as *const Box<dyn Fn()>))() };
            }
            _ => {}
        }
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
    let data = Box::into_raw(Box::new(on_click)) as usize;
    let _ = unsafe { SetWindowSubclass(hwnd, Some(proc), 1, data) };
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
    let dir = wide(&dir).ok()?;
    let stop = Arc::new(Event(unsafe { CreateEventW(None, true, false, None) }.ok()?));
    let thread_stop = stop.clone();
    std::thread::spawn(move || {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            // Attribute edits (e.g. in the Explorer properties dialog) touch no other field.
            | FILE_NOTIFY_CHANGE_ATTRIBUTES;
        let Ok(change) =
            (unsafe { FindFirstChangeNotificationW(PCWSTR(dir.as_ptr()), false, filter) })
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
        ])
        .unwrap();
        let wide: Vec<u16> = bytes[20..]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let text = String::from_utf16(&wide).unwrap();
        assert_eq!(text, "C:\\demo\\日本語_😀.txt\0\\\\server\\share\\a.txt\0\0");
    }
    /// Audit #6: a CF_HDROP block from another process is checked before DragQueryFileW.
    #[test]
    fn hdrop_blocks_are_checked() {
        let good = hdrop_bytes(&[PathBuf::from(r"C:\a.txt"), PathBuf::from(r"C:\b")]).unwrap();
        assert!(hdrop_valid(&good));
        assert!(hdrop_valid(&hdrop_bytes(&[]).unwrap()));
        assert!(!hdrop_valid(&good[..good.len() - 2]), "no double NUL");
        assert!(!hdrop_valid(&good[..19]), "short header");
        let mut beyond = good.clone();
        beyond[..4].copy_from_slice(&(good.len() as u32 + 2).to_le_bytes());
        assert!(!hdrop_valid(&beyond), "list starts past the end");
        let mut inside = good.clone();
        inside[..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(!hdrop_valid(&inside), "list overlaps the header");
        // ANSI lists (fWide = 0) end with two zero bytes.
        let mut ansi = good[..20].to_vec();
        ansi[16..20].copy_from_slice(&0u32.to_le_bytes());
        ansi.extend_from_slice(b"C:\\a.txt\0\0");
        assert!(hdrop_valid(&ansi));
        assert!(!hdrop_valid(&ansi[..ansi.len() - 1]));
    }
    #[test]
    fn os_path_refuses_names_win32_would_change() {
        for ok in [r"\\?\C:\a\b.txt", r"\\?\UNC\server\share\a.b", r"C:\plain\victim.", r"\\?\C:\a\console.txt", r"\\?\C:\COM10"] {
            assert!(os_path(Path::new(ok)).is_ok(), "{ok}");
        }
        assert_eq!(os_path(Path::new(r"\\?\UNC\s\share\a")).unwrap(), Path::new(r"\\s\share\a"));
        for bad in [
            r"\\?\C:\a\victim.",
            r"\\?\C:\a\victim ",
            r"\\?\C:\a.\b.txt",
            r"\\?\C:\a\nul.txt",
            r"\\?\C:\a\Com1",
            r"\\?\C:\a\lpt² .log",
            r"\\?\C:\a\CONOUT$",
            r"\\?\C:\a\..\b",
            r"\\?\UNC\server\share\x.",
        ] {
            let error = os_path(Path::new(bad)).unwrap_err();
            assert!(error.starts_with("Windows の通常のパスでは扱えない名前です"), "{bad}");
        }
        assert!(hdrop_bytes(&[PathBuf::from(r"\\?\C:\a\victim.")]).is_err());
    }
    /// Audit #2: deleting `victim.` must not delete its neighbour `victim`.
    #[test]
    fn trailing_dot_never_reaches_the_neighbour() {
        let root = std::env::temp_dir().join(format!("enyfile-shell-dot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let (victim, dotted) = (root.join("victim"), root.join("victim."));
        std::fs::write(&victim, "keep me").unwrap();
        std::fs::write(&dotted, "junk").unwrap();
        let _ole = Apartment::new().unwrap();
        assert!(item(&dotted).is_err());
        let plan = crate::paste::Plan {
            remove: vec![dotted.clone()],
            remove_ids: vec![crate::pathid::ident(&dotted)],
            ..Default::default()
        };
        assert!(transfer(&plan).unwrap_err().contains("victim."));
        let copy = crate::paste::Plan {
            ops: vec![crate::paste::Op { source: dotted.clone(), dir: root.join("out"), name: None, fresh: true, overwrites: None }],
            ..Default::default()
        };
        assert!(transfer(&copy).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        assert_eq!(std::fs::read_to_string(&dotted).unwrap(), "junk");
        let _ = std::fs::remove_file(&dotted);
        let _ = std::fs::remove_dir_all(&root);
    }
    fn tree(tag: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("enyfile-shell-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, file).unwrap();
        }
        root
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }
    fn replace_plan(root: &Path) -> crate::paste::Plan {
        let mut ask = |_: crate::paste::Question| (crate::paste::Answer::Replace, false);
        crate::paste::plan(&[root.join(r"src\Folder")], &root.join("dst"), false, &mut ask).unwrap().unwrap()
    }
    /// Audit #3: the replaced folder is deleted only after the paste succeeded.
    #[test]
    fn replace_deletes_the_old_folder_last() {
        let root = tree("replace", &[r"src\Folder\new.txt", r"dst\Folder\old.txt"]);
        let _ole = Apartment::new().unwrap();
        let plan = replace_plan(&root);
        assert_eq!(plan.remove, [root.join(r"dst\Folder")]);
        let done = transfer(&plan).unwrap();
        assert!(!done.aborted && done.kept.is_empty());
        assert_eq!(names(&root.join("dst")), ["Folder"]);
        assert_eq!(names(&root.join(r"dst\Folder")), ["new.txt"]);
        assert_eq!(names(&root.join(r"src\Folder")), ["new.txt"]);
        // A paste that fails gives the replaced folder its name back.
        std::fs::write(root.join(r"src\Folder\new.txt"), "newer").unwrap();
        let mut failing = replace_plan(&root);
        failing.ops.push(crate::paste::Op { source: root.join("missing"), dir: root.join("dst"), name: None, fresh: false, overwrites: None });
        assert!(transfer(&failing).is_err());
        assert_eq!(names(&root.join("dst")), ["Folder"]);
        assert_eq!(std::fs::read_to_string(root.join(r"dst\Folder\new.txt")).unwrap(), r"src\Folder\new.txt");
        let _ = std::fs::remove_dir_all(&root);
    }
    /// Audit #1 after #3: Folder\Folder replaces Folder (contents one level up), taken from
    /// where Folder was set aside; a failure puts everything back.
    #[test]
    fn replace_with_a_folder_inside_it() {
        let root = tree("nested", &[r"c\Folder\outer.txt", r"c\Folder\Folder\inner.txt", r"m\Folder\outer.txt", r"m\Folder\Folder\inner.txt"]);
        let _ole = Apartment::new().unwrap();
        let nested_plan = |dir: &str, moved: bool| {
            let source = shell_path(&root.join(dir).join(r"Folder\Folder"));
            let mut ask = |_: crate::paste::Question| (crate::paste::Answer::Replace, false);
            crate::paste::plan(&[source], &root.join(dir), moved, &mut ask).unwrap().unwrap()
        };
        let mut failing = nested_plan("c", false);
        failing.ops.push(crate::paste::Op { source: root.join("missing"), dir: root.join("c"), name: None, fresh: false, overwrites: None });
        assert!(transfer(&failing).is_err());
        assert_eq!(names(&root.join("c")), ["Folder"]);
        assert_eq!(names(&root.join(r"c\Folder")), ["Folder", "outer.txt"]);
        for (dir, moved) in [("c", false), ("m", true)] {
            let done = transfer(&nested_plan(dir, moved)).unwrap();
            assert!(!done.aborted && done.kept.is_empty(), "{dir}");
            assert_eq!(names(&root.join(dir)), ["Folder"], "{dir}");
            assert_eq!(names(&root.join(dir).join("Folder")), ["inner.txt"], "{dir}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
    /// Stopped before the copy engine runs: every replaced folder keeps its name and its
    /// contents, and nothing is pasted.
    #[test]
    fn replace_stopped_early_changes_nothing() {
        let root = tree("stopped", &[r"src\A\new-a.txt", r"src\B\new-b.txt", r"dst\A\old-a.txt", r"dst\B\old-b.txt"]);
        let _ole = Apartment::new().unwrap();
        let plan = || {
            let mut ask = |_: crate::paste::Question| (crate::paste::Answer::Replace, false);
            crate::paste::plan(&[root.join(r"src\A"), root.join(r"src\B")], &root.join("dst"), false, &mut ask).unwrap().unwrap()
        };
        let unchanged = || {
            assert_eq!(names(&root.join("dst")), ["A", "B"]);
            assert_eq!(names(&root.join(r"dst\A")), ["old-a.txt"]);
            assert_eq!(names(&root.join(r"dst\B")), ["old-b.txt"]);
        };
        // B cannot be set aside (a file inside held open): A, already aside, comes back.
        let first = plan();
        assert_eq!(first.remove.len(), 2);
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::File::options().read(true).share_mode(0).open(root.join(r"dst\B\old-b.txt")).unwrap()
        };
        let error = transfer(&first).unwrap_err();
        drop(held);
        assert!(error.contains("退避できません") && !error.contains("残っています"), "{error}");
        unchanged();
        // B swapped for another folder after the user answered: refused before any rename.
        let second = plan();
        std::fs::remove_dir_all(root.join(r"dst\B")).unwrap();
        std::fs::create_dir(root.join(r"dst\B")).unwrap();
        std::fs::write(root.join(r"dst\B\old-b.txt"), "someone else's").unwrap();
        assert!(transfer(&second).unwrap_err().contains("確認後に変わった"));
        unchanged();
        assert_eq!(std::fs::read_to_string(root.join(r"dst\B\old-b.txt")).unwrap(), "someone else's");
        let _ = std::fs::remove_dir_all(&root);
    }
    /// An item the copy engine fails on is noticed even when the operation as a whole is not
    /// aborted (here no dialog, so the test does not wait for anyone to press Skip).
    #[test]
    fn missed_items_are_noticed() {
        let root = tree("missed", &[r"src\locked.txt", r"src\free.txt"]);
        std::fs::create_dir(root.join("dst")).unwrap();
        let _ole = Apartment::new().unwrap();
        let held = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::File::options().read(true).share_mode(0).open(root.join(r"src\locked.txt")).unwrap()
        };
        let missed = Arc::new(AtomicBool::new(false));
        let sink: IFileOperationProgressSink = Missed(missed.clone(), None).into();
        let op = operation(FILEOPERATION_FLAGS(FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0)).unwrap();
        unsafe {
            op.Advise(&sink).unwrap();
            op.CopyItem(&item(&root.join("src")).unwrap(), &item(&root.join("dst")).unwrap(), PCWSTR::null(), None).unwrap();
            let _ = op.PerformOperations();
        }
        drop(held);
        assert!(missed.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(&root);
    }
    /// Past MAX_PATH the copy engine fails partway: a move within the volume is one rename
    /// here, anything else is refused before a single item moves.
    #[test]
    fn beyond_max_path() {
        let deep = ["long-path-segment-1234567890"; 10].join("\\");
        let root = tree("long", &[&format!(r"src\Deep\{deep}\日本語_😀.txt"), r"src\Deep\top.txt", r"src\short.txt"]);
        std::fs::create_dir(root.join("dst")).unwrap();
        let _ole = Apartment::new().unwrap();
        let plan = |moved: bool| {
            let mut ask = |_: crate::paste::Question| unreachable!();
            crate::paste::plan(&[root.join(r"src\Deep"), root.join(r"src\short.txt")], &root.join("dst"), moved, &mut ask).unwrap().unwrap()
        };
        let copy = plan(false);
        assert!(beyond_shell(&copy, &[None, None]).unwrap_err().contains("中止"));
        assert!(transfer(&copy).is_err());
        assert!(names(&root.join("dst")).is_empty());
        let moved = plan(true);
        assert_eq!(beyond_shell(&moved, &[None, None]).unwrap(), [true, false]);
        let done = transfer(&moved).unwrap();
        assert!(!done.aborted && !done.missed);
        assert_eq!(names(&root.join("src")), Vec::<String>::new());
        assert_eq!(names(&root.join("dst")), ["Deep", "short.txt"]);
        let file = root.join(format!(r"dst\Deep\{deep}\日本語_😀.txt"));
        assert!(file.as_os_str().len() > SHELL_MAX && std::fs::read_to_string(&file).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn verbatim_forms() {
        assert_eq!(verbatim(Path::new(r"C:\a\b")), Path::new(r"\\?\C:\a\b"));
        assert_eq!(verbatim(Path::new(r"\\server\share\a")), Path::new(r"\\?\UNC\server\share\a"));
        assert_eq!(verbatim(Path::new(r"\\?\C:\a.")), Path::new(r"\\?\C:\a."));
    }
    #[test]
    fn set_aside_and_restore() {
        let root = tree("aside", &[r"A\a.txt", r"A~enyfile-old\taken.txt", r"B\b.txt"]);
        let (a, b) = (root.join("A"), root.join("B"));
        // A taken temporary name moves on to the next one.
        let aside = set_aside(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(aside[0].1, root.join("A~enyfile-old-2"));
        assert_eq!(names(&root), ["A~enyfile-old", "A~enyfile-old-2", "B~enyfile-old"]);
        // Once the new item holds the name, the old one stays aside: neither is lost.
        std::fs::create_dir(&b).unwrap();
        assert_eq!(restore(&aside), [root.join("B~enyfile-old")]);
        assert_eq!(names(&root), ["A", "A~enyfile-old", "B", "B~enyfile-old"]);
        // A failure partway puts the earlier ones back.
        let error = set_aside(&[a.clone(), root.join("missing")]).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        assert_eq!(names(&root), ["A", "A~enyfile-old", "B", "B~enyfile-old"]);
        assert_eq!(names(&a), ["a.txt"]);
        // A file held open inside for a moment (a scanner) is waited out; held for good, the
        // set-aside fails and nothing is renamed.
        let hold = || {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::File::options().read(true).share_mode(0).open(a.join("a.txt")).unwrap()
        };
        let held = hold();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let aside = set_aside(&[a.clone()]).unwrap();
        release.join().unwrap();
        assert!(restore(&aside).is_empty());
        let held = hold();
        assert!(set_aside(&[a.clone()]).is_err());
        drop(held);
        assert_eq!(names(&a), ["a.txt"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
