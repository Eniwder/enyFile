//! `--smoke <root> <log>`: drives the real UI callbacks and Windows clipboard against
//! `enyfile-shellprobe`, which uses the Shell's own data object and folder Paste command.
//! Overwrites the Windows clipboard. Never touches anything outside `<root>`.
use super::*;
use std::{
    io::Write,
    process::{Child, Command},
};

fn names(c: &Rc<RefCell<Controller>>, p: usize) -> Vec<String> {
    c.borrow().data(p).rows.iter().map(|e| e.name.clone()).collect()
}
/// Model row of `name` (shifted by the ".." row), as the UI passes it to `pointer`.
fn index(c: &Rc<RefCell<Controller>>, p: usize, name: &str) -> Result<i32, String> {
    let offset = c.borrow().data(p).offset();
    names(c, p)
        .iter()
        .position(|n| n == name)
        .map(|i| (i + offset) as i32)
        .ok_or_else(|| format!("{name} not listed in pane {p}"))
}
fn selected(c: &Rc<RefCell<Controller>>, p: usize) -> Vec<String> {
    let c = c.borrow();
    c.selected_paths(p)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}
/// The conflict question on screen: (name, kind).
fn asked(c: &Rc<RefCell<Controller>>) -> Option<(String, paste::Kind)> {
    let c = c.borrow();
    let (_, q, _) = c.asking.front()?;
    Some((q.target.file_name()?.to_string_lossy().into_owned(), q.kind))
}
fn idle(c: &Rc<RefCell<Controller>>) -> bool {
    let c = c.borrow();
    c.operations == 0
        && c.panes
            .iter()
            .all(|p| p.pending.is_none() && !p.stale && !p.model.data.borrow().dir.as_os_str().is_empty())
}
fn click(ui: &App, p: i32, i: i32, ctrl: bool) {
    ui.invoke_pointer(p, i, ctrl, false, 0);
    ui.invoke_pointer(p, i, false, false, 1);
}
fn ctrl_key(ui: &App, p: i32, letter: &str) {
    let mut event = slint::language::KeyEvent::default();
    event.text = letter.into();
    event.modifiers.control = true;
    ui.invoke_key_event(p, event);
}
fn fkey(ui: &App, p: i32, key: slint::platform::Key) {
    let mut event = slint::language::KeyEvent::default();
    event.text = key.into();
    ui.invoke_key_event(p, event);
}
/// Through the window's real key path (FocusScope + tracked modifiers), not the callback.
fn window_ctrl_key(ui: &App, letter: &str) {
    use slint::platform::{Key, WindowEvent};
    let w = ui.window();
    w.dispatch_event(WindowEvent::KeyPressed { text: Key::Control.into() });
    w.dispatch_event(WindowEvent::KeyPressed { text: letter.into() });
    w.dispatch_event(WindowEvent::KeyReleased { text: letter.into() });
    w.dispatch_event(WindowEvent::KeyReleased { text: Key::Control.into() });
}
fn expect(ok: bool, what: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.into()) }
}
fn clip() -> Result<(Vec<String>, u32), String> {
    let (paths, effect) = shell::read_clipboard()?;
    Ok((
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect(),
        effect,
    ))
}

struct State {
    step: usize,
    probe: Option<Child>,
    log: Vec<String>,
    root: PathBuf,
    /// ENYFILE_SMOKE_HOLD_MS keeps the first conflict dialog open (for screenshots).
    hold: Duration,
    seen: Option<Instant>,
    held: Option<Instant>,
}
impl State {
    fn spawn(&mut self, args: &[&std::ffi::OsStr]) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let probe = exe.with_file_name("enyfile-shellprobe.exe");
        // Shell parsing names do not accept the `\\?\` form `canonicalize` produces.
        let args = args.iter().map(|a| shell::shell_path(Path::new(a)));
        self.probe = Some(Command::new(probe).args(args).spawn().map_err(|e| e.to_string())?);
        Ok(())
    }
    /// None while running; Some(exit code) once finished.
    fn probe_done(&mut self) -> Option<i32> {
        let status = self.probe.as_mut()?.try_wait().ok()??;
        self.probe = None;
        Some(status.code().unwrap_or(-1))
    }
    fn pass(&mut self, what: &str) {
        self.log.push(format!("PASS {what}"));
    }
}

