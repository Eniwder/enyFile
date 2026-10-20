//! The Explorer context menu (right click). It runs on its own STA thread with a hidden
//! window, so the menu's modal loop and Shell extensions never re-enter Slint's event loop.
//! The thread lives as long as the app: Shell verbs such as "Copy" may leave a clipboard
//! object on it, and dialogs they open keep running there.
use std::{cell::RefCell, path::PathBuf, sync::mpsc};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::CoTaskMemFree, LibraryLoader::GetModuleHandleW},
        UI::{
            Shell::{Common::ITEMIDLIST, *},
            WindowsAndMessaging::*,
        },
    },
    core::{Interface, PCSTR, PCWSTR, PSTR, w},
};

pub enum Target {
    /// Selected files and folders (all in one folder).
    Items(Vec<PathBuf>),
    /// The folder background: New, Paste, Properties, ...
    Background(PathBuf),
}
pub struct Request {
    pub target: Target,
    /// Main window, to give activation back after the menu.
    pub owner: isize,
    /// Shift held: extended verbs, as in Explorer.
    pub extended: bool,
}
pub enum Outcome {
    /// Verbs the app handles itself, so its own conflict and rename dialogs are used.
    Paste,
    Rename,
    Done,
    Failed(String),
}

thread_local! {
    /// The menu being shown; owner-drawn submenus ("Send to", "Open with") need its messages.
    static CURRENT: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

pub struct Menus {
    tx: mpsc::Sender<Request>,
    hwnd: isize,
}
impl Menus {
    pub fn start(reply: impl Fn(Outcome) + Send + 'static) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Request>();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || unsafe {
            let _ole = crate::shell::Apartment::new();
            let Ok(instance) = GetModuleHandleW(None) else {
                let _ = ready_tx.send(None);
                return;
            };
            let class = w!("enyFileMenuHost");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class,
                w!(""),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            );
            let Ok(hwnd) = hwnd else {
                let _ = ready_tx.send(None);
                return;
            };
            let _ = ready_tx.send(Some(hwnd.0 as isize));
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_APP {
                    while let Ok(request) = rx.try_recv() {
                        reply(show(hwnd, request));
                    }
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        let hwnd = ready_rx.recv().ok().flatten()?;
        Some(Self { tx, hwnd })
    }
    pub fn show(&self, request: Request) {
        if self.tx.send(request).is_ok() {
            let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_APP, WPARAM(0), LPARAM(0)) };
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        let handled = CURRENT.with(|current| {
            let current = current.borrow();
            let menu = current.as_ref()?;
            if let Ok(menu) = menu.cast::<IContextMenu3>() {
                let mut result = LRESULT(0);
                unsafe { menu.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }
                    .ok()
                    .map(|_| result)
            } else if let Ok(menu) = menu.cast::<IContextMenu2>() {
                unsafe { menu.HandleMenuMsg(msg, wparam, lparam) }.ok().map(|_| LRESULT(0))
            } else {
                None
            }
        });
        if let Some(result) = handled {
            return result;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn wide(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    crate::shell::shell_path(path).as_os_str().encode_wide().chain(Some(0)).collect()
}
unsafe fn items_menu(paths: &[PathBuf]) -> windows::core::Result<IContextMenu> {
    unsafe {
        let mut pidls: Vec<*const ITEMIDLIST> = Vec::new();
        let mut failed = None;
        for path in paths {
            let mut pidl = std::ptr::null_mut();
            match SHParseDisplayName(PCWSTR(wide(path).as_ptr()), None, &mut pidl, 0, None) {
                Ok(()) => pidls.push(pidl),
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        let array = match failed {
            None => SHCreateShellItemArrayFromIDLists(&pidls),
            Some(e) => Err(e),
        };
        for pidl in pidls {
            CoTaskMemFree(Some(pidl as *const _));
        }
        array?.BindToHandler(None, &BHID_SFUIObject)
    }
}
unsafe fn background_menu(dir: &std::path::Path, hwnd: HWND) -> windows::core::Result<IContextMenu> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(dir).as_ptr()), None)?;
        let folder: IShellFolder = item.BindToHandler(None, &BHID_SFObject)?;
        folder.CreateViewObject(hwnd)
    }
}
fn verb(menu: &IContextMenu, id: usize) -> String {
    let mut buffer = [0u16; 128];
    let ok = unsafe {
        menu.GetCommandString(id, GCS_VERBW, None, PSTR(buffer.as_mut_ptr().cast()), buffer.len() as u32)
    };
    if ok.is_err() {
        return String::new();
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf16_lossy(&buffer[..len]).to_lowercase()
}

fn show(hwnd: HWND, request: Request) -> Outcome {
    const FIRST: u32 = 1;
    unsafe {
        let menu = match &request.target {
            Target::Items(paths) => items_menu(paths),
            Target::Background(dir) => background_menu(dir, hwnd),
        };
        let menu = match menu {
            Ok(menu) => menu,
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let Ok(popup) = CreatePopupMenu() else {
            return Outcome::Failed("CreatePopupMenu".into());
        };
        let mut flags = CMF_NORMAL | CMF_CANRENAME;
        if request.extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        if menu.QueryContextMenu(popup, 0, FIRST, 0x7FFF, flags).is_err() {
            let _ = DestroyMenu(popup);
            return Outcome::Failed("QueryContextMenu".into());
        }
        CURRENT.with(|c| *c.borrow_mut() = Some(menu.clone()));
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // A menu only closes on outside clicks when its owner is the foreground window.
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenuEx(popup, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, point.x, point.y, hwnd, None).0 as u32;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let outcome = if command < FIRST {
            Outcome::Done
        } else {
            let id = (command - FIRST) as usize;
            match verb(&menu, id).as_str() {
                "paste" => Outcome::Paste,
                "rename" => Outcome::Rename,
                _ => {
                    let info = CMINVOKECOMMANDINFOEX {
                        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                        fMask: 0x4000 /* CMIC_MASK_UNICODE */ | CMIC_MASK_PTINVOKE,
                        hwnd,
                        lpVerb: PCSTR(id as *const u8),
                        lpVerbW: PCWSTR(id as *const u16),
                        nShow: SW_SHOWNORMAL.0,
                        ptInvoke: point,
                        ..Default::default()
                    };
                    match menu.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) {
                        Ok(()) => Outcome::Done,
                        Err(e) => Outcome::Failed(e.to_string()),
                    }
                }
            }
        };
        CURRENT.with(|c| *c.borrow_mut() = None);
        let _ = DestroyMenu(popup);
        let _ = SetForegroundWindow(HWND(request.owner as *mut _));
        outcome
    }
}
