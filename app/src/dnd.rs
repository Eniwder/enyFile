//! OLE adapter for the Slint window. The folder's own Shell target owns the transfer,
//! including source notification, conflict UI, cancellation and partial completion.
//! All interfaces and callbacks stay on the window's OLE STA. Never hold a Controller
//! or RefCell borrow while calling Shell COM: it may pump messages and reenter us.
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
// Wheel input can otherwise be consumed by the source application's OLE drag loop.
// The low-level callback only posts a message; UI/COM work happens outside the hook.
pub(super) const WHEEL_MESSAGE: u32 = WM_APP + 0x431;
thread_local! {
    static WHEEL_ROUTE: Cell<Option<(HWND, usize)>> = const { Cell::new(None) };
    static WHEEL_SERIAL: Cell<usize> = const { Cell::new(0) };
    static WHEEL_REMAINDER: Cell<(i32, i32)> = const { Cell::new((-1, 0)) };
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
        ui.set_drop_pane(-1);
        ui.set_drop_row(-1);
        for _ in 0..(total / 120).unsigned_abs() { c.page_step(pane as usize, total < 0); }
    });
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

/// Called only by --smoke, against its dedicated fixture. Exercise the actual OLE
/// adapter and Slint hit testing without releasing a drop onto the user's desktop.
pub(super) fn smoke_check(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path) -> std::result::Result<(), String> {
    use windows::Win32::{Graphics::Gdi::ClientToScreen, UI::Shell::BHID_DataObject};
    let check = || -> std::result::Result<(), String> {
        let hwnd = hwnd(ui).ok_or("no HWND")?;
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
        let size = ui.window().size();
        'rows: for y in (111..(size.height as f32 / scale) as i32 - 26).step_by(8) {
            for x in (2..(size.width as f32 / scale) as i32).step_by(16) {
                let hit = ui.invoke_hit_drop(x as f32, y as f32);
                if hit.pane < 0 || !checked.insert((hit.pane, hit.row, hit.in_name)) { continue; }
                let expected = {
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
                    if hit.in_name { folder } else { data.dir.clone() }
                };
                let mut point = POINT { x: (x as f32 * scale) as i32, y: (y as f32 * scale) as i32 };
                if !unsafe { ClientToScreen(hwnd, &mut point) }.as_bool() { return Err("ClientToScreen failed".into()); }
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
        Ok(())
    };
    let original = c.borrow().config.pane_drag_drop;
    let result = check();
    c.borrow_mut().config.pane_drag_drop = original;
    result
}

#[derive(Default)]
struct State {
    wheel: Option<WheelHook>,
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
            ui.set_drop_pane(pane);
            ui.set_drop_row(row);
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
            if !accepts_origin(ORIGIN.get(), c.config.pane_drag_drop, p) { return None; }
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
            Some((dir, hit.pane, target_row))
        })
    }
    fn over(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT, final_check: bool) -> WinResult<()> {
        // OLE supplies a valid, writable in/out effect pointer for every callback.
        if effect.is_null() { return Err(E_POINTER.into()); }
        let allowed = unsafe { *effect };
        unsafe { *effect = DROPEFFECT_NONE; }
        let Some((dir, pane, row)) = self.destination(pt) else { self.leave(); return Ok(()); };
        let (paths, data, previous, target) = {
            let state = self.state.borrow();
            (state.paths.clone(), state.data.clone(), state.destination.clone(), state.target.clone())
        };
        if (final_check || previous.as_ref() != Some(&dir)) && !valid_destination(&paths, &dir) { self.leave(); return Ok(()); }
        // This feature supports copy and move only. Explicit link modifiers must not
        // silently fall back to a destructive move.
        let mut chosen = DROPEFFECT(allowed.0 & (DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0));
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
        *self.state.borrow_mut() = State { data, paths, wheel, ..Default::default() };
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
        let target = self.state.borrow_mut().target.take();
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
