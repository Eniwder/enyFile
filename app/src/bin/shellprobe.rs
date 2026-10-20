//! Test stand-in for Explorer, used only by the smoke test.
//!   paste <folder>             invoke the Shell's own "paste" verb on a folder
//!   copy|cut <file> <ready>    put the Shell's own data object on the clipboard, write <ready>,
//!                              keep it alive; `cut` exits 0 once the target reports Paste Succeeded
use std::{
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        System::{Com::*, DataExchange::{GetClipboardSequenceNumber, RegisterClipboardFormatW}, Memory::*, Ole::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{PCSTR, PCWSTR, w},
};

fn pump(until: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < until {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
fn wide(path: &PathBuf) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
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
fn dword(data: &IDataObject, cf: u32) -> Option<u32> {
    unsafe {
        let mut medium = data.GetData(&etc(cf)).ok()?;
        let ptr = GlobalLock(medium.u.hGlobal);
        let value = (!ptr.is_null()).then(|| *ptr.cast::<u32>());
        let _ = GlobalUnlock(medium.u.hGlobal);
        ReleaseStgMedium(&mut medium);
        value
    }
}

fn main() -> windows::core::Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    unsafe { OleInitialize(None)? };
    let mode = args[0].to_string_lossy().into_owned();
    let code = unsafe {
        match mode.as_str() {
            "paste" => {
                let folder: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(&args[1]).as_ptr()), None)?;
                let menu: IContextMenu = folder.BindToHandler(None, &BHID_SFUIObject)?;
                let popup = CreatePopupMenu()?;
                menu.QueryContextMenu(popup, 0, 1, 0x7fff, CMF_NORMAL).ok()?;
                let info = CMINVOKECOMMANDINFO {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                    fMask: SEE_MASK_FLAG_NO_UI, // same bit as CMIC_MASK_FLAG_NO_UI
                    lpVerb: PCSTR(b"paste\0".as_ptr()),
                    nShow: SW_SHOWNORMAL.0,
                    ..Default::default()
                };
                menu.InvokeCommand(&info)?;
                let _ = DestroyMenu(popup);
                // The Shell may finish the copy asynchronously on this STA.
                pump(Duration::from_secs(3), || false);
                0
            }
            "copy" | "cut" => {
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(&args[1]).as_ptr()), None)?;
                let data: IDataObject = item.BindToHandler(None, &BHID_DataObject)?;
                let effect: u32 = if mode == "cut" { 2 } else { 5 };
                let h = GlobalAlloc(GMEM_MOVEABLE, 4)?;
                *GlobalLock(h).cast::<u32>() = effect;
                let _ = GlobalUnlock(h);
                let medium = STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: h },
                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                };
                data.SetData(&etc(RegisterClipboardFormatW(w!("Preferred DropEffect"))), &medium, true)?;
                OleSetClipboard(&data)?;
                let sequence = GetClipboardSequenceNumber();
                std::fs::write(&args[2], b"ready").unwrap();
                let succeeded = RegisterClipboardFormatW(w!("Paste Succeeded"));
                let reported = pump(Duration::from_secs(30), || dword(&data, succeeded) == Some(2));
                // Let the target finish its own clipboard handling before we exit.
                pump(Duration::from_millis(1500), || false);
                if GetClipboardSequenceNumber() == sequence {
                    let _ = OleFlushClipboard();
                }
                if reported { 0 } else { 3 }
            }
            _ => 2,
        }
    };
    unsafe { OleUninitialize() };
    std::process::exit(code);
}