pub fn start(c: Rc<RefCell<Controller>>, root: PathBuf, log: Option<PathBuf>) -> slint::Timer {
    let timer = slint::Timer::default();
    let started = Instant::now();
    let state = RefCell::new(State {
        step: 0,
        probe: None,
        log: Vec::new(),
        root: std::fs::canonicalize(&root).unwrap_or(root),
        hold: Duration::from_millis(
            std::env::var("ENYFILE_SMOKE_HOLD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        ),
        seen: None,
        held: None,
    });
    let finish = move |state: &mut State, error: Option<String>| {
        if let Some(mut probe) = state.probe.take() {
            let _ = probe.kill();
        }
        state.log.push(match &error {
            Some(e) => format!("FAIL step {}: {e}", state.step),
            None => "PASS all".into(),
        });
        if let Some(log) = &log {
            if let Ok(mut f) = std::fs::File::create(log) {
                let _ = f.write_all(state.log.join("\n").as_bytes());
            }
        }
        let _ = slint::quit_event_loop();
    };
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        let mut s = state.borrow_mut();
        if s.step == usize::MAX {
            return;
        }
        if started.elapsed() > Duration::from_secs(90) {
            finish(&mut s, Some("timeout".into()));
            s.step = usize::MAX;
            return;
        }
        let Some(ui) = c.borrow().ui.upgrade() else { return };
        let root = s.root.clone();
        let result = (|| -> Result<bool, String> {
            match s.step {
                0 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    let a = index(&c, 0, "a.txt")?;
                    let j = index(&c, 0, "日本語 ファイル.txt")?;
                    click(&ui, 0, a, false);
                    click(&ui, 0, j, true);
                    ui.invoke_focus_pane(0);
                    window_ctrl_key(&ui, "c");
                    let (files, effect) = clip()?;
                    expect(files == ["a.txt", "日本語 ファイル.txt"], "Ctrl+C puts selected files on clipboard")?;
                    expect(effect & 2 == 0 && effect & 1 != 0, "Ctrl+C preferred effect is COPY")?;
                    s.pass("Ctrl+C via window key path -> CF_HDROP + Preferred DropEffect=COPY");
                    ctrl_key(&ui, 1, "v");
                }
                1 => {
                    if !idle(&c) || !names(&c, 1).contains(&"a.txt".to_string()) {
                        return Ok(false);
                    }
                    for n in ["a.txt", "日本語 ファイル.txt"] {
                        let same = std::fs::read(root.join("left").join(n)).ok()
                            == std::fs::read(root.join("right").join(n)).ok();
                        expect(same, "pasted bytes match")?;
                    }
                    let mut sel = selected(&c, 1);
                    sel.sort();
                    expect(sel == ["a.txt", "日本語 ファイル.txt"], "pasted items are selected")?;
                    s.pass("app Ctrl+V of app copy: IFileOperation copy, pasted items selected");
                    ctrl_key(&ui, 0, "v");
                }
                2 => {
                    if !idle(&c) || names(&c, 0).len() < 6 {
                        return Ok(false);
                    }
                    let added = selected(&c, 0);
                    expect(added.len() == 2 && added.iter().all(|n| n != "a.txt" && n != "日本語 ファイル.txt"), "same-folder paste creates renamed duplicates")?;
                    s.log.push(format!("INFO same-folder names: {added:?}"));
                    s.pass("same-folder Ctrl+V duplicates with Windows naming, no prompt");
                    std::fs::write(root.join("right/日本語 ファイル.txt"), "keep me").map_err(|e| e.to_string())?;
                    ctrl_key(&ui, 1, "v");
                }
                3 => {
                    if asked(&c).is_none_or(|(name, _)| name != "a.txt") {
                        return Ok(false);
                    }
                    if s.seen.get_or_insert_with(Instant::now).elapsed() < s.hold {
                        return Ok(false);
                    }
                    expect(ui.get_ask_open() && ui.get_ask().kind == 0, "own dialog asks about the file")?;
                    expect(ui.get_ask().rename == "a (2).txt", "default rename is keep-both name")?;
                    expect(!ui.invoke_ask_validate("A.TXT".into()).is_empty(), "existing name rejected case-insensitively")?;
                    ui.invoke_ask_answer(4, "".into(), false);
                    expect(asked(&c).is_some(), "merge is ignored for files")?;
                    ui.invoke_ask_answer(3, "a renamed.txt".into(), false);
                }
                4 => {
                    if asked(&c).is_none_or(|(name, _)| name != "日本語 ファイル.txt") {
                        return Ok(false);
                    }
                    ui.invoke_ask_answer(2, "".into(), false);
                }
                5 => {
                    if !idle(&c) || !names(&c, 1).contains(&"a renamed.txt".to_string()) {
                        return Ok(false);
                    }
                    expect(std::fs::read(root.join("right/a renamed.txt")).ok() == std::fs::read(root.join("left/a.txt")).ok(), "renamed copy bytes")?;
                    expect(std::fs::read(root.join("right/日本語 ファイル.txt")).ok().as_deref() == Some("keep me".as_bytes()), "skipped item untouched")?;
                    expect(selected(&c, 1) == ["a renamed.txt"], "renamed item selected")?;
                    s.pass("one-by-one dialog: rename with validation, skip, no Windows prompt");
                    std::fs::write(root.join("right/a.txt"), "old").map_err(|e| e.to_string())?;
                    ctrl_key(&ui, 1, "v");
                }
                6 => {
                    if asked(&c).is_none() {
                        return Ok(false);
                    }
                    ui.invoke_ask_answer(0, "".into(), true);
                }
                7 => {
                    if !idle(&c) || std::fs::read(root.join("right/a.txt")).ok().as_deref() != Some("alpha".as_bytes()) {
                        return Ok(false);
                    }
                    expect(std::fs::read(root.join("right/日本語 ファイル.txt")).ok() == std::fs::read(root.join("left/日本語 ファイル.txt")).ok(), "apply to all overwrote the second file")?;
                    s.pass("overwrite + apply to all: second conflict not asked");
                    click(&ui, 0, index(&c, 0, "Folder A")?, false);
                    ctrl_key(&ui, 0, "c");
                    ctrl_key(&ui, 1, "v");
                }
                8 => {
                    if asked(&c).is_none_or(|(name, _)| name != "Folder A") {
                        return Ok(false);
                    }
                    expect(ui.get_ask().kind == 1, "folder conflict")?;
                    ui.invoke_ask_answer(4, "".into(), false);
                }
                9 => {
                    if asked(&c).is_none_or(|(name, _)| name != "inner.txt") {
                        return Ok(false);
                    }
                    expect(ui.get_ask().existing_path.contains("Folder A"), "nested conflict shows inner path")?;
                    ui.invoke_ask_answer(0, "".into(), false);
                }
                10 => {
                    if !idle(&c) || !root.join("right/Folder A/sub/deep.txt").exists() {
                        return Ok(false);
                    }
                    let read = |p: &str| std::fs::read_to_string(root.join(p)).unwrap_or_default();
                    expect(read("right/Folder A/inner.txt") == "new", "nested overwrite")?;
                    expect(read("right/Folder A/keep.txt") == "keep", "merge keeps existing files")?;
                    expect(selected(&c, 1) == ["Folder A"], "merged folder selected")?;
                    s.pass("folder merge recurses and asks about nested conflicts");
                    std::fs::write(root.join("right/Folder A/inner.txt"), "changed").map_err(|e| e.to_string())?;
                    ctrl_key(&ui, 1, "v");
                }
                11 => {
                    if asked(&c).is_none_or(|(name, _)| name != "Folder A") {
                        return Ok(false);
                    }
                    ui.invoke_ask_answer(4, "".into(), false);
                }
                12 => {
                    // Cancel on the nested question: nothing at all may change.
                    if asked(&c).is_none_or(|(name, _)| name != "inner.txt") {
                        return Ok(false);
                    }
                    ui.invoke_ask_answer(6, "".into(), false);
                }
                13 => {
                    if !idle(&c) || !asked(&c).is_none() {
                        return Ok(false);
                    }
                    expect(std::fs::read_to_string(root.join("right/Folder A/inner.txt")).ok().as_deref() == Some("changed"), "cancel changed nothing")?;
                    expect(!ui.get_ask_open(), "dialog closed after cancel")?;
                    s.pass("cancel during nested questions leaves files untouched");
                    ctrl_key(&ui, 1, "v");
                }
                14 => {
                    if asked(&c).is_none_or(|(name, _)| name != "Folder A") {
                        return Ok(false);
                    }
                    ui.invoke_ask_answer(5, "".into(), false);
                }
                15 => {
                    if !idle(&c) || root.join("right/Folder A/keep.txt").exists() {
                        return Ok(false);
                    }
                    expect(std::fs::read_to_string(root.join("right/Folder A/inner.txt")).ok().as_deref() == Some("new"), "replaced folder content")?;
                    expect(root.join("right/Folder A/sub/deep.txt").exists(), "replaced folder complete")?;
                    s.pass("folder replace: existing folder removed, incoming copied");
                    // Restore the clipboard the Explorer-side steps expect.
                    click(&ui, 0, index(&c, 0, "a.txt")?, false);
                    click(&ui, 0, index(&c, 0, "日本語 ファイル.txt")?, true);
                    ctrl_key(&ui, 0, "c");
                    let dst = root.join("explorer-dst");
                    s.spawn(&["paste".as_ref(), dst.as_os_str()])?;
                }
                16 => {
                    let Some(code) = s.probe_done() else { return Ok(false) };
                    expect(code == 0, "probe paste exit code")?;
                    let dst = root.join("explorer-dst");
                    if !(dst.join("a.txt").exists() && dst.join("日本語 ファイル.txt").exists()) {
                        return Err("Shell Paste of app copy did not create files".into());
                    }
                    s.pass("Shell folder Paste consumes app Ctrl+C (Explorer side)");
                    let j = index(&c, 1, "日本語 ファイル.txt")?;
                    click(&ui, 1, j, false);
                    ctrl_key(&ui, 1, "x");
                    let (files, effect) = clip()?;
                    expect(files == ["日本語 ファイル.txt"] && effect == 2, "Ctrl+X preferred effect is MOVE")?;
                    let dst2 = root.join("explorer-dst2");
                    s.spawn(&["paste".as_ref(), dst2.as_os_str()])?;
                }
                17 => {
                    let Some(code) = s.probe_done() else { return Ok(false) };
                    expect(code == 0, "probe paste exit code")?;
                    expect(root.join("explorer-dst2/日本語 ファイル.txt").exists(), "Shell Paste of app cut created file")?;
                    expect(!root.join("right/日本語 ファイル.txt").exists(), "Shell Paste of app cut removed source")?;
                    s.pass("Shell folder Paste moves app Ctrl+X items");
                }
                18 => {
                    // The watcher must drop the moved item without F5.
                    if names(&c, 1).contains(&"日本語 ファイル.txt".to_string()) || !idle(&c) {
                        return Ok(false);
                    }
                    s.pass("folder watcher refreshed pane after external move");
                    let src = root.join("explorer-src/ex_copy.txt");
                    let ready = root.join("ready-copy");
                    s.spawn(&["copy".as_ref(), src.as_os_str(), ready.as_os_str()])?;
                }
                19 => {
                    if !root.join("ready-copy").exists() {
                        return Ok(false);
                    }
                    let (files, effect) = clip()?;
                    expect(files == ["ex_copy.txt"] && effect & 2 == 0, "Shell copy on clipboard")?;
                    ctrl_key(&ui, 0, "v");
                }
                20 => {
                    if !idle(&c) || !names(&c, 0).contains(&"ex_copy.txt".to_string()) {
                        return Ok(false);
                    }
                    expect(root.join("explorer-src/ex_copy.txt").exists(), "copy source kept")?;
                    expect(selected(&c, 0) == ["ex_copy.txt"], "pasted item selected")?;
                    s.pass("app Ctrl+V of Shell data object copy (live object in another process)");
                    if let Some(mut probe) = s.probe.take() {
                        let _ = probe.kill();
                        let _ = probe.wait();
                    }
                    let src = root.join("explorer-src/ex_cut.txt");
                    let ready = root.join("ready-cut");
                    s.spawn(&["cut".as_ref(), src.as_os_str(), ready.as_os_str()])?;
                }
                21 => {
                    if !root.join("ready-cut").exists() {
                        return Ok(false);
                    }
                    expect(clip()?.1 == 2, "Shell cut preferred effect MOVE")?;
                    ctrl_key(&ui, 1, "v");
                }
                22 => {
                    if !idle(&c) || !names(&c, 1).contains(&"ex_cut.txt".to_string()) {
                        return Ok(false);
                    }
                    expect(!root.join("explorer-src/ex_cut.txt").exists(), "cut source moved")?;
                    let Some(code) = s.probe_done() else { return Ok(false) };
                    expect(code == 0, "Shell data object received Paste Succeeded=MOVE")?;
                    expect(clip().is_err(), "clipboard emptied after cut-paste")?;
                    s.pass("app Ctrl+V of Shell cut: optimized move, Paste Succeeded reported, clipboard cleared");
                    std::fs::write(root.join("left/external.txt"), b"external").map_err(|e| e.to_string())?;
                }
                23 => {
                    if !names(&c, 0).contains(&"external.txt".to_string()) {
                        return Ok(false);
                    }
                    let current = c.borrow().panes[0].model.selection.borrow().current;
                    expect(current.is_some(), "focus kept after watcher refresh")?;
                    s.pass("folder watcher picked up external file creation");
                    ui.invoke_focus_pane(0);
                    ctrl_key(&ui, 0, "c");
                    let before = clip()?;
                    // Nothing selected after Escape: Ctrl+C must leave the clipboard alone.
                    ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: slint::platform::Key::Escape.into() });
                    ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text: slint::platform::Key::Escape.into() });
                    ctrl_key(&ui, 0, "c");
                    expect(clip()? == before, "Ctrl+C with empty selection keeps clipboard")?;
                    s.pass("empty selection does not overwrite clipboard");
                    // Check.ps1 points the desktop place at left\Folder A in this run's config.
                    ui.invoke_place(0);
                }
                24 => {
                    if !idle(&c) || !c.borrow().data(0).dir.ends_with("Folder A") {
                        return Ok(false);
                    }
                    s.pass("toolbar place opens the configured folder in the active pane");
                    ui.invoke_bookmark_add();
                    let rows = ui.get_bookmarks();
                    expect(rows.row_count() == 1 && rows.row_data(0).unwrap().name == "Folder A", "current folder added as bookmark")?;
                    ui.invoke_bookmark_edit(0);
                    expect(ui.get_bookmark_editing(), "edit dialog opens")?;
                    let path = ui.get_bookmark_path();
                    ui.invoke_bookmark_save(0, "作業".into(), path);
                    expect(!ui.get_bookmark_editing(), "edit dialog closes on save")?;
                    let saved = std::fs::read_to_string(root.join("config.json")).map_err(|e| e.to_string())?;
                    expect(saved.contains("作業") && saved.contains("\"theme\""), "bookmark saved, other settings kept")?;
                    s.pass("bookmark add / rename saved to config.json");
                    // ".." takes the cursor: Home, Down, Up, then Enter opens the parent.
                    let key = |k: slint::platform::Key| {
                        let mut e = slint::language::KeyEvent::default();
                        e.text = k.into();
                        ui.invoke_key_event(0, e);
                    };
                    let on_parent = || c.borrow().panes[0].model.on_parent();
                    ui.invoke_focus_pane(0);
                    key(slint::platform::Key::Home);
                    expect(on_parent() && c.borrow().panes[0].model.row_data(0).unwrap().current, "Home puts the cursor on ..")?;
                    key(slint::platform::Key::DownArrow);
                    expect(!on_parent(), "Down leaves ..")?;
                    key(slint::platform::Key::UpArrow);
                    expect(on_parent(), "Up from the first entry reaches ..")?;
                    key(slint::platform::Key::Return);
                }
                25 => {
                    if !idle(&c) || !c.borrow().data(0).dir.ends_with("left") {
                        return Ok(false);
                    }
                    expect(c.borrow().saved(0).current.is_some_and(|p| p.ends_with("Folder A")), "parent opened with the folder focused")?;
                    s.pass(".. row takes the cursor; Enter on it opens the parent");
                    ui.invoke_open_settings();
                    expect(ui.get_settings_open(), "settings dialog opens")?;
                    let row = ui.get_settings_places().row_data(0).unwrap();
                    expect(row.value.ends_with("Folder A") && row.label == "デスクトップ", "configured desktop shown")?;
                    expect(!ui.get_settings_places().row_data(1).unwrap().fallback.is_empty(), "default shown for unset place")?;
                    ui.invoke_setting_edited(2, r"X:\写真".into());
                    ui.invoke_settings_done(true);
                    expect(!ui.get_settings_open(), "settings dialog closes on save")?;
                    ui.set_split(0.3);
                    ui.set_col_a(0.2);
                    ui.set_quad_y(0.7);
                    ui.invoke_layout_committed();
                    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                    expect(saved["places"]["pictures"] == r"X:\写真", "picture place saved")?;
                    expect(saved["places"]["desktop"].as_str().is_some_and(|d| d.ends_with("Folder A")), "other places kept")?;
                    expect((saved["split"].as_f64().unwrap_or(0.) - 0.3).abs() < 1e-6, "split saved")?;
                    expect(saved["split3_columns"][0] == 0.2 && saved["split4"][1] == 0.7, "3/4-pane splitter positions saved")?;
                    s.pass("settings dialog saves places; splitter position saved");
                }
                26 => {
                    // Drives arrive from the drive thread and are listed after the entries.
                    let (rows, offset, entries) = {
                        let c = c.borrow();
                        let data = c.data(0);
                        (c.panes[0].model.total(), data.offset(), data.rows.len())
                    };
                    if rows <= offset + entries {
                        return Ok(false);
                    }
                    let drive = c.borrow().panes[0].model.row_at(offset + entries).unwrap();
                    expect(drive.drive && drive.name.contains(':'), "drive row after the entries")?;
                    s.pass("drive list shown under the entries");
                    ui.invoke_focus_pane(0);
                    fkey(&ui, 0, slint::platform::Key::F7);
                    expect(ui.get_name_open() && ui.get_name_initial() == "新しいフォルダー", "F7 opens the new-folder dialog")?;
                    ui.invoke_name_done(true, "新規 A".into());
                }
                27 => {
                    if !idle(&c) || !names(&c, 0).contains(&"新規 A".to_string()) {
                        return Ok(false);
                    }
                    expect(root.join("left/新規 A").is_dir() && selected(&c, 0) == ["新規 A"], "folder created and selected")?;
                    ctrl_key(&ui, 0, "n");
                    expect(ui.get_name_initial() == "新しいテキスト ドキュメント.txt", "Ctrl+N opens the new-file dialog")?;
                    ui.invoke_name_done(true, "memo.txt".into());
                }
                28 => {
                    if !idle(&c) || !names(&c, 0).contains(&"memo.txt".to_string()) {
                        return Ok(false);
                    }
                    expect(root.join("left/memo.txt").is_file(), "file created")?;
                    fkey(&ui, 0, slint::platform::Key::F2);
                    expect(ui.get_name_initial() == "memo.txt" && ui.get_name_stem() == 4, "F2 opens rename with the stem selected")?;
                    expect(!ui.invoke_name_validate("A.TXT".into()).is_empty(), "rename to an existing name rejected")?;
                    ui.invoke_name_done(true, "memo2.txt".into());
                }
                29 => {
                    if !idle(&c) || !names(&c, 0).contains(&"memo2.txt".to_string()) {
                        return Ok(false);
                    }
                    expect(!root.join("left/memo.txt").exists() && selected(&c, 0) == ["memo2.txt"], "renamed and selected")?;
                    s.pass("F7 new folder, Ctrl+N new file, F2 rename");
                    let hidden = root.join("left/b.txt");
                    std::process::Command::new("attrib").arg("+h").arg(shell::shell_path(&hidden)).status().map_err(|e| e.to_string())?;
                    fkey(&ui, 0, slint::platform::Key::F5);
                }
                30 => {
                    if !idle(&c) || names(&c, 0).contains(&"b.txt".to_string()) {
                        return Ok(false);
                    }
                    let mut e = slint::language::KeyEvent::default();
                    e.text = "z".into();
                    e.modifiers.alt = true;
                    ui.invoke_key_event(0, e);
                }
                31 => {
                    if !idle(&c) || !names(&c, 0).contains(&"b.txt".to_string()) {
                        return Ok(false);
                    }
                    s.pass("hidden files hidden by default, Alt+Z shows them");
                    ui.invoke_sort(0, 3);
                }
                32 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    let data = c.borrow().data(0);
                    let exts: Vec<String> = data.rows.iter().filter(|e| !e.directory).map(|e| e.ext.to_lowercase()).collect();
                    let mut sorted = exts.clone();
                    sorted.sort();
                    expect(exts == sorted, "files ordered by extension")?;
                    s.pass("sort by extension");
                }
                33 => {
                    ui.invoke_navigate(0, display(&root.join("many")).into());
                }
                34 => {
                    if !idle(&c) || !c.borrow().data(0).dir.ends_with("many") {
                        return Ok(false);
                    }
                    let model = c.borrow().panes[0].model.clone();
                    let size = model.page_size.get();
                    expect(size > 3 && model.row_count() == size && model.pages().1 > 1, "one page of rows, several pages")?;
                    // Default extension colors: .txt is colored.
                    expect(model.row_data(1).is_some_and(|r| r.tinted), "txt rows get the extension color")?;
                    ui.invoke_focus_pane(0);
                    fkey(&ui, 0, slint::platform::Key::DownArrow);
                    let before = c.borrow().cursor_row(0);
                    fkey(&ui, 0, slint::platform::Key::RightArrow);
                    expect(model.page_start.get() == size && c.borrow().cursor_row(0) == before + size, "Right: next page, same position")?;
                    fkey(&ui, 0, slint::platform::Key::LeftArrow);
                    expect(model.page_start.get() == 0 && c.borrow().cursor_row(0) == before, "Left: previous page")?;
                    ui.invoke_page_step(0, true);
                    expect(model.page_start.get() == size, "wheel turns a page")?;
                    expect(ui.get_p0_info().page.starts_with("2/"), "page number shown")?;
                    s.pass("paged list: Right/Left and wheel turn pages, page number shown");
                    ui.invoke_open_settings();
                    ui.set_settings_tab(1);
                }
                35 => {
                    if s.held.is_none() && !s.hold.is_zero() {
                        // Tells a screenshot script the settings dialog is on screen.
                        let _ = std::fs::write(root.join("holding-settings"), b"");
                    }
                    if s.held.get_or_insert_with(Instant::now).elapsed() < s.hold {
                        return Ok(false);
                    }
                    expect(ui.get_settings_colors().row_count() == theme::BASE.len(), "base colors listed")?;
                    expect(ui.get_settings_ext().row_count() == theme::EXTENSIONS.len(), "default extension rules listed")?;
                    let red = slint::Color::from_rgb_u8(255, 0, 0);
                    ui.invoke_color_base(1, red);
                    ui.invoke_color_ext(100000, "foo; .BAR".into(), red);
                    ui.invoke_font_set("Yu Gothic UI".into(), 16);
                    ui.invoke_settings_done(true);
                    expect(ui.global::<Theme>().get_text() == red, "base color applied")?;
                    expect(ui.get_font_family() == "Yu Gothic UI" && ui.get_font_size() == 16.0, "font applied")?;
                    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                    expect(saved["colors"]["text"] == "#ff0000", "base color saved")?;
                    expect(saved["ext_colors"].as_array().is_some_and(|r| r.iter().any(|x| x["exts"] == "FOO;BAR")), "extension rule saved")?;
                    expect(saved["font"]["family"] == "Yu Gothic UI" && saved["font"]["size"] == 16.0, "font saved")?;
                    s.pass("settings: colors, extension rules and font applied and saved");
                }
                36 => {
                    let config = || -> Result<serde_json::Value, String> {
                        serde_json::from_slice(&std::fs::read(root.join("config.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
                    };
                    expect(config()?["show_hidden"] == serde_json::json!([true, false, false, false]), "hidden files saved per pane")?;
                    s.pass("Alt+Z is per pane");
                    // Ctrl+Shift+S: size order, again for reverse.
                    let sort_key = |letter: &str| {
                        let mut e = slint::language::KeyEvent::default();
                        e.text = letter.into();
                        e.modifiers.control = true;
                        e.modifiers.shift = true;
                        ui.invoke_key_event(0, e);
                    };
                    sort_key("S");
                    expect(c.borrow().panes[0].column == 1 && !c.borrow().panes[0].descending, "Ctrl+Shift+S sorts by size")?;
                    sort_key("S");
                    expect(c.borrow().panes[0].descending, "second Ctrl+Shift+S reverses")?;
                    s.pass("sort shortcuts with reverse on repeat");
                    ui.invoke_copy_path(0);
                    ui.invoke_open_settings();
                    ui.invoke_setting_edited(5, display(&root.join("left")).into());
                    ui.set_settings_opacity(80);
                    ui.invoke_settings_done(true);
                    let saved = config()?;
                    expect(saved["opacity"] == 0.8, "opacity saved")?;
                    expect(saved["start"]["left"].as_str().is_some_and(|l| l.ends_with("left")), "start folder saved")?;
                    let layered = {
                        use windows::Win32::UI::WindowsAndMessaging::*;
                        let h = hwnd(&ui).ok_or("no window")?;
                        let style = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
                        let (mut alpha, mut flags) = (0u8, LAYERED_WINDOW_ATTRIBUTES_FLAGS(0));
                        let _ = unsafe { GetLayeredWindowAttributes(h, None, Some(&mut alpha), Some(&mut flags)) };
                        style & WS_EX_LAYERED.0 != 0 && alpha == 204
                    };
                    expect(layered, "window opacity applied (80 %)")?;
                    s.pass("settings: start folder and window opacity saved and applied");
                    // Shift+letter: the drive holding the test data.
                    let letter = display(&root).chars().next().unwrap_or('C').to_string();
                    let mut e = slint::language::KeyEvent::default();
                    e.text = letter.to_uppercase().into();
                    e.modifiers.shift = true;
                    ui.invoke_key_event(1, e);
                }
                37 => {
                    if !idle(&c) || c.borrow().data(1).dir.parent().is_some() {
                        return Ok(false);
                    }
                    s.pass("Shift+letter opens that drive");
                    let out = std::process::Command::new("powershell")
                        .args(["-NoProfile", "-Command", "Get-Clipboard"])
                        .output()
                        .map_err(|e| e.to_string())?;
                    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    expect(text == display(&c.borrow().data(0).dir), "path bar copy puts the path on the clipboard")?;
                    s.pass("path copy");
                }
                38 => {
                    // Edit menu, in the 150-file folder of pane 0.
                    ui.invoke_focus_pane(0);
                    c.borrow_mut().active(0);
                    ui.invoke_menu_command(8);
                    expect(c.borrow().panes[0].model.selection.borrow().count() == 150, "same extension selects all .txt")?;
                    ui.invoke_menu_command(7);
                    let out = std::process::Command::new("powershell")
                        .args(["-NoProfile", "-Command", "(Get-Clipboard).Count"])
                        .output()
                        .map_err(|e| e.to_string())?;
                    expect(String::from_utf8_lossy(&out.stdout).trim() == "150", "file names copied one per line")?;
                    s.pass("edit menu: select same extension, copy file names");
                    for (command, name) in [(20, "size"), (21, "date"), (22, "time"), (23, "attr"), (24, "kind")] {
                        ui.invoke_menu_command(command);
                        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                        expect(saved["columns"][name] == false, "column hidden and saved")?;
                        ui.invoke_menu_command(command);
                    }
                    let columns = ui.get_columns();
                    expect(columns.size && columns.date && columns.time && columns.attr && columns.kind, "columns shown again")?;
                    // 3 panes: Tab visits all three; 4 panes; back to 2 with the focus kept valid.
                    ui.invoke_menu_command(11);
                    expect(ui.get_pane_count() == 3, "3-pane layout")?;
                    let tab = |p: i32| {
                        let mut e = slint::language::KeyEvent::default();
                        e.text = slint::platform::Key::Tab.into();
                        ui.invoke_key_event(p, e);
                    };
                    tab(0);
                    tab(1);
                    expect(c.borrow().active == 2, "Tab reaches the third pane")?;
                    tab(2);
                    expect(c.borrow().active == 0, "Tab wraps to the first pane")?;
                    ui.invoke_menu_command(12);
                    tab(0);
                    tab(1);
                    tab(2);
                    expect(c.borrow().active == 3, "Tab reaches the fourth pane")?;
                    // Ctrl+Shift+3: stacked, again: side by side; Ctrl+Shift+4; Ctrl+Shift+2 (JP keyboard symbols).
                    let layout_key = |text: &str| {
                        let mut e = slint::language::KeyEvent::default();
                        e.text = text.into();
                        e.modifiers.control = true;
                        e.modifiers.shift = true;
                        ui.invoke_key_event(0, e);
                    };
                    layout_key("#");
                    expect(ui.get_pane_count() == 3 && ui.get_three_columns(), "Ctrl+Shift+3: side by side")?;
                    layout_key("#");
                    expect(ui.get_pane_count() == 3 && !ui.get_three_columns(), "Ctrl+Shift+3 again: stacked")?;
                    layout_key("#");
                    expect(ui.get_three_columns(), "third time: side by side again")?;
                    layout_key("$");
                    expect(ui.get_pane_count() == 4, "Ctrl+Shift+4")?;
                    layout_key("\"");
                    expect(ui.get_pane_count() == 2 && c.borrow().active == 0, "Ctrl+Shift+2: back to 2 panes, focus moved to a shown pane")?;
                    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                    expect(saved["panes"] == 2, "pane count saved")?;
                    s.pass("column switches; 3 and 4 pane layouts with Tab");
                }
                39 => return Err("done".into()),
                _ => {}
            }
            Ok(true)
        })();
        match result {
            Ok(true) => s.step += 1,
            Ok(false) => {}
            Err(e) if e == "done" => {
                finish(&mut s, None);
                s.step = usize::MAX;
            }
            Err(e) => {
                finish(&mut s, Some(e));
                s.step = usize::MAX;
            }
        }
    });
    timer
}
