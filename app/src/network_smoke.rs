//! Opt-in integration probe for a real remote filesystem. It never stops WSL itself.
//! --network-smoke <local-test-root> <remote-test-folder> uses isolated settings.
//! An external runner writes {id, action} to command.json and reads status.json.
use super::*;

pub fn start(c: Rc<RefCell<Controller>>, root: PathBuf, remote: PathBuf) -> slint::Timer {
    let timer = slint::Timer::default();
    let mut ticks = 0u64;
    let mut acknowledged = 0u64;
    let mut probe_ok = false;
    let mut quit = false;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        ticks += 1;
        let command = root.join("command.json");
        if let Ok(bytes) = std::fs::read(&command) {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let (Some(id), Some(action)) = (value["id"].as_u64(), value["action"].as_str()) {
                    if id > acknowledged {
                        match action {
                            "refresh" => c.borrow_mut().command(1, 3),
                            "automatic" => c.borrow_mut().refresh_shown(None),
                            "resume" => c.borrow_mut().resumed(),
                            "cached-remote" => {
                                // A last-known listing fixture for testing stopped/unavailable WSL.
                                let mut c = c.borrow_mut();
                                let mut rows = c.data(0).rows.clone();
                                for row in &mut rows { row.path = Arc::new(remote.join(&row.name)); }
                                c.panes[1].generation += 1;
                                let generation = c.panes[1].generation;
                                c.panes[1].pending = Some(Mode::Refresh);
                                c.complete(1, generation, Ok(Arc::new(Snapshot::new(remote.clone(), rows, None))));
                            }
                            "stop-watch" => c.borrow_mut().panes[1].watch = None,
                            "local" => c.borrow_mut().navigate(1, root.join("local"), Mode::New(None)),
                            "remote" => c.borrow_mut().navigate(1, remote.clone(), Mode::New(None)),
                            "slow" => c.borrow_mut().navigate(1, remote.join("slow"), Mode::New(None)),
                            "probe" => {
                                // Invoke callbacks outside the controller borrow.
                                let ui = c.borrow().ui.upgrade();
                                if let Some(ui) = ui {
                                    ui.invoke_show_menu(0);
                                    let opened = ui.get_open_menu() == 0;
                                    ui.invoke_dismiss_menu();
                                    let mut key = slint::language::KeyEvent::default();
                                    key.text = slint::SharedString::from(slint::platform::Key::Tab);
                                    ui.invoke_key_event(0, key);
                                    probe_ok = opened && ui.get_open_menu() == -1 && c.borrow().active == 1;
                                    c.borrow_mut().command(0, 3);
                                }
                            }
                            "exit" => quit = true,
                            _ => {}
                        }
                        acknowledged = id;
                    }
                }
            }
        }
        let state = {
            let c = c.borrow();
            let panes: Vec<_> = c.panes.iter().take(2).map(|pane| {
                let data = pane.model.data.borrow();
                serde_json::json!({
                    "pending": pane.pending.is_some(), "stale": pane.stale,
                    "wsl_paused": pane.wsl_paused,
                    "generation": pane.generation, "watch": pane.watch.as_ref().is_some_and(shell::Watch::is_alive),
                    "path": display(&data.dir),
                    "names": data.rows.iter().map(|e| &e.name).collect::<Vec<_>>(),
                })
            }).collect();
            serde_json::json!({"ticks": ticks, "ack": acknowledged, "probe_ok": probe_ok,
                "message": c.ui.upgrade().map(|ui| ui.get_message().to_string()), "panes": panes})
        };
        // Test files are local. Replace atomically so the runner never sees partial JSON.
        let temporary = root.join("status.tmp");
        if std::fs::write(&temporary, state.to_string()).is_ok() {
            let _ = std::fs::rename(temporary, root.join("status.json"));
        }
        if quit {
            let _ = slint::quit_event_loop();
        }
    });
    timer
}
