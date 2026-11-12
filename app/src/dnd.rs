//! OLE adapter for the Slint window. The folder's own Shell target owns the transfer,
//! including source notification, conflict UI, cancellation and partial completion.
//! All interfaces and callbacks stay on the window's OLE STA. Never hold a Controller
//! or RefCell borrow while calling Shell COM: it may pump messages and reenter us.
mod smoke;
#[cfg(test)]
mod tests;
pub(super) use smoke::{smoke_modal, refresh as smoke_refresh};

use super::*;
use windows::{
    core::{implement, Ref, Result as WinResult},
    Win32::{
        Foundation::{POINT, POINTL, E_POINTER, HINSTANCE, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::ScreenToClient,
        System::{Com::IDataObject, Ole::*, SystemServices::MODIFIERKEYS_FLAGS},
        UI::{Shell::{IShellItem, SHCreateItemFromParsingName, BHID_SFUIObject}, WindowsAndMessaging::*},
    },
};
use std::os::windows::ffi::OsStrExt;

thread_local! { static ORIGIN: Cell<Option<usize>> = const { Cell::new(None) }; }
thread_local! { static DROPPING: Cell<bool> = const { Cell::new(false) }; }
pub(super) fn is_dragging() -> bool { ORIGIN.get().is_some() || WHEEL_ROUTE.get().is_some() || DROPPING.get() }
struct NativeDrop(bool);
impl Drop for NativeDrop {
    fn drop(&mut self) { DROPPING.set(self.0); }
}
// Wheel input can otherwise be consumed by the source application's OLE drag loop.
// The low-level callback only posts a message; UI/COM work happens outside the hook.
pub(super) const WHEEL_MESSAGE: u32 = WM_APP + 0x431;
pub(super) const TASK_MESSAGE: u32 = WM_APP + 0x432;
pub(super) const REDRAW_MESSAGE: u32 = WM_APP + 0x433;
thread_local! {
    static WHEEL_ROUTE: Cell<Option<(HWND, usize)>> = const { Cell::new(None) };
    static WHEEL_SERIAL: Cell<usize> = const { Cell::new(0) };
    static WHEEL_REMAINDER: Cell<(i32, i32)> = const { Cell::new((-1, 0)) };
    static TASKS: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::new(VecDeque::new());
    static REDRAW_PENDING: Cell<bool> = const { Cell::new(false) };
}

/// Run outside winit's application callback. Entering OLE from a Slint timer keeps
/// that callback borrowed, so winit cannot deliver RedrawRequested until drag ends.
pub(super) fn defer(hwnd: HWND, task: impl FnOnce() + 'static) -> WinResult<()> {
    TASKS.with(|tasks| tasks.borrow_mut().push_back(Box::new(task)));
    if let Err(error) = unsafe { PostMessageW(Some(hwnd), TASK_MESSAGE, WPARAM(0), LPARAM(0)) } {
        TASKS.with(|tasks| tasks.borrow_mut().pop_back());
        return Err(error);
    }
    Ok(())
}
pub(super) fn run_deferred() {
    let task = TASKS.with(|tasks| tasks.borrow_mut().pop_front());
    if let Some(task) = task { task(); }
}

fn request_frame(ui: &App) {
    let Some(hwnd) = hwnd(ui) else { return };
    if !REDRAW_PENDING.replace(true) {
        if unsafe { PostMessageW(Some(hwnd), REDRAW_MESSAGE, WPARAM(0), LPARAM(0)) }.is_err() {
            REDRAW_PENDING.set(false);
        }
    }
}
/// Bypass Slint's timer-based frame throttle during OLE's modal message loop.
/// Called from our native window message, with no Controller/state borrow held.
pub(super) fn redraw(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_INVALIDATE, RDW_UPDATENOW, RDW_NOERASE};
    REDRAW_PENDING.set(false);
    let _ = unsafe { RedrawWindow(Some(hwnd), None, None, RDW_INVALIDATE | RDW_UPDATENOW | RDW_NOERASE) };
}
fn feedback(ui: &App, pane: i32, row: i32) {
    if ui.get_drop_pane() != pane || ui.get_drop_row() != row {
        ui.set_drop_pane(pane);
        ui.set_drop_row(row);
        request_frame(ui);
    }
}
struct WheelHook { hook: HHOOK, serial: usize }
impl WheelHook {
    fn new(hwnd: HWND) -> WinResult<Self> {
        unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            if code >= 0 && wparam.0 == WM_MOUSEWHEEL as usize && lparam.0 != 0 {
                // Windows owns this structure for the duration of this callback.
                let mouse = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
                if let Some((hwnd, serial)) = WHEEL_ROUTE.get() {
                    if unsafe { GetAncestor(WindowFromPoint(mouse.pt), GA_ROOT) } == hwnd {
                        let delta = (mouse.mouseData >> 16) as u16;
                        let position = (mouse.pt.x as u16 as u32) | ((mouse.pt.y as u16 as u32) << 16);
                        if unsafe { PostMessageW(Some(hwnd), WHEEL_MESSAGE, WPARAM((serial << 16) | delta as usize), LPARAM(position as isize)) }.is_ok() {
                            return LRESULT(1); // Prevent duplicate Slint/source wheel handling.
                        }
                    }
                }
            }
            unsafe { CallNextHookEx(None, code, wparam, lparam) }
        }
        let module = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)? };
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook), Some(HINSTANCE(module.0)), 0)? };
        let serial = WHEEL_SERIAL.get().wrapping_add(1) & (usize::MAX >> 16);
        WHEEL_SERIAL.set(serial);
        WHEEL_ROUTE.set(Some((hwnd, serial)));
        WHEEL_REMAINDER.set((-1, 0));
        Ok(Self { hook, serial })
    }
}
impl Drop for WheelHook {
    fn drop(&mut self) {
        if WHEEL_ROUTE.get().is_some_and(|(_, serial)| serial == self.serial) {
            WHEEL_ROUTE.set(None);
            WHEEL_REMAINDER.set((-1, 0));
        }
        let _ = unsafe { UnhookWindowsHookEx(self.hook) };
    }
}

