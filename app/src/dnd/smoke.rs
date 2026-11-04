//! UI/OLE smoke scenarios, also used by release builds with --smoke.
use super::*;
use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_USEDEFAULTCURSORS};

#[implement(IDropTarget)]
struct FailingTarget(Rc<Cell<usize>>);
impl IDropTarget_Impl for FailingTarget_Impl {
    fn DragEnter(&self, _: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, _: *mut DROPEFFECT) -> WinResult<()> {
        Err(windows::Win32::Foundation::E_FAIL.into())
    }
    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, _: *mut DROPEFFECT) -> WinResult<()> {
        Err(windows::Win32::Foundation::E_FAIL.into())
    }
    fn DragLeave(&self) -> WinResult<()> { self.0.set(self.0.get() + 1); Ok(()) }
    fn Drop(&self, _: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, _: *mut DROPEFFECT) -> WinResult<()> {
        Err(windows::Win32::Foundation::E_FAIL.into())
    }
}

fn check_failed_hover(ui: &App, point: POINTL, data: &IDataObject) -> Result<(), String> {
    let hwnd = hwnd(ui).ok_or("no HWND")?;
    let left = Rc::new(Cell::new(0));
    let target = Target { hwnd, ui: ui.as_weak(), state: RefCell::new(State::default()) };
    let (destination, _, _) = target.destination(&point).ok_or("no failure-test destination")?;
    *target.state.borrow_mut() = State {
        wheel: Some(WheelHook::new(hwnd).map_err(|e| e.to_string())?),
        data: Some(data.clone()),
        paths: shell::hdrop_paths(data).ok_or("no failure-test source")?.into(),
        destination: Some(destination), target: Some(FailingTarget(left.clone()).into()),
        ..Default::default()
    };
    target.highlight(1, 0);
    let mut effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
    let result = target.update_drag(MODIFIERKEYS_FLAGS(9), &point, &mut effect, false);
    let state = target.state.borrow();
    if result.is_ok() || effect != DROPEFFECT_NONE || left.get() != 1
        || state.data.is_some() || !state.paths.is_empty() || state.target.is_some()
        || state.wheel.is_some() || WHEEL_ROUTE.get().is_some() || ui.get_drop_pane() != -1 {
        return Err("failed hover retained its target, source, feedback or wheel capture".into());
    }
    Ok(())
}

/// The few pixels below a full page must not resolve to the next page's folder.
/// Use a real pane and native coordinates, but never perform a file transfer.
fn check_page_tail(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path, left: POINTL, data: &IDataObject) -> Result<(), String> {
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    let hwnd = hwnd(ui).ok_or("no HWND")?;
    let font = ui.get_font_size();
    let original = c.borrow().data(0);
    let start = c.borrow().panes[0].model.page_start.get();
    let options = { let c = c.borrow(); (c.config.pane_drag_drop, c.config.intra_pane_move) };
    let result = (|| -> Result<(), String> {
        let mut client = POINT { x: left.x, y: left.y };
        if !unsafe { ScreenToClient(hwnd, &mut client) }.as_bool() { return Err("ScreenToClient failed".into()); }
        let mut tail = None;
        // Usually the default height already has a remainder. Try nearby row heights
        // too, so an exactly divisible window cannot silently skip this regression.
        for offset in 0..4 {
            ui.set_font_size(font + offset as f32);
            redraw(hwnd);
            let count = c.borrow().panes[0].model.row_count();
            let scale = ui.window().scale_factor();
            for y in 0..ui.window().size().height as i32 {
                let hit = ui.invoke_hit_drop(client.x as f32 / scale, y as f32 / scale);
                if hit.pane == 0 && hit.in_name && hit.row as usize == count {
                    tail = Some((POINT { x: client.x, y }, count));
                    break;
                }
            }
            if tail.is_some() { break; }
        }
        let (mut point, count) = tail.ok_or("no partial-row blank area in the real pane")?;
        let next = match c.borrow().panes[0].model.slot(c.borrow().panes[0].model.page_start.get() + count) {
            Some(Slot::Entry(i)) => i,
            _ => return Err("tail regression needs an item on the next page".into()),
        };
        let hidden = root.join("dnd-probe/hidden-next-page");
        std::fs::create_dir_all(&hidden).map_err(|e| e.to_string())?;
        let mut snapshot = (*original).clone();
        snapshot.rows[next].path = Arc::new(hidden);
        snapshot.rows[next].directory = true;
        *c.borrow().panes[0].model.data.borrow_mut() = Arc::new(snapshot);
        c.borrow().panes[0].model.notify.reset();
        redraw(hwnd);
        if !unsafe { ClientToScreen(hwnd, &mut point) }.as_bool() { return Err("ClientToScreen failed".into()); }
        let point = POINTL { x: point.x, y: point.y };
        let probe = Target { hwnd, ui: ui.as_weak(), state: RefCell::new(State::default()) };
        let target: IDropTarget = Target { hwnd, ui: ui.as_weak(), state: RefCell::new(State::default()) }.into();
        for origin in [None, Some(1), Some(0)] {
            c.borrow_mut().config.pane_drag_drop = true;
            c.borrow_mut().config.intra_pane_move = true;
            let _origin = origin.map(Origin::new);
            let expected = (origin != Some(0)).then(|| (original.dir.clone(), 0, -1));
            let actual = probe.destination(&point);
            if actual != expected { return Err(format!("blank row {count} resolved outside the displayed page: {actual:?}")); }
            let mut effect = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            let keys = if origin == Some(0) { 1 } else { 9 };
            unsafe {
                target.DragEnter(data, MODIFIERKEYS_FLAGS(keys), point, &mut effect).map_err(|e| e.to_string())?;
                target.DragLeave().map_err(|e| e.to_string())?;
            }
            let expected_effect = if origin == Some(0) { DROPEFFECT_NONE } else { DROPEFFECT_COPY };
            if effect != expected_effect || ui.get_drop_row() != -1 { return Err("blank area advertised a hidden folder drop".into()); }
        }
        Ok(())
    })();
    {
        let mut c = c.borrow_mut();
        *c.panes[0].model.data.borrow_mut() = original;
        c.panes[0].model.page_start.set(start);
        c.panes[0].model.notify.reset();
        (c.config.pane_drag_drop, c.config.intra_pane_move) = options;
    }
    ui.set_font_size(font);
    redraw(hwnd);
    c.borrow_mut().repaint(0);
    result
}

