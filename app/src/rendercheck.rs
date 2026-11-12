//! Isolated native-window recovery check; no clipboard access or real folder operations.
use super::*;
use windows::Win32::Foundation::{LPARAM, WPARAM};

pub fn start(c: Rc<RefCell<Controller>>, activity: Rc<RefCell<recovery::Activity>>, attached: Rc<Cell<Option<HWND>>>, root: PathBuf) -> slint::Timer {
    let timer = slint::Timer::default();
    let started = Instant::now();
    let mut stage = 0;
    let mut baseline = 0;
    let mut first_rebuilt = None;
    let mut folders: Vec<PathBuf> = Vec::new();
    let mut selections: Vec<Vec<PathBuf>> = Vec::new();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        let result = (|| -> Result<bool, String> {
            if started.elapsed() > Duration::from_secs(75) { return Err("recovery check timed out".into()); }
            let ui = c.borrow().ui.upgrade().ok_or("UI disappeared")?;
            let Some(h) = hwnd(&ui).filter(|h| attached.get() == Some(*h)) else { return Ok(false) };
            if stage == 0 {
                let controller = c.borrow();
                if controller.panes.iter().any(|p| p.pending.is_some()) || activity.borrow().graphics_generations == 0 { return Ok(false); }
                folders = controller.panes.iter().map(|p| p.model.data.borrow().dir.clone()).collect();
                selections = (0..4).map(|p| controller.selected_paths(p)).collect();
                baseline = activity.borrow().graphics_generations;
                drop(controller);
                let mut key = slint::language::KeyEvent::default();
                key.text = slint::platform::Key::F12.into();
                key.modifiers.control = true;
                key.modifiers.shift = true;
                ui.invoke_key_event(1, key);
                stage = 1;
            } else if stage == 1 || stage == 3 {
                if activity.borrow().graphics_generations <= baseline { return Ok(false); }
                let controller = c.borrow();
                for p in 0..4 {
                    if controller.panes[p].model.data.borrow().dir != folders[p] || controller.selected_paths(p) != selections[p] {
                        return Err(format!("pane {} state changed across recreation", p + 1));
                    }
                }
                let mut alpha = 0;
                let mut flags = windows::Win32::UI::WindowsAndMessaging::LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetLayeredWindowAttributes(h, None, Some(&mut alpha), Some(&mut flags)) }.map_err(|e| e.to_string())?;
                let expected = (controller.config.opacity * 255.0).round() as u8;
                if alpha != expected { return Err(format!("opacity not restored: {alpha} != {expected}")); }
                drop(controller);
                recovery::checkpoint(format!("render recovery native check: stage={stage} passed; generation={} alpha={alpha}", activity.borrow().graphics_generations));
                if stage == 3 { return Ok(true); }
                baseline = activity.borrow().graphics_generations;
                first_rebuilt = Some(Instant::now());
                stage = 2;
            } else if stage == 2 && first_rebuilt.is_some_and(|at| at.elapsed() >= Duration::from_secs(31)) {
                #[repr(C)]
                struct Setting { guid: windows::core::GUID, len: u32, state: u32 }
                use windows::Win32::{System::SystemServices::GUID_SESSION_DISPLAY_STATUS, UI::WindowsAndMessaging::*};
                for state in [0, 1] {
                    let setting = Setting { guid: GUID_SESSION_DISPLAY_STATUS, len: 4, state };
                    unsafe { SendMessageW(h, WM_POWERBROADCAST, Some(WPARAM(PBT_POWERSETTINGCHANGE as usize)), Some(LPARAM(&setting as *const _ as isize))); }
                }
                stage = 3;
            }
            Ok(false)
        })();
        if !matches!(result, Ok(false)) {
            let passed = result.is_ok();
            let note = match result { Ok(_) => "PASS: manual and display-on recreation; pane state and opacity preserved".into(), Err(error) => format!("FAIL: {error}") };
            let _ = std::fs::write(root.join("render-check.txt"), &note);
            recovery::checkpoint(note);
            if passed {
                // Check normal close still terminates the until-quit event loop.
                let ui = c.borrow().ui.upgrade();
                if let Some(h) = ui.as_ref().and_then(hwnd) {
                    unsafe { windows::Win32::UI::WindowsAndMessaging::SendMessageW(h, windows::Win32::UI::WindowsAndMessaging::WM_CLOSE, None, None); }
                }
            } else {
                let _ = slint::quit_event_loop();
            }
        }
    });
    timer
}