/// Window messages are session-tagged so queued input cannot affect a later drag.
pub(super) fn wheel(ui: &App, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    if WHEEL_ROUTE.get() != Some((hwnd, wparam.0 >> 16)) { return; }
    let mut point = POINT { x: lparam.0 as u16 as i16 as i32, y: ((lparam.0 as u32 >> 16) as u16 as i16) as i32 };
    if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() { return; }
    if ui.get_settings_open() || ui.get_name_open() || ui.get_props_open() || ui.get_ask_open()
        || ui.get_memory_open() || ui.get_open_menu() >= 0 || ui.get_bookmarks_open() { return; }
    let scale = ui.window().scale_factor();
    let pane = ui.invoke_hit_drop(point.x as f32 / scale, point.y as f32 / scale).pane;
    if pane < 0 { WHEEL_REMAINDER.set((-1, 0)); return; }
    let (previous, remainder) = WHEEL_REMAINDER.get();
    let delta = wparam.0 as u16 as i16 as i32;
    let total = if previous == pane { remainder + delta } else { delta };
    WHEEL_REMAINDER.set((pane, total % 120));
    if total / 120 == 0 { return; }
    CONTROLLER.with(|cell| {
        let Some(controller) = cell.borrow().clone() else { return };
        let Ok(mut c) = controller.try_borrow_mut() else { return };
        // Clear stale row feedback immediately. OLE's next DragOver (and Drop itself)
        // resolves the newly visible row before allowing any operation.
        feedback(ui, -1, -1);
        for _ in 0..(total / 120).unsigned_abs() { c.page_step(pane as usize, total < 0); }
    });
    request_frame(ui);
}
pub struct Origin(Option<usize>);
impl Origin {
    pub fn new(pane: usize) -> Self { Self(ORIGIN.replace(Some(pane))) }
}
impl Drop for Origin {
    fn drop(&mut self) { ORIGIN.set(self.0); }
}

pub struct Registration(HWND);
impl Drop for Registration {
    fn drop(&mut self) {
        // Released before the main OLE apartment; Revoke releases Windows' COM ref.
        let _ = unsafe { RevokeDragDrop(self.0) };
    }
}
pub fn register(hwnd: HWND, ui: slint::Weak<App>) -> WinResult<Registration> {
    let target: IDropTarget = Target { hwnd, ui, state: RefCell::new(State::default()) }.into();
    unsafe { RegisterDragDrop(hwnd, &target)?; }
    Ok(Registration(hwnd))
}

#[derive(Default)]
struct State {
    wheel: Option<WheelHook>,
    move_only: bool,
    buttons: u32,
    data: Option<IDataObject>,
    // Immutable source batch: cloning a hover snapshot must not copy every path.
    paths: Rc<[PathBuf]>,
    destination: Option<PathBuf>,
    target: Option<IDropTarget>,
}
#[implement(IDropTarget)]
struct Target {
    hwnd: HWND,
    ui: slint::Weak<App>,
    state: RefCell<State>,
}