/// A real drop while the other pane holds completed search results. The caller
/// checks those results on the next smoke tick, after deferred refreshes can run.
pub(crate) fn refresh(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path) -> Result<(), String> {
    use windows::Win32::{Graphics::Gdi::ClientToScreen, UI::Shell::BHID_DataObject};
    if c.borrow().panes[0].found.is_none() { return Err("refresh test needs search results".into()); }
    let hwnd = hwnd(ui).ok_or("no HWND")?;
    let scale = ui.window().scale_factor();
    let point_for = |pane, name| -> Result<POINTL, String> {
        for x in (2..(ui.window().size().width as f32 / scale) as i32).step_by(4) {
            let hit = ui.invoke_hit_drop(x as f32, 111.0);
            if hit.pane == pane && hit.in_name == name {
                let mut point = POINT { x: (x as f32 * scale) as i32, y: (111.0 * scale) as i32 };
                if !unsafe { ClientToScreen(hwnd, &mut point) }.as_bool() { return Err("ClientToScreen failed".into()); }
                return Ok(POINTL { x: point.x, y: point.y });
            }
        }
        Err("no refresh-test point".into())
    };
    let target: IDropTarget = Target { hwnd, ui: ui.as_weak(), state: RefCell::new(State::default()) }.into();
    let source = root.join("dnd-probe/search-refresh.txt");
    std::fs::write(&source, b"preserve search").map_err(|e| e.to_string())?;
    let data: IDataObject = unsafe { shell::item_array(&[std::fs::canonicalize(&source).map_err(|e| e.to_string())?])?.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.to_string())?;
    // Search results reject a drop; this must not schedule any listing work.
    let rejected = point_for(0, true)?;
    let mut effect = DROPEFFECT_COPY;
    unsafe {
        target.DragEnter(&data, MODIFIERKEYS_FLAGS(9), rejected, &mut effect).map_err(|e| e.to_string())?;
        target.Drop(&data, MODIFIERKEYS_FLAGS(8), rejected, &mut effect).map_err(|e| e.to_string())?;
    }
    if effect != DROPEFFECT_NONE { return Err("search pane accepted a drop".into()); }
    let accepted = point_for(1, false)?;
    let destination = c.borrow().data(1).dir.clone();
    if !std::fs::canonicalize(&destination).is_ok_and(|dir| dir.starts_with(root)) { return Err("refresh test escaped its fixture".into()); }
    effect = DROPEFFECT_COPY;
    unsafe {
        target.DragEnter(&data, MODIFIERKEYS_FLAGS(9), accepted, &mut effect).map_err(|e| e.to_string())?;
        target.Drop(&data, MODIFIERKEYS_FLAGS(8), accepted, &mut effect).map_err(|e| e.to_string())?;
    }
    if effect != DROPEFFECT_COPY { return Err("refresh test did not hand off its copy".into()); }
    Ok(())
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
        let checked = self.ui.upgrade().ok_or("window closed".to_string())
            .and_then(|ui| smoke_check(&ui, &self.controller, &self.root));
        *self.result.borrow_mut() = Some(checked);
        DRAGDROP_S_CANCEL
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> windows::core::HRESULT { DRAGDROP_S_USEDEFAULTCURSORS }
}
pub(crate) fn smoke_modal(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path) -> Result<(), String> {
    use windows::Win32::UI::Shell::BHID_DataObject;
    let data: IDataObject = unsafe { shell::item_array(&[root.join("left")])?.BindToHandler(None, &BHID_DataObject) }.map_err(|e| e.to_string())?;
    let result = Rc::new(RefCell::new(None));
    let source: IDropSource = ModalProbe { ui: ui.as_weak(), controller: c.clone(), root: root.to_path_buf(), result: result.clone() }.into();
    let mut effect = DROPEFFECT_NONE;
    // Wake OLE's input pump without moving the pointer or injecting system input.
    unsafe { PostThreadMessageW(windows::Win32::System::Threading::GetCurrentThreadId(), WM_MOUSEMOVE, WPARAM(0), LPARAM(0)) }.map_err(|e| e.to_string())?;
    let status = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    if status != DRAGDROP_S_CANCEL { return Err(format!("modal probe did not cancel: {status:?}")); }
    let checked = result.borrow_mut().take().ok_or("modal probe did not run")?;
    checked
}

/// Called only by --smoke, against its dedicated fixture. Exercise the actual OLE
/// adapter and Slint hit testing without releasing a drop onto the user's desktop.
fn smoke_check(ui: &App, c: &Rc<RefCell<Controller>>, root: &Path) -> std::result::Result<(), String> {
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
        check_failed_hover(ui, right, &data)?;
        check_page_tail(ui, c, root, left, &data)?;
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
