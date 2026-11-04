//! OLE adapter for the Slint window. The folder's own Shell target owns the transfer,
//! including source notification, conflict UI, cancellation and partial completion.
//! All interfaces and callbacks stay on the window's OLE STA. Never hold a Controller
//! or RefCell borrow while calling Shell COM: it may pump messages and reenter us.
use super::*;
use windows::{
    core::{implement, Ref, Result as WinResult},
    Win32::{
        Foundation::{POINT, POINTL, E_POINTER, HINSTANCE, LPARAM, LRESULT, WPARAM, DRAGDROP_S_CANCEL, DRAGDROP_S_USEDEFAULTCURSORS},
        Graphics::Gdi::ScreenToClient,
        System::{Com::IDataObject, Ole::*, SystemServices::MODIFIERKEYS_FLAGS},
        UI::{Shell::{IShellItem, SHCreateItemFromParsingName, BHID_SFUIObject}, WindowsAndMessaging::*},
    },
};
use std::os::windows::ffi::OsStrExt;

thread_local! { static ORIGIN: Cell<Option<usize>> = const { Cell::new(None) }; }
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

// This test source always cancels on the first QueryContinueDrag. It cannot drop on
// the user's desktop, but runs our checks inside a real OLE drag, outside winit's
// application callback, using exactly the same native task entry as drag_out.
#[implement(IDropSource)]
struct ModalProbe {
    ui: slint::Weak<App>,
    controller: Rc<RefCell<Controller>>,
    root: PathBuf,
    result: Rc<RefCell<Option<Result<(), String>>>>,
}
impl IDropSource_Impl for ModalProbe_Impl {
    fn QueryContinueDrag(&self, _: windows::core::BOOL, _: MODIFIERKEYS_FLAGS) -> windows::core::HRESULT {
        let _ = std::fs::write(self.root.join("dnd-modal-stage"), "checking");
        let checked = self.ui.upgrade().ok_or("window closed".to_string())
            .and_then(|ui| smoke_check(&ui, &self.controller, &self.root));
        *self.result.borrow_mut() = Some(checked);
        DRAGDROP_S_CANCEL
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> windows::core::HRESULT { DRAGDROP_S_USEDEFAULTCURSORS }
}
pub(super) fn smoke_modal(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path) -> Result<(), String> {
    use windows::Win32::UI::Shell::BHID_DataObject;
    let _ = std::fs::write(root.join("dnd-modal-stage"), "creating data");
    let data: IDataObject = unsafe { shell::item_array(&[root.join("left")])?.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.to_string())?;
    let result = Rc::new(RefCell::new(None));
    let source: IDropSource = ModalProbe { ui: ui.as_weak(), controller: c.clone(), root: root.to_path_buf(), result: result.clone() }.into();
    let mut effect = DROPEFFECT_NONE;
    let _ = std::fs::write(root.join("dnd-modal-stage"), "entering OLE");
    // Wake OLE's input pump without moving the pointer or injecting system input.
    unsafe { PostThreadMessageW(windows::Win32::System::Threading::GetCurrentThreadId(), WM_MOUSEMOVE, WPARAM(0), LPARAM(0)) }.map_err(|e| e.to_string())?;
    let status = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    let _ = std::fs::write(root.join("dnd-modal-stage"), "cancelled");
    if status != DRAGDROP_S_CANCEL { return Err(format!("modal probe did not cancel: {status:?}")); }
    let checked = result.borrow_mut().take().ok_or("modal probe did not run")?;
    checked
}

/// Called only by --smoke, against its dedicated fixture. Exercise the actual OLE
/// adapter and Slint hit testing without releasing a drop onto the user's desktop.
pub(super) fn smoke_check(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path) -> std::result::Result<(), String> {
    use windows::Win32::{Graphics::Gdi::ClientToScreen, UI::Shell::BHID_DataObject};
    let check = || -> std::result::Result<(), String> {
        let hwnd = hwnd(ui).ok_or("no HWND")?;
        let frames = Rc::new(Cell::new(0usize));
        let rendered = frames.clone();
        ui.window().set_rendering_notifier(move |state, _| {
            if matches!(state, slint::RenderingState::AfterRendering) { rendered.set(rendered.get() + 1); }
        }).map_err(|e| e.to_string())?;
        let present = || -> Result<(), String> {
            let before = frames.get();
            // Deliver the same native paint message the OLE loop dispatches. A
            // snapshot alone would mask the missing on-screen rendering regression.
            unsafe { SendMessageW(hwnd, REDRAW_MESSAGE, Some(WPARAM(0)), Some(LPARAM(0))) };
            if frames.get() == before { return Err("OLE modal loop did not render the window".into()); }
            Ok(())
        };
        let source = root.join("dnd-probe");
        std::fs::create_dir_all(&source).map_err(|e| e.to_string())?;
        let file = source.join("probe.txt");
        std::fs::write(&file, b"drag probe").map_err(|e| e.to_string())?;
        let data: IDataObject = unsafe { shell::item_array(&[file.clone()])?.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.to_string())?;
        let target: IDropTarget = Target { hwnd, ui: ui.as_weak(), state: RefCell::new(State::default()) }.into();
        let scale = ui.window().scale_factor();
        let point_for = |pane| -> std::result::Result<POINTL, String> {
            let width = (ui.window().size().width as f32 / scale) as i32;
            for x in (2..width).step_by(4) {
                if ui.invoke_hit_drop(x as f32, 111.0).pane == pane {
                    let mut point = POINT { x: (x as f32 * scale) as i32, y: (111.0 * scale) as i32 };
                    if !unsafe { ClientToScreen(hwnd, &mut point) }.as_bool() { return Err("ClientToScreen failed".into()); }
                    return Ok(POINTL { x: point.x, y: point.y });
                }
            }
            Err(format!("pane {pane} not found by Slint hit test"))
        };
        let left = point_for(0)?;
        let right = point_for(1)?;
        // Check real row geometry: names enter folders; metadata targets the pane.
        // Include both ordinary folders and the parent row, without transferring files.
        let probe = Target { hwnd, ui: ui.as_weak(), state: RefCell::new(State::default()) };
        let mut checked = HashSet::new();
        let mut covered = [false; 4];
        let mut folder_name = None;
        let mut folder_metadata = None;
        let size = ui.window().size();
        'rows: for y in (111..(size.height as f32 / scale) as i32 - 26).step_by(8) {
            for x in (2..(size.width as f32 / scale) as i32).step_by(16) {
                let hit = ui.invoke_hit_drop(x as f32, y as f32);
                if hit.pane < 0 || !checked.insert((hit.pane, hit.row, hit.in_name)) { continue; }
                let (expected, kind) = {
                    let c = c.borrow();
                    let p = hit.pane as usize;
                    let data = c.data(p);
                    let slot = c.panes[p].model.slot(hit.row as usize + c.panes[p].model.page_start.get());
                    let (folder, kind) = match slot {
                        Some(Slot::Parent) => (data.dir.parent().unwrap().to_path_buf(), 0),
                        Some(Slot::Entry(i)) if data.rows[i].directory => ((*data.rows[i].path).clone(), 2),
                        _ => continue,
                    };
                    covered[kind + usize::from(!hit.in_name)] = true;
                    (if hit.in_name { folder } else { data.dir.clone() }, kind)
                };
                let mut point = POINT { x: (x as f32 * scale) as i32, y: (y as f32 * scale) as i32 };
                if !unsafe { ClientToScreen(hwnd, &mut point) }.as_bool() { return Err("ClientToScreen failed".into()); }
                if hit.pane == 1 && kind == 2 {
                    let position = POINTL { x: point.x, y: point.y };
                    if hit.in_name { folder_name = Some(position); } else { folder_metadata = Some(position); }
                }
                let (destination, _, row) = probe.destination(&POINTL { x: point.x, y: point.y }).ok_or("no drop destination")?;
                if destination != expected || row != if hit.in_name { hit.row } else { -1 } {
                    return Err("name and metadata drop destinations or highlights disagree".into());
                }
                if hit.pane == 1 {
                    let enabled = c.borrow().config.pane_drag_drop;
                    c.borrow_mut().config.pane_drag_drop = true;
                    let internal = { let _origin = Origin::new(0); probe.destination(&POINTL { x: point.x, y: point.y }) };
                    c.borrow_mut().config.pane_drag_drop = enabled;
                    if internal != Some((expected, hit.pane, row)) { return Err("internal drop uses different column rules".into()); }
                }
                if covered.iter().all(|v| *v) { break 'rows; }
            }
        }
        if !covered.iter().all(|v| *v) { return Err(format!("folder drop geometry coverage incomplete: {covered:?}")); }
        let enter = |point: POINTL, expected: DROPEFFECT| -> std::result::Result<(), String> {
            let mut effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            unsafe { target.DragEnter(&data, MODIFIERKEYS_FLAGS(9), point, &mut effect) }.map_err(|e| e.to_string())?;
            let highlighted = ui.get_drop_pane();
            if expected != DROPEFFECT_NONE {
                effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
                unsafe { target.DragOver(MODIFIERKEYS_FLAGS(5), point, &mut effect) }.map_err(|e| e.to_string())?;
                if effect != DROPEFFECT_MOVE { return Err("Shift during drag did not select move".into()); }
                effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
                unsafe { target.DragOver(MODIFIERKEYS_FLAGS(9), point, &mut effect) }.map_err(|e| e.to_string())?;
                if effect != DROPEFFECT_COPY { return Err("Ctrl during drag did not select copy".into()); }
                present()?;
                // Inspect rendered pixels as well as hit testing: an overlay without
                // explicit x/y is centered by Slint even when the hit test is correct.
                let shot = ui.window().take_snapshot().map_err(|e| e.to_string())?;
                let mut client = POINT { x: point.x, y: point.y };
                if !unsafe { ScreenToClient(hwnd, &mut client) }.as_bool() { return Err("ScreenToClient failed".into()); }
                let x = client.x as usize + (4.0 * scale).ceil() as usize;
                let y = client.y as usize;
                let accent = ui.global::<Theme>().get_accent();
                let pixel = shot.as_slice().get(y * shot.width() as usize + x).ok_or("highlight outside snapshot")?;
                let rgb: Vec<u8> = shot.as_slice().iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                let mut ppm = format!("P6\n{} {}\n255\n", shot.width(), shot.height()).into_bytes();
                ppm.extend(rgb);
                std::fs::write(root.join("dnd-highlight.ppm"), ppm).map_err(|e| e.to_string())?;
                if [pixel.r, pixel.g, pixel.b] != [accent.red(), accent.green(), accent.blue()] {
                    return Err("drop highlight is not anchored to the row's left edge".into());
                }
            }
            unsafe { target.DragLeave() }.map_err(|e| e.to_string())?;
            if effect != expected { return Err(format!("drop effect {effect:?}, expected {expected:?}")); }
            if (highlighted >= 0) != (expected != DROPEFFECT_NONE) || ui.get_drop_pane() != -1 { return Err("drop highlight not cleared".into()); }
            Ok(())
        };
        c.borrow_mut().config.pane_drag_drop = false;
        enter(right, DROPEFFECT_COPY)?; // External remains enabled.
        {
            let _origin = Origin::new(0);
            enter(right, DROPEFFECT_NONE)?;
            c.borrow_mut().config.pane_drag_drop = true;
            enter(right, DROPEFFECT_COPY)?;
            enter(left, DROPEFFECT_NONE)?;
        }
        // Same-pane moves are independently enabled and never become copies, even
        // when Ctrl is pressed or the source only offers COPY.
        c.borrow_mut().config.intra_pane_move = true;
        c.borrow_mut().config.pane_drag_drop = false;
        {
            let _origin = Origin::new(1);
            let name = folder_name.ok_or("no ordinary folder name target")?;
            let metadata = folder_metadata.ok_or("no ordinary folder metadata target")?;
            let both = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            for (position, keys, allowed, expected) in [
                (name, 1, both, DROPEFFECT_MOVE),
                (name, 5, both, DROPEFFECT_MOVE),
                (name, 9, both, DROPEFFECT_NONE),
                (name, 13, both, DROPEFFECT_NONE),
                (name, 33, both, DROPEFFECT_NONE),
                (name, 1, DROPEFFECT_COPY, DROPEFFECT_NONE),
                (metadata, 1, both, DROPEFFECT_NONE),
            ] {
                let mut effect = allowed;
                unsafe { target.DragEnter(&data, MODIFIERKEYS_FLAGS(keys), position, &mut effect) }.map_err(|e| e.to_string())?;
                if effect != expected { return Err(format!("same-pane effect: {effect:?}, expected {expected:?}")); }
                if expected == DROPEFFECT_MOVE && ui.get_drop_row() < 0 { return Err("same-pane folder move has no highlight".into()); }
                unsafe { target.DragLeave() }.map_err(|e| e.to_string())?;
            }
            c.borrow_mut().config.intra_pane_move = false;
            let mut effect = both;
            unsafe { target.DragEnter(&data, MODIFIERKEYS_FLAGS(1), name, &mut effect) }.map_err(|e| e.to_string())?;
            unsafe { target.DragLeave() }.map_err(|e| e.to_string())?;
            if effect != DROPEFFECT_NONE { return Err("disabled same-pane option accepted a move".into()); }
        }
        c.borrow_mut().config.pane_drag_drop = true;
        // The actual HWND message path used by the short-lived wheel hook. No system
        // input is injected and no user's pointer or unrelated application is touched.
        let saved = {
            let c = c.borrow();
            let selection = c.panes[0].model.selection.borrow().clone();
            (selection, c.panes[0].model.special.get(), c.active)
        };
        for internal in [false, true] {
            let _origin = internal.then(|| Origin::new(1));
            let mut effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            unsafe { target.DragEnter(&data, MODIFIERKEYS_FLAGS(9), left, &mut effect) }.map_err(|e| e.to_string())?;
            let (_, serial) = WHEEL_ROUTE.get().ok_or("wheel capture not installed")?;
            let position = (left.x as u16 as u32) | ((left.y as u16 as u32) << 16);
            let send = |delta: i16, serial: usize| {
                unsafe { SendMessageW(hwnd, WHEEL_MESSAGE, Some(WPARAM((serial << 16) | delta as u16 as usize)), Some(LPARAM(position as isize))) };
            };
            let start = c.borrow().panes[0].model.page_start.get();
            send(-60, serial);
            if c.borrow().panes[0].model.page_start.get() != start { return Err("partial wheel delta turned a page".into()); }
            send(-60, serial);
            if c.borrow().panes[0].model.page_start.get() <= start { return Err("drag wheel did not turn page down".into()); }
            present()?;
            // Re-evaluate the row underneath the stationary pointer after paging.
            effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            unsafe { target.DragOver(MODIFIERKEYS_FLAGS(9), left, &mut effect) }.map_err(|e| e.to_string())?;
            if ui.get_drop_row() != -1 { return Err("paging retained a stale folder-row highlight".into()); }
            send(120, serial);
            if c.borrow().panes[0].model.page_start.get() != start { return Err("drag wheel did not turn page up".into()); }
            unsafe { target.DragLeave() }.map_err(|e| e.to_string())?;
            if WHEEL_ROUTE.get().is_some() { return Err("wheel capture survived DragLeave".into()); }
            send(-120, serial);
            if c.borrow().panes[0].model.page_start.get() != start { return Err("stale drag wheel message was accepted".into()); }
        }
        {
            let mut c = c.borrow_mut();
            *c.panes[0].model.selection.borrow_mut() = saved.0;
            c.panes[0].model.special.set(saved.1);
            c.repaint(0);
            c.active(saved.2);
        }
        if !file.exists() { return Err("DragLeave modified the source".into()); }
        // Exercise Drop itself with dedicated temporary file/folder sources, proving
        // the MOVE-only negotiation does not leave duplicates behind.
        c.borrow_mut().config.intra_pane_move = true;
        {
            let _origin = Origin::new(1);
            let point = folder_name.ok_or("no folder move target")?;
            let (destination, _, _) = probe.destination(&point).ok_or("no same-pane move destination")?;
            if !std::fs::canonicalize(&destination).is_ok_and(|dir| dir.starts_with(root)) {
                return Err("move test destination escaped its temporary fixture".into());
            }
            let moved_file = source.join("same-pane-file.txt");
            let moved_folder = source.join("same-pane-folder");
            std::fs::write(&moved_file, b"move only").map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&moved_folder).map_err(|e| e.to_string())?;
            std::fs::write(moved_folder.join("child.txt"), b"child").map_err(|e| e.to_string())?;
            let move_data: IDataObject = unsafe { shell::item_array(&[moved_file.clone(), moved_folder.clone()])?.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.to_string())?;
            let mut effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            unsafe {
                target.DragEnter(&move_data, MODIFIERKEYS_FLAGS(1), point, &mut effect).map_err(|e| e.to_string())?;
                target.Drop(&move_data, MODIFIERKEYS_FLAGS(0), point, &mut effect).map_err(|e| e.to_string())?;
            }
            let start = Instant::now();
            while (moved_file.exists() || moved_folder.exists()) && start.elapsed() < Duration::from_secs(5) {
                unsafe {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if moved_file.exists() || moved_folder.exists()
                || std::fs::read(destination.join("same-pane-file.txt")).ok().as_deref() != Some(b"move only")
                || std::fs::read(destination.join(r"same-pane-folder\child.txt")).ok().as_deref() != Some(b"child") {
                return Err("same-pane Drop did not move the entire file/folder batch".into());
            }
        }
        Ok(())
    };
    let original = c.borrow().config.pane_drag_drop;
    let original_within = c.borrow().config.intra_pane_move;
    let result = check();
    c.borrow_mut().config.pane_drag_drop = original;
    c.borrow_mut().config.intra_pane_move = original_within;
    result
}

#[derive(Default)]
struct State {
    wheel: Option<WheelHook>,
    move_only: bool,
    buttons: u32,
    data: Option<IDataObject>,
    paths: Vec<PathBuf>,
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
    fn leave(&self) {
        let target = {
            let mut state = self.state.borrow_mut();
            state.destination = None;
            state.target.take()
        };
        if let Some(target) = target { let _ = unsafe { target.DragLeave() }; }
        self.highlight(-1, -1);
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
            let row = hit.row as usize + pane.model.page_start.get();
            let mut target_row = hit.row;
            let dir = match pane.model.slot(row) {
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
        let Some((dir, pane, row)) = self.destination(pt) else { self.leave(); return Ok(()); };
        let same_pane = ORIGIN.get() == Some(pane as usize);
        // Respect explicit copy/link intent by refusing it, never silently moving.
        if same_pane && keys.0 & (8 | 32) != 0 { self.leave(); return Ok(()); }
        let keys = if same_pane { MODIFIERKEYS_FLAGS(keys.0 | 4) } else { keys };
        let (paths, data, previous, target, buttons) = {
            let state = self.state.borrow();
            (state.paths.clone(), state.data.clone(), state.destination.clone(), state.target.clone(), state.buttons)
        };
        // Revalidate hover using the initiating button even after release. The Shell
        // may reject a DragOver with no buttons; Drop itself receives the real state.
        let keys = if final_check { MODIFIERKEYS_FLAGS(keys.0 | buttons) } else { keys };
        if (final_check || previous.as_ref() != Some(&dir)) && !valid_destination(&paths, &dir) { self.leave(); return Ok(()); }
        // This feature supports copy and move only. Explicit link modifiers must not
        // silently fall back to a destructive move.
        let supported = if same_pane { DROPEFFECT_MOVE.0 } else { DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 };
        let mut chosen = DROPEFFECT(allowed.0 & supported);
        let target = if previous.as_ref() == Some(&dir) { target } else {
            self.leave();
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Shell::BHID_DataObject;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::current_dir().unwrap().join("target/dnd-test-temp").join(format!("enyfile-dnd-{tag}-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap()));
            std::fs::create_dir_all(root.join("src/folder/sub")).unwrap();
            std::fs::create_dir_all(root.join("dst")).unwrap();
            std::fs::write(root.join("src/a.txt"), b"a").unwrap();
            std::fs::write(root.join("src/b.txt"), b"b").unwrap();
            std::fs::write(root.join("src/folder/sub/c.txt"), b"c").unwrap();
            Self(std::fs::canonicalize(root).unwrap())
        }
        fn paths(&self) -> Vec<PathBuf> { ["a.txt", "b.txt", "folder"].map(|p| self.0.join("src").join(p)).to_vec() }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn rejects_same_missing_and_descendants() {
        let f = Fixture::new("guards");
        let paths = f.paths();
        assert!(valid_destination(&paths, &f.0.join("dst")));
        assert!(!valid_destination(&paths, &f.0.join("src")));
        assert!(!valid_destination(&paths, &f.0.join("src/folder/sub")));
        assert!(!valid_destination(&paths, &f.0.join("missing")));
        assert!(!valid_destination(&paths, &f.0.join("src/a.txt")));
        assert!(!valid_destination(&[f.0.join("missing")], &f.0.join("dst")));
        assert!(!valid_destination(&[], &f.0.join("dst")));
    }

    #[test]
    fn internal_origin_scope_is_restored() {
        assert_eq!(ORIGIN.get(), None);
        assert!(accepts_origin(None, false, 0));
        assert!(accepts_origin(None, true, 0));
        assert!(!accepts_origin(Some(0), false, 1));
        assert!(!accepts_origin(Some(0), true, 0));
        assert!(accepts_origin(Some(0), true, 1));
        { let _outer = Origin::new(1); assert_eq!(ORIGIN.get(), Some(1));
          { let _inner = Origin::new(2); assert_eq!(ORIGIN.get(), Some(2)); }
          assert_eq!(ORIGIN.get(), Some(1)); }
        assert_eq!(ORIGIN.get(), None);
    }

    #[test]
    fn shell_target_multiple_copy_move_and_drag_cancel() {
        let _ole = shell::Apartment::new().unwrap();
        for (tag, keys, expected) in [("copy", 8, DROPEFFECT_COPY), ("move", 4, DROPEFFECT_MOVE), ("default", 0, DROPEFFECT_MOVE)] {
            let f = Fixture::new(tag);
            let paths = f.paths();
            let data: IDataObject = unsafe { shell::item_array(&paths).unwrap().BindToHandler(None, &BHID_DataObject).unwrap() };
            let target = folder_target(&f.0.join("dst")).unwrap();
            let point = POINTL::default();
            let allowed = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            let mut effect = allowed;
            unsafe {
                target.DragEnter(&data, MODIFIERKEYS_FLAGS(keys | 1), point, &mut effect).unwrap();
                assert_eq!(effect, expected);
                // Leaving (including Esc) must not touch any files.
                target.DragLeave().unwrap();
                assert!(paths.iter().all(|p| p.exists()));
                assert_eq!(std::fs::read_dir(f.0.join("dst")).unwrap().count(), 0);
                effect = allowed;
                target.DragEnter(&data, MODIFIERKEYS_FLAGS(keys | 1), point, &mut effect).unwrap();
                effect = allowed;
                target.DragOver(MODIFIERKEYS_FLAGS(keys | 1), point, &mut effect).unwrap();
                assert_eq!(effect, expected);
                target.Drop(&data, MODIFIERKEYS_FLAGS(keys), point, &mut effect).unwrap();
            }
            // Some Shell versions schedule the copy engine asynchronously.
            let start = Instant::now();
            while (!f.0.join("dst/folder/sub/c.txt").exists() || !f.0.join("dst/b.txt").exists()) && start.elapsed() < Duration::from_secs(10) {
                // The Shell copy worker can call back into this source STA.
                use windows::Win32::UI::WindowsAndMessaging::*;
                unsafe {
                    let mut msg = MSG::default();
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(std::fs::read(f.0.join("dst/a.txt")).unwrap(), b"a");
            assert_eq!(std::fs::read(f.0.join("dst/b.txt")).unwrap(), b"b");
            assert_eq!(std::fs::read(f.0.join("dst/folder/sub/c.txt")).unwrap(), b"c");
            for path in &paths { assert_eq!(path.exists(), expected == DROPEFFECT_COPY); }
        }
    }
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        self.leave();
        *self.state.borrow_mut() = State::default();
        let data = data.as_ref().cloned();
        let paths = data.as_ref().and_then(shell::hdrop_paths).unwrap_or_default();
        let wheel = WheelHook::new(self.hwnd).ok();
        *self.state.borrow_mut() = State { data, paths, wheel, buttons: keys.0 & (1 | 2 | 16), ..Default::default() };
        // Unsupported virtual objects fail closed; never promise an operation we cannot validate.
        self.over(keys, pt, effect, false)
    }
    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        self.over(keys, pt, effect, false)
    }
    fn DragLeave(&self) -> WinResult<()> {
        self.leave();
        *self.state.borrow_mut() = State::default();
        Ok(())
    }
    fn Drop(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> WinResult<()> {
        let wheel = self.state.borrow_mut().wheel.take();
        drop(wheel); // No interception while Shell transfer/conflict dialogs are open.
        // Recheck position, setting, identities and existence on release.
        if let Err(error) = self.over(keys, pt, effect, true) {
            self.leave();
            *self.state.borrow_mut() = State::default();
            return Err(error);
        }
        let (target, move_only) = {
            let mut state = self.state.borrow_mut();
            (state.target.take(), state.move_only)
        };
        let keys = if move_only { MODIFIERKEYS_FLAGS(keys.0 | 4) } else { keys };
        self.highlight(-1, -1);
        let result = if unsafe { *effect } != DROPEFFECT_NONE {
            if let Some(target) = target { unsafe { target.Drop(data.as_ref(), keys, *pt, effect) } } else { Ok(()) }
        } else {
            if let Some(target) = target { let _ = unsafe { target.DragLeave() }; }
            Ok(())
        };
        *self.state.borrow_mut() = State::default();
        if let Err(error) = &result {
            unsafe { *effect = DROPEFFECT_NONE; }
            if let Some(ui) = self.ui.upgrade() {
                ui.set_message(format!("ドロップできません: {error}").into());
                ui.set_message_error(true);
            }
        }
        // Shell operations (including partial/cancelled moves) may have changed both panes.
        slint::Timer::single_shot(Duration::ZERO, || {
            CONTROLLER.with(|cell| {
                if let Some(c) = cell.borrow().clone() {
                    let mut c = c.borrow_mut();
                    for p in 0..c.config.panes { let dir = c.data(p).dir.clone(); if !dir.as_os_str().is_empty() { c.navigate(p, dir, Mode::Automatic); } }
                }
            });
        });
        result
    }
}