/// Reject the entire batch rather than unexpectedly transferring just some entries.
fn valid_destination(paths: &[PathBuf], dir: &Path) -> bool {
    if paths.is_empty() || !dir.is_dir() { return false; }
    let mut ids = pathid::Idents::default();
    paths.iter().all(|source| {
        source.exists()
            && !source.parent().is_some_and(|parent| ids.same(parent, dir))
            && !ids.within(dir, source)
    })
}

fn accepts_origin(origin: Option<usize>, enabled: bool, pane: usize) -> bool {
    origin.is_none_or(|source| enabled && source != pane)
}

fn folder_target(dir: &Path) -> WinResult<IDropTarget> {
    let path = shell::os_path(dir).map_err(|_| windows::core::Error::from(E_POINTER))?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // The nul-terminated buffer lives through parsing; the returned COM interfaces
    // own their references and never retain the buffer.
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(windows::core::PCWSTR(wide.as_ptr()), None)?;
        item.BindToHandler(None, &BHID_SFUIObject)
    }
}

impl Target {
    fn highlight(&self, pane: i32, row: i32) {
        if let Some(ui) = self.ui.upgrade() {
            feedback(&ui, pane, row);
        }
    }
    fn leave_folder(&self) {
        let target = {
            let mut state = self.state.borrow_mut();
            state.destination = None;
            state.target.take()
        };
        if let Some(target) = target { let _ = unsafe { target.DragLeave() }; }
        self.highlight(-1, -1);
    }
    /// End the whole session, unlike leaving one folder while still in this window.
    /// Detach first: releasing COM references and unhooking may reenter the window.
    fn end_drag(&self) {
        let mut state = std::mem::take(&mut *self.state.borrow_mut());
        drop(state.wheel.take());
        if let Some(target) = state.target.take() { let _ = unsafe { target.DragLeave() }; }
        self.highlight(-1, -1);
    }
    fn update_drag(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT, final_check: bool) -> WinResult<()> {
        let result = self.over(keys, pt, effect, final_check);
        if result.is_err() { self.end_drag(); }
        result
    }
    fn destination(&self, pt: &POINTL) -> Option<(PathBuf, i32, i32)> {
        let ui = self.ui.upgrade()?;
        if ui.get_settings_open() || ui.get_name_open() || ui.get_props_open()
            || ui.get_ask_open() || ui.get_memory_open() || ui.get_open_menu() >= 0
            || ui.get_bookmarks_open() { return None; }
        let mut point = POINT { x: pt.x, y: pt.y };
        if !unsafe { ScreenToClient(self.hwnd, &mut point) }.as_bool() { return None; }
        let scale = ui.window().scale_factor();
        let hit = ui.invoke_hit_drop(point.x as f32 / scale, point.y as f32 / scale);
        let p = usize::try_from(hit.pane).ok()?;
        CONTROLLER.with(|cell| {
            let controller = cell.borrow().clone()?;
            let c = controller.try_borrow().ok()?;
            if p >= c.panes.len() || c.operations != 0 { return None; }
            let same_pane = ORIGIN.get() == Some(p);
            if !same_pane && !accepts_origin(ORIGIN.get(), c.config.pane_drag_drop, p) { return None; }
            let pane = &c.panes[p];
            // Search and comparison rows can name other folders; reject their entire pane.
            if pane.found.is_some() || pane.model.compare.borrow().is_some() { return None; }
            let data = c.data(p);
            if data.dir.as_os_str().is_empty() { return None; }
            // A partial row at the bottom is blank space, not the next page's item.
            let slot = usize::try_from(hit.row).ok()
                .filter(|&row| row < pane.model.row_count())
                .and_then(|row| pane.model.slot(pane.model.page_start.get() + row));
            let mut target_row = hit.row;
            let dir = match slot {
                Some(Slot::Parent) if hit.in_name => data.dir.parent()?.to_path_buf(),
                Some(Slot::Entry(i)) if hit.in_name && data.rows[i].directory => (*data.rows[i].path).clone(),
                Some(Slot::Drive(_)) | Some(Slot::Gap) => return None,
                _ => { target_row = -1; data.dir.clone() },
            };
            if same_pane && (!c.config.intra_pane_move || target_row < 0) { return None; }
            Some((dir, hit.pane, target_row))
        })
    }
    fn over(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT, final_check: bool) -> WinResult<()> {
        // OLE supplies a valid, writable in/out effect pointer for every callback.
        if effect.is_null() { return Err(E_POINTER.into()); }
        let allowed = unsafe { *effect };
        unsafe { *effect = DROPEFFECT_NONE; }
        let Some((dir, pane, row)) = self.destination(pt) else { self.leave_folder(); return Ok(()); };
        let same_pane = ORIGIN.get() == Some(pane as usize);
        // Respect explicit copy/link intent by refusing it, never silently moving.
        if same_pane && keys.0 & (8 | 32) != 0 { self.leave_folder(); return Ok(()); }
        let keys = if same_pane { MODIFIERKEYS_FLAGS(keys.0 | 4) } else { keys };
        let (paths, data, target, buttons, changed) = {
            let state = self.state.borrow();
            let changed = state.destination.as_ref() != Some(&dir);
            let data = if changed { state.data.clone() } else { None };
            (state.paths.clone(), data, state.target.clone(), state.buttons, changed)
        };
        // Revalidate hover using the initiating button even after release. The Shell
        // may reject a DragOver with no buttons; Drop itself receives the real state.
        let keys = if final_check { MODIFIERKEYS_FLAGS(keys.0 | buttons) } else { keys };
        if (final_check || changed) && !valid_destination(&paths, &dir) { self.leave_folder(); return Ok(()); }
        // This feature supports copy and move only. Explicit link modifiers must not
        // silently fall back to a destructive move.
        let supported = if same_pane { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 };
        let mut chosen = DROPEFFECT(allowed.0 & supported);
        let target = if !changed { target } else {
            self.leave_folder();
            let Some(data) = data else { return Ok(()); };
            let target = folder_target(&dir)?;
            let target = unsafe {
                target.DragEnter(&data, keys, *pt, &mut chosen)?;
                target
            };
            let mut state = self.state.borrow_mut();
            state.destination = Some(dir.clone());
            state.target = Some(target.clone());
            Some(target)
        };
        self.state.borrow_mut().move_only = same_pane;
        if let Some(target) = target {
            unsafe { target.DragOver(keys, *pt, &mut chosen)?; }
            unsafe { *effect = chosen; }
            if chosen != DROPEFFECT_NONE { self.highlight(pane, row); } else { self.highlight(-1, -1); }
        }
        Ok(())
    }
}


impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        self.end_drag();
        let data = data.as_ref().cloned();
        let paths = data.as_ref().and_then(shell::hdrop_paths).unwrap_or_default();
        let wheel = match WheelHook::new(self.hwnd) {
            Ok(wheel) => Some(wheel),
            Err(error) => { recovery::record(format!("drag wheel hook unavailable: {error}")); None }
        };
        *self.state.borrow_mut() = State { data, paths: paths.into(), wheel, buttons: keys.0 & (1 | 2 | 16), ..Default::default() };
        // Unsupported virtual objects fail closed; never promise an operation we cannot validate.
        self.update_drag(keys, pt, effect, false)
    }
    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        self.update_drag(keys, pt, effect, false)
    }
    fn DragLeave(&self) -> WinResult<()> {
        self.end_drag();
        Ok(())
    }
    fn Drop(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        let _transfer = NativeDrop(DROPPING.replace(true));
        let wheel = self.state.borrow_mut().wheel.take();
        drop(wheel); // No interception while Shell transfer/conflict dialogs are open.
        // Recheck position, setting, identities and existence on release.
        self.update_drag(keys, pt, effect, true)?;
        let (target, move_only) = {
            let mut state = self.state.borrow_mut();
            (state.target.take(), state.move_only)
        };
        let keys = if move_only { MODIFIERKEYS_FLAGS(keys.0 | 4) } else { keys };
        self.highlight(-1, -1);
        let delegated = target.is_some() && unsafe { *effect } != DROPEFFECT_NONE;
        let result = if unsafe { *effect } != DROPEFFECT_NONE {
            if let Some(target) = target { unsafe { target.Drop(data.as_ref(), keys, *pt, effect) } } else { Ok(()) }
        } else {
            if let Some(target) = target { let _ = unsafe { target.DragLeave() }; }
            Ok(())
        };
        self.end_drag();
        if let Err(error) = &result {
            unsafe { *effect = DROPEFFECT_NONE; }
            if let Some(ui) = self.ui.upgrade() {
                ui.set_message(format!("ドロップできません: {error}").into());
                ui.set_message_error(true);
            }
        }
        // A handed-off operation can partially succeed even on error/cancel. Reuse
        // the controller's refresh policy, preserving searches and pending listings.
        if delegated {
            slint::Timer::single_shot(Duration::ZERO, || {
                CONTROLLER.with(|cell| {
                    let controller = cell.borrow().clone();
                    if let Some(c) = controller { c.borrow_mut().refresh_shown(None); }
                });
            });
        }
        result
    }
}
