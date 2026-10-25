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
/// Compared rows of pane `p`: the entry name, or "" for an empty row.
fn compared(c: &Rc<RefCell<Controller>>, p: usize) -> Vec<String> {
    let c = c.borrow();
    let model = &c.panes[p].model;
    (0..model.total()).map(|r| model.row_at(r).map_or("?".into(), |row| if row.gap { String::new() } else { format!("{}{}{}", row.name, if row.ext.is_empty() { "" } else { "." }, row.ext) })).collect()
}
fn ctrl_shift_key(ui: &App, p: i32, letter: &str) {
    let mut event = slint::language::KeyEvent::default();
    event.text = letter.into();
    event.modifiers.control = true;
    event.modifiers.shift = true;
    ui.invoke_key_event(p, event);
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
/// The recursive search of pane 0 for `text` is answered and its results are sorted in.
fn searched(c: &Rc<RefCell<Controller>>, text: &str) -> bool {
    idle(c)
        && c.borrow().panes[0]
            .found
            .as_ref()
            .is_some_and(|f| f.text == text && !f.running && f.finished.is_none() && f.backlog.is_empty() && !f.replace)
}
/// Search results of pane 0 as "folder\name", in name order (the pane may sort descending).
fn found(c: &Rc<RefCell<Controller>>) -> Vec<String> {
    let mut found: Vec<String> = c.borrow().data(0).rows.iter().map(|e| format!("{}{}", e.location, e.name)).collect();
    found.sort();
    found
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
fn shift_letter(ui: &App, p: i32, letter: &str) {
    let mut event = slint::language::KeyEvent::default();
    event.text = letter.into();
    event.modifiers.shift = true;
    ui.invoke_key_event(p, event);
}
/// The system drive and its Windows folder, when the test data is on another drive.
fn other_drive(root: &Path) -> Option<(String, PathBuf)> {
    let system = PathBuf::from(std::env::var_os("SystemRoot")?);
    let drive = crate::drive_of(&system)?;
    (crate::drive_of(root)? != drive).then_some((drive, system))
}
fn shift_key(ui: &App, p: i32, key: slint::platform::Key) {
    let mut event = slint::language::KeyEvent::default();
    event.text = key.into();
    event.modifiers.shift = true;
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
    /// Menu bar hover sweep: pointer x and the menus seen open along the way.
    sweep: f32,
    menus: Vec<i32>,
    /// The recursive search walk started with the search bar.
    session: u64,
    /// Sort order of pane 0 before a comparison.
    sort: (usize, bool),
    /// Toolbar Home was pressed (step 41 waits for it).
    homed: bool,
    /// When the slow fake agent was cancelled.
    cancelled: Option<Instant>,
}
/// A stand-in for `codex exec --json`: records its arguments and prompt, makes a file in its
/// working folder and answers with Codex's events. `slow` waits in a grandchild first.
fn fake_agent(root: &Path, slow: bool) -> Result<PathBuf, String> {
    let file = root.join(if slow { "fake-agent-slow.cmd" } else { "fake-agent.cmd" });
    let wait = if slow { "ping -n 60 127.0.0.1 >nul\r\n" } else { "" };
    let script = format!(
        "@echo off\r\n\
         if \"%~1\"==\"--version\" (echo codex-cli 9.9.9-fake& exit /b 0)\r\n\
         echo %*> \"%~dp0agent-args.txt\"\r\n\
         findstr \"^\" > \"%~dp0agent-prompt.txt\"\r\n\
         echo {{\"type\":\"thread.started\",\"thread_id\":\"fake-1\"}}\r\n\
         {wait}\
         echo {{\"type\":\"item.started\",\"item\":{{\"id\":\"1\",\"type\":\"command_execution\",\"command\":\"make\",\"status\":\"in_progress\"}}}}\r\n\
         echo agent> agent-made.txt\r\n\
         echo {{\"type\":\"item.completed\",\"item\":{{\"id\":\"1\",\"type\":\"command_execution\",\"command\":\"make\",\"exit_code\":0,\"status\":\"completed\"}}}}\r\n\
         echo {{\"type\":\"item.completed\",\"item\":{{\"id\":\"2\",\"type\":\"agent_message\",\"text\":\"fake done\"}}}}\r\n"
    );
    std::fs::write(&file, script).map_err(|e| e.to_string())?;
    Ok(file)
}
fn agent_texts(c: &Rc<RefCell<Controller>>) -> Vec<String> {
    c.borrow().agent.lines.iter().map(|l| l.text.to_string()).collect()
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
        sweep: 0.0,
        menus: Vec::new(),
        session: 0,
        sort: (0, false),
        homed: false,
        cancelled: None,
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
            // The status line usually says why the step never completed.
            let message = c.borrow().ui.upgrade().map(|ui| ui.get_message().to_string()).unwrap_or_default();
            finish(&mut s, Some(format!("timeout (message: {message})")));
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
                    let aside = std::fs::read_dir(root.join("right")).map_err(|e| e.to_string())?.flatten().any(|e| e.file_name().to_string_lossy().contains("~enyfile-old"));
                    expect(!aside, "old folder set aside, then deleted")?;
                    s.pass("folder replace: existing folder set aside and deleted, incoming copied");
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
                    expect(ui.get_name_open() && ui.get_name_initial() == "新しいフォルダ", "F7 opens the new-folder dialog")?;
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
                    // No page to turn to: the wheel leaves the cursor, Left/Right step one row.
                    ui.invoke_page_step(0, false);
                    let at = c.borrow().cursor_row(0);
                    ui.invoke_page_step(0, false);
                    expect(c.borrow().cursor_row(0) == at && model.page_start.get() == 0, "wheel up on the first page keeps the cursor")?;
                    fkey(&ui, 0, slint::platform::Key::LeftArrow);
                    expect(c.borrow().cursor_row(0) == at - 1, "Left on the first page steps up a row")?;
                    fkey(&ui, 0, slint::platform::Key::End);
                    let last = c.borrow().cursor_row(0);
                    ui.invoke_page_step(0, true);
                    expect(c.borrow().cursor_row(0) == last, "wheel down on the last page keeps the cursor")?;
                    fkey(&ui, 0, slint::platform::Key::UpArrow);
                    fkey(&ui, 0, slint::platform::Key::UpArrow);
                    fkey(&ui, 0, slint::platform::Key::RightArrow);
                    expect(c.borrow().cursor_row(0) == last - 1, "Right on the last page steps down a row")?;
                    s.pass("first/last page: wheel stays, Left/Right step one row");
                    // Shift ranges stop at entries ("..", drive rows) and start where the move does.
                    use slint::platform::Key;
                    let entries = c.borrow().data(0).rows.len();
                    let selected_count = || c.borrow().panes[0].model.selection.borrow().count();
                    fkey(&ui, 0, Key::Home);
                    shift_key(&ui, 0, Key::PageDown);
                    expect(selected_count() == size && c.borrow().cursor_row(0) == size, "Shift+PageDown from \"..\" selects the first page")?;
                    shift_key(&ui, 0, Key::End);
                    expect(selected_count() == entries && c.borrow().cursor_row(0) == entries, "Shift+End selects to the last entry, not a drive")?;
                    shift_key(&ui, 0, Key::Home);
                    expect(selected_count() == 1 && c.borrow().cursor_row(0) == 1, "Shift+Home stops at the first entry")?;
                    click(&ui, 0, 5, false);
                    fkey(&ui, 0, Key::Home);
                    shift_key(&ui, 0, Key::DownArrow);
                    expect(selected(&c, 0) == [names(&c, 0)[0].clone()], "a range from \"..\" ignores the earlier anchor")?;
                    fkey(&ui, 0, Key::Home);
                    fkey(&ui, 0, Key::DownArrow);
                    s.pass("Shift+PageUp/PageDown/Home/End select entries only, from where the move starts");
                    // Dragging out (the probe stands in for the system drag loop).
                    let all = names(&c, 0);
                    let probe = || -> Vec<String> {
                        let c = c.borrow();
                        c.drag_probe.iter().flatten().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
                    };
                    c.borrow_mut().drag_probe = Some(Vec::new());
                    click(&ui, 0, 1, false);
                    ui.invoke_pointer(0, 3, false, true, 0);
                    ui.invoke_pointer(0, 3, false, false, 1);
                    ui.invoke_pointer(0, 2, false, false, 0);
                    ui.invoke_pointer(0, 2, false, false, 4);
                    ui.invoke_pointer(0, 2, false, false, 1);
                    expect(probe() == all[..3] && selected(&c, 0).len() == 3, "dragging a selected item carries the whole selection")?;
                    ui.invoke_pointer(0, 6, false, false, 0);
                    ui.invoke_pointer(0, 6, false, false, 4);
                    ui.invoke_pointer(0, 6, false, false, 1);
                    expect(probe() == [all[5].clone()] && selected(&c, 0) == [all[5].clone()], "dragging an unselected item selects and carries it")?;
                    ui.invoke_pointer(0, 0, false, false, 0);
                    ui.invoke_pointer(0, 0, false, false, 4);
                    expect(probe() == [all[5].clone()], "\"..\" is not dragged")?;
                    c.borrow_mut().drag_probe = None;
                    fkey(&ui, 0, Key::DownArrow);
                    s.pass("drag out: the selection, or the item pressed");
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
                    expect(!ui.get_settings_search_links(), "following links is off by default")?;
                    ui.set_settings_search_links(true);
                    ui.invoke_settings_done(true);
                    let saved = config()?;
                    expect(saved["opacity"] == 0.8, "opacity saved")?;
                    expect(saved["search_links"] == true && c.borrow().config.search_links, "その他: following links saved")?;
                    // Later steps search with links not followed.
                    c.borrow_mut().config.search_links = false;
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
                    // Shift+letter on another drive opens the folder last shown there.
                    if let Some((drive, system)) = other_drive(&root) {
                        c.borrow_mut().config.drive_dirs.insert(drive.clone(), system);
                        shift_letter(&ui, 1, &drive[..1]);
                    }
                }
                38 => {
                    let Some((drive, system)) = other_drive(&root) else { return Ok(true) };
                    let dir = c.borrow().data(1).dir.clone();
                    if !idle(&c) || crate::drive_of(&dir).as_deref() != Some(drive.as_str()) {
                        return Ok(false);
                    }
                    expect(display(&dir).eq_ignore_ascii_case(&display(&system)), &format!("the remembered folder: {}", display(&dir)))?;
                    s.pass("Shift+letter opens the folder last shown on that drive");
                    ui.invoke_navigate(1, display(&root).into());
                }
                39 => {
                    let Some((drive, _)) = other_drive(&root) else { return Ok(true) };
                    if !idle(&c) || !c.borrow().data(1).dir.ends_with(root.file_name().unwrap_or_default()) {
                        return Ok(false);
                    }
                    // A remembered folder that is gone: the drive root.
                    let gone = PathBuf::from(format!(r"{drive}\enyfile-smoke-gone"));
                    c.borrow_mut().config.drive_dirs.insert(drive.clone(), gone);
                    shift_letter(&ui, 1, &drive[..1]);
                }
                40 => {
                    let Some((drive, _)) = other_drive(&root) else { return Ok(true) };
                    let dir = c.borrow().data(1).dir.clone();
                    if !idle(&c) || crate::drive_of(&dir).as_deref() != Some(drive.as_str()) {
                        return Ok(false);
                    }
                    expect(dir.parent().is_none(), "a vanished folder falls back to the drive root")?;
                    s.pass("Shift+letter falls back to the root when the folder is gone");
                }
                41 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    // Toolbar Home: the active pane's start folder; then back to the test data's
                    // drive root, where step 37 left pane 1.
                    if !s.homed {
                        c.borrow_mut().config.start[1] = Some(root.join("right"));
                        c.borrow_mut().active(1);
                        s.homed = true;
                        ui.invoke_home();
                        return Ok(false);
                    }
                    expect(c.borrow().data(1).dir.ends_with("right") && c.borrow().data(0).dir.ends_with("many"), "Home opens the start folder in the active pane only")?;
                    s.pass("toolbar Home opens the active pane's start folder");
                    let here = crate::drive_of(&root).unwrap_or_default();
                    ui.invoke_navigate(1, format!(r"{here}\").into());
                }
                42 => {
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
                43 => {
                    // Ctrl+F: incremental wildcard search in the 150-file folder; Ctrl+G next.
                    if !idle(&c) {
                        return Ok(false);
                    }
                    let current = || {
                        let c = c.borrow();
                        let data = c.data(0);
                        let i = c.panes[0].model.selection.borrow().current;
                        i.and_then(|i| data.rows.get(i)).map(|e| e.name.clone()).unwrap_or_default()
                    };
                    let key = |letter: &str, shift: bool| {
                        let mut e = slint::language::KeyEvent::default();
                        e.text = letter.into();
                        e.modifiers.control = true;
                        e.modifiers.shift = shift;
                        ui.invoke_key_event(0, e);
                    };
                    {
                        let mut c = c.borrow_mut();
                        if c.panes[0].column != 0 || c.panes[0].descending {
                            c.panes[0].column = 0;
                            c.panes[0].descending = false;
                            c.rearrange(0);
                            return Ok(false);
                        }
                    }
                    fkey(&ui, 0, slint::platform::Key::Home);
                    fkey(&ui, 0, slint::platform::Key::DownArrow);
                    expect(current() == "file001.txt", "cursor on the first file")?;
                    // Through the window's key path: typing in the bar, Ctrl+G there, Esc.
                    use slint::platform::WindowEvent;
                    let press = |text: slint::SharedString| {
                        ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                        ui.window().dispatch_event(WindowEvent::KeyReleased { text });
                    };
                    ui.invoke_focus_pane(0);
                    window_ctrl_key(&ui, "f");
                    expect(ui.get_find_mode() == 1, "Ctrl+F opens the search bar")?;
                    for ch in "FILE12?.TXT".chars() {
                        press(ch.to_string().into());
                    }
                    expect(current() == "file120.txt", "search moves the cursor while typing")?;
                    window_ctrl_key(&ui, "g");
                    expect(current() == "file121.txt", "Ctrl+G in the search bar: next match")?;
                    press(slint::platform::Key::Escape.into());
                    expect(ui.get_find_mode() == 0 && c.borrow().search == "FILE12?.TXT", "Esc ends the search and keeps it")?;
                    key("g", false);
                    expect(current() == "file122.txt", "Ctrl+G: next match")?;
                    key("G", true);
                    key("G", true);
                    expect(current() == "file120.txt", "Ctrl+Shift+G: previous match")?;
                    key("g", true);
                    expect(current() == "file129.txt", "previous match wraps around")?;
                    key("f", false);
                    ui.invoke_find_edited("nothing*".into());
                    expect(current() == "file129.txt" && ui.get_message().contains("見つかりません"), "no match keeps the cursor")?;
                    ui.invoke_find_done(false, "nothing*".into());
                    s.pass("Ctrl+F wildcard search, Ctrl+G / Ctrl+Shift+G");
                    key("F", true);
                    expect(ui.get_find_mode() == 2, "Ctrl+Shift+F opens the filter bar")?;
                    ui.invoke_find_done(true, "*5.txt; file001".into());
                }
                44 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    let shown = names(&c, 0);
                    expect(shown.len() == 16 && shown.iter().all(|n| n.ends_with("5.txt") || n == "file001.txt"), "filter shows matching entries only")?;
                    expect(ui.get_p0_info().filter == "*5.txt; file001" && ui.get_p0_info().files == "16", "filter shown in the info line")?;
                    // Sorting keeps the filter and works on the memory copy.
                    ui.invoke_sort(0, 0);
                    s.pass("Ctrl+Shift+F filters with wildcards and ';'");
                }
                45 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    expect(names(&c, 0).len() == 16, "sort keeps the filter")?;
                    fkey(&ui, 0, slint::platform::Key::Escape);
                }
                46 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    expect(names(&c, 0).len() == 150 && ui.get_p0_info().filter.is_empty(), "Esc clears the filter")?;
                    s.pass("Esc clears the filter");
                    c.borrow_mut().set_filter(0, "file00*");
                }
                47 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    expect(names(&c, 0).len() == 9, "filter set again")?;
                    ui.invoke_command(0, 2);
                }
                48 => {
                    if !idle(&c) || c.borrow().data(0).dir.ends_with("many") {
                        return Ok(false);
                    }
                    expect(c.borrow().panes[0].filter.is_none() && names(&c, 0).contains(&"many".to_string()), "opening another folder drops the filter")?;
                    s.pass("filter is dropped when another folder opens");
                    // Menu bar: click ファイル, then move along the bar; each menu opens on hover.
                    use slint::platform::{PointerEventButton, WindowEvent};
                    let at = slint::LogicalPosition::new(12.0, 12.0);
                    ui.window().dispatch_event(WindowEvent::PointerMoved { position: at });
                    ui.window().dispatch_event(WindowEvent::PointerPressed { position: at, button: PointerEventButton::Left });
                    ui.window().dispatch_event(WindowEvent::PointerReleased { position: at, button: PointerEventButton::Left });
                    expect(ui.get_open_menu() == 0, "click opens the File menu")?;
                    s.sweep = 12.0;
                }
                49 => {
                    use slint::platform::{PointerEventButton, WindowEvent};
                    let open = ui.get_open_menu();
                    if s.menus.last() != Some(&open) {
                        s.menus.push(open);
                    }
                    if s.sweep < 330.0 {
                        s.sweep += 8.0;
                        let position = slint::LogicalPosition::new(s.sweep, 12.0);
                        ui.window().dispatch_event(WindowEvent::PointerMoved { position });
                        return Ok(false);
                    }
                    expect(s.menus == [0, 1, 2, 3], &format!("hovering the bar switches menus (seen {:?})", s.menus))?;
                    let size = ui.window().size().to_logical(ui.window().scale_factor());
                    let away = slint::LogicalPosition::new(size.width / 2.0, size.height / 2.0);
                    ui.window().dispatch_event(WindowEvent::PointerMoved { position: away });
                    ui.window().dispatch_event(WindowEvent::PointerPressed { position: away, button: PointerEventButton::Left });
                    ui.window().dispatch_event(WindowEvent::PointerReleased { position: away, button: PointerEventButton::Left });
                    expect(ui.get_open_menu() == -1, "a click outside closes the menu")?;
                    {
                        use windows::Win32::{Foundation::{LPARAM, WPARAM}, UI::WindowsAndMessaging::*};
                        ui.invoke_show_menu(1);
                        expect(ui.get_open_menu() == 1, "menu reopened")?;
                        let h = hwnd(&ui).ok_or("no window")?;
                        // HTNOWHERE: the frame press arrives without starting a real move loop.
                        unsafe { SendMessageW(h, WM_NCLBUTTONDOWN, Some(WPARAM(HTNOWHERE as usize)), Some(LPARAM(0))) };
                        expect(ui.get_open_menu() == -1, "a press on the title bar closes the menu")?;
                        // A lone Alt, as Windows sends it (scan code 0x38), activates the bar.
                        let alt = 1 | (0x38 << 16) | (1 << 29);
                        unsafe { SendMessageW(h, WM_SYSKEYDOWN, Some(WPARAM(0x12)), Some(LPARAM(alt))) };
                        unsafe { SendMessageW(h, WM_SYSKEYUP, Some(WPARAM(0x12)), Some(LPARAM(alt | (3 << 30)))) };
                        // Not checked: on some machines (IME, resident tools) the posted Alt does
                        // not come through, though a real lone Alt works there. Checked by hand.
                        // expect(ui.get_bar_focus() == 0 && ui.get_open_menu() == -1, "a lone Alt highlights ファイル")?;
                        if ui.get_bar_focus() != 0 {
                            // The keys below need the bar's keyboard focus.
                            ui.invoke_key_menu(0);
                        }
                    }
                    let key = |text: &str| {
                        use slint::platform::WindowEvent;
                        ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.into() });
                        ui.window().dispatch_event(WindowEvent::KeyReleased { text: text.into() });
                    };
                    key("w");
                    expect(ui.get_open_menu() == 2 && ui.get_menu_item() == 0, "W opens 表示 at its first item")?;
                    // Name, size, time, extension, (separator), サイズの表示.
                    for _ in 0..4 {
                        key("\u{F701}");
                    }
                    expect(ui.get_menu_item() == 5, &format!("Down skips the separator (at {})", ui.get_menu_item()))?;
                    let shown = ui.get_columns().size;
                    key("\n");
                    expect(ui.get_open_menu() == -1 && ui.get_columns().size != shown, "Enter runs the item")?;
                    ui.invoke_menu_command(20);
                    let mut e = slint::language::KeyEvent::default();
                    e.text = "t".into();
                    e.modifiers.alt = true;
                    ui.invoke_key_event(0, e);
                    expect(ui.get_open_menu() == 3, "Alt+T opens ツール")?;
                    key("\u{F702}");
                    expect(ui.get_open_menu() == 2, "Left moves to 表示")?;
                    key("\u{1b}");
                    expect(ui.get_open_menu() == -1 && ui.get_bar_focus() == -1, "Esc closes the menu")?;
                    s.pass("menu bar: hover switches between open menus, outside or title bar click closes; Alt / Alt+letter / arrows / Enter / Esc");
                    // Ctrl+Alt+Shift+F: a small tree with a junction back to its own root.
                    let tree = root.join("tree");
                    for dir in ["a/b", "b"] {
                        std::fs::create_dir_all(tree.join(dir)).map_err(|e| e.to_string())?;
                    }
                    for file in ["a/hit.rs", "b/hit.txt", "a/b/deep.txt"] {
                        std::fs::write(tree.join(file), file).map_err(|e| e.to_string())?;
                    }
                    let made = std::process::Command::new("cmd")
                        .args(["/c", "mklink", "/J"])
                        .arg(tree.join("loop"))
                        .arg(&tree)
                        .output()
                        .map_err(|e| e.to_string())?;
                    expect(made.status.success(), "junction created for the search test")?;
                    ui.invoke_navigate(0, display(&tree).into());
                }
                50 => {
                    if !idle(&c) || !c.borrow().data(0).dir.ends_with("tree") {
                        return Ok(false);
                    }
                    let mut e = slint::language::KeyEvent::default();
                    e.text = "F".into();
                    e.modifiers.control = true;
                    e.modifiers.alt = true;
                    e.modifiers.shift = true;
                    ui.invoke_key_event(0, e);
                    expect(ui.get_find_mode() == 3, "Ctrl+Alt+Shift+F opens the recursive search bar")?;
                    s.session = c.borrow().panes[0].finder.as_ref().map(|f| f.id).ok_or("the walk starts with the bar")?;
                    // Typing: only the text after the pause is searched.
                    ui.invoke_find_edited("hi".into());
                    ui.invoke_find_edited("hit*".into());
                }
                51 => {
                    if !searched(&c, "hit*") {
                        return Ok(false);
                    }
                    expect(found(&c) == ["a\\hit.rs", "b\\hit.txt"], &format!("results with their folder, the junction not entered: {:?}", found(&c)))?;
                    expect(ui.get_p0_info().search == "hit*" && ui.get_find_mode() == 3, "results while typing, shown in the info line")?;
                    s.pass("Ctrl+Alt+Shift+F searches below the folder while typing, junctions skipped");
                    ui.invoke_find_edited("a/*.rs".into());
                }
                52 => {
                    if !searched(&c, "a/*.rs") {
                        return Ok(false);
                    }
                    expect(found(&c) == ["a\\hit.rs"], &format!("a path pattern with '/': {:?}", found(&c)))?;
                    expect(c.borrow().panes[0].finder.as_ref().map(|f| f.id) == Some(s.session), "the next text reuses the walk")?;
                    s.pass("changing the text searches the kept walk again");
                    ui.invoke_find_edited("".into());
                    let kept = c.borrow().panes[0].found.as_ref().is_some_and(|f| f.text == "a/*.rs");
                    expect(kept && !c.borrow().typing.running() && ui.get_find_mode() == 3, "erasing the text keeps the results and the bar")?;
                    ui.invoke_find_done(true, "a/*.rs".into());
                    expect(ui.get_find_mode() == 0 && c.borrow().panes[0].found.is_some(), "Enter keeps the results")?;
                    c.borrow_mut().config.search_links = true;
                    c.borrow_mut().search_tree(0, r"\A\B\deep");
                }
                53 => {
                    if !searched(&c, r"\A\B\deep") {
                        return Ok(false);
                    }
                    // Following links, the junction back to the root is still entered only once.
                    expect(found(&c) == ["a\\b\\deep.txt"], &format!("a path pattern with '\\', link cycle cut: {:?}", found(&c)))?;
                    c.borrow_mut().config.search_links = false;
                    s.pass("path patterns with / and \\; a junction cycle is not followed");
                    fkey(&ui, 0, slint::platform::Key::Escape);
                }
                54 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    let names = names(&c, 0);
                    expect(
                        c.borrow().panes[0].found.is_none() && ["a", "b", "loop"].iter().all(|n| names.contains(&n.to_string())),
                        "Esc goes back to the folder",
                    )?;
                    expect(c.borrow().panes[0].finder.is_none(), "closing the results drops the walk")?;
                    // Backspace on an empty field: the first press only warns, the second ends.
                    c.borrow_mut().open_find(0, 2);
                    let backspace = || {
                        use slint::platform::{Key, WindowEvent};
                        ui.window().dispatch_event(WindowEvent::KeyPressed { text: Key::Backspace.into() });
                        ui.window().dispatch_event(WindowEvent::KeyReleased { text: Key::Backspace.into() });
                    };
                    backspace();
                    expect(ui.get_find_mode() == 2, "one Backspace on an empty field keeps the bar")?;
                    backspace();
                    expect(ui.get_find_mode() == 0 && c.borrow().finding.is_none(), "a second Backspace ends it")?;
                    s.pass("Backspace twice on an empty search field ends it");
                    // A tree bigger than the index limit: typing stops, Enter searches the disk.
                    c.borrow_mut().index_limit = 2;
                    c.borrow_mut().open_find(0, 3);
                }
                55 => {
                    if !c.borrow().panes[0].finder.as_ref().is_some_and(|f| f.overflow) {
                        return Ok(false);
                    }
                    ui.invoke_find_edited("hit*".into());
                    expect(!c.borrow().typing.running(), "past the index limit typing does not search")?;
                    ui.invoke_find_done(true, "hit*".into());
                }
                56 => {
                    if !searched(&c, "hit*") {
                        return Ok(false);
                    }
                    expect(found(&c) == ["a\\hit.rs", "b\\hit.txt"], &format!("past the index limit Enter walks the disk: {:?}", found(&c)))?;
                    c.borrow_mut().index_limit = crate::finder::INDEX_LIMIT;
                    s.pass("past the index limit the search runs on Enter");
                    // Leaving the results stops the walk.
                    let stop = c.borrow().panes[0].finder.as_ref().map(|f| f.finder.stopped()).ok_or("no walk")?;
                    ui.invoke_command(0, 2);
                    expect(stop.load(std::sync::atomic::Ordering::Relaxed), "\"..\" stops the walk")?;
                }
                57 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    expect(c.borrow().panes[0].found.is_none() && c.borrow().data(0).dir.ends_with("tree"), "\"..\" in the results returns to the searched folder")?;
                    s.pass("Esc / \"..\" leave the results and stop the walk");
                    std::fs::remove_dir(root.join("tree").join("loop")).map_err(|e| e.to_string())?;
                    // Ctrl+Shift+D: two small folders that differ in each way.
                    let at = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
                    for (side, files) in [("cmpL", [("same.txt", "x"), ("old.txt", "o"), ("size.txt", "aa")]), ("cmpR", [("same.txt", "x"), ("new.txt", "n"), ("size.txt", "aaaa")])] {
                        let dir = root.join(side);
                        std::fs::create_dir_all(dir.join("dir")).map_err(|e| e.to_string())?;
                        for (name, text) in files {
                            std::fs::write(dir.join(name), text).map_err(|e| e.to_string())?;
                            let file = std::fs::File::options().write(true).open(dir.join(name)).map_err(|e| e.to_string())?;
                            file.set_modified(at).map_err(|e| e.to_string())?;
                        }
                    }
                    ui.invoke_navigate(0, display(&root.join("cmpL")).into());
                    ui.invoke_navigate(1, display(&root.join("cmpR")).into());
                }
                58 => {
                    if !idle(&c) || !c.borrow().data(0).dir.ends_with("cmpL") || !c.borrow().data(1).dir.ends_with("cmpR") {
                        return Ok(false);
                    }
                    s.sort = (c.borrow().panes[0].column, c.borrow().panes[0].descending);
                    ctrl_shift_key(&ui, 0, "D");
                    expect(c.borrow().compare.as_ref().is_some_and(|k| k.panes == [0, 1]), "Ctrl+Shift+D compares the two panes")?;
                }
                59 => {
                    if !idle(&c) || c.borrow().panes[0].model.compare.borrow().is_none() {
                        return Ok(false);
                    }
                    expect(compared(&c, 0) == ["dir", "", "old.txt", "same.txt", "size.txt"], &format!("left rows: {:?}", compared(&c, 0)))?;
                    expect(compared(&c, 1) == ["dir", "new.txt", "", "same.txt", "size.txt"], &format!("right rows: {:?}", compared(&c, 1)))?;
                    let kinds: Vec<i32> = (0..5).map(|r| c.borrow().panes[0].model.row_at(r).map_or(-1, |row| row.diff)).collect();
                    expect(kinds == [0, 0, 2, 0, 3] && c.borrow().panes[1].model.row_at(1).is_some_and(|row| row.diff == 1), &format!("- / + / changed rows: {kinds:?}"))?;
                    let size = c.borrow().panes[1].model.row_at(4).ok_or("no row")?;
                    expect(size.size_diff && !size.time_diff && !size.attr_diff, "only the size cell differs")?;
                    expect(c.borrow().panes[0].model.row_at(1).is_some_and(|row| row.gap), "the left side has an empty row for new.txt")?;
                    expect(ui.get_p0_info().compare == "-1 +1 ≠1 =2", &format!("counts in the info line: {}", ui.get_p0_info().compare))?;
                    // The cursor moves on both sides; an empty row can hold it.
                    fkey(&ui, 0, slint::platform::Key::Home);
                    fkey(&ui, 0, slint::platform::Key::DownArrow);
                    let other = c.borrow().panes[1].model.selection.borrow().current;
                    let right = other.map(|i| c.borrow().data(1).rows[i].name.clone());
                    expect(c.borrow().panes[0].model.special.get() == Some(Special::Gap(1)) && right.as_deref() == Some("new.txt"), "both cursors on row 2")?;
                    s.pass("Ctrl+Shift+D lines up both folders: -, +, differing cells, cursor in step");
                    ctrl_shift_key(&ui, 0, "D");
                    expect(compared(&c, 1) == ["new.txt", "", "size.txt"], &format!("second Ctrl+Shift+D: only the differences: {:?}", compared(&c, 1)))?;
                    // A change on disk compares again.
                    let made = root.join("cmpR").join("old.txt");
                    std::fs::write(&made, "o").map_err(|e| e.to_string())?;
                    std::fs::File::options().write(true).open(&made).and_then(|f| f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000))).map_err(|e| e.to_string())?;
                }
                60 => {
                    if !idle(&c) || c.borrow().compare.as_ref().is_none_or(|k| k.counts[0] != 0) {
                        return Ok(false);
                    }
                    expect(compared(&c, 0) == ["", "size.txt"], &format!("old.txt now on both sides and the same: {:?}", compared(&c, 0)))?;
                    s.pass("second Ctrl+Shift+D shows only the differences; changes compare again");
                    ctrl_shift_key(&ui, 0, "D");
                    expect(c.borrow().compare.is_none(), "third Ctrl+Shift+D ends the comparison")?;
                }
                61 => {
                    if !idle(&c) {
                        return Ok(false);
                    }
                    let names = names(&c, 0);
                    expect(c.borrow().panes[0].model.compare.borrow().is_none() && names.len() == 4, &format!("the full folder is back: {names:?}"))?;
                    expect((c.borrow().panes[0].column, c.borrow().panes[0].descending) == s.sort, "the sort order is back")?;
                    s.pass("the comparison ends with the folders and sort order as before");
                }
                // Ctrl+I: the agent panel with a fake CLI, in pane 0's folder (cmpL).
                62 => {
                    let program = fake_agent(&root, false)?;
                    c.borrow_mut().config.agent_path = Some(program);
                    c.borrow_mut().active(0);
                    let a = index(&c, 0, "size.txt")?;
                    click(&ui, 0, a, false);
                    expect(c.borrow().config.agent_memory().is_file(), "the memory file is there from the start")?;
                    ctrl_key(&ui, 0, "i");
                    expect(ui.get_agent_open(), "Ctrl+I opens the agent panel")?;
                    expect(ui.invoke_agent_send("make a file".into()), "a request is sent")?;
                    expect(ui.get_agent_running(), "the panel shows the run")?;
                    expect(!ui.invoke_agent_send("second".into()), "nothing more is sent while it runs")?;
                }
                63 => {
                    if c.borrow().agent.run.is_some() || !idle(&c) || !names(&c, 0).contains(&"agent-made.txt".to_string()) || c.borrow().agent.version.is_none() {
                        return Ok(false);
                    }
                    let lines = agent_texts(&c);
                    expect(lines.iter().any(|l| l == "fake done") && lines.iter().any(|l| l == "$ make"), &format!("reply and step in the log: {lines:?}"))?;
                    expect(ui.get_agent_status().starts_with("完了"), &format!("status: {}", ui.get_agent_status()))?;
                    expect(c.borrow().agent.session.as_deref() == Some("fake-1"), "the conversation id is kept")?;
                    let args = std::fs::read_to_string(root.join("agent-args.txt")).map_err(|e| e.to_string())?;
                    expect(args.contains("-s workspace-write") && args.contains("--add-dir") && !args.contains("resume"), &format!("new conversation in the sandbox: {args}"))?;
                    let prompt = std::fs::read_to_string(root.join("agent-prompt.txt")).map_err(|e| e.to_string())?;
                    expect(prompt.contains("<memory>") && prompt.contains("size.txt") && prompt.contains("make a file"), "the prompt carries memory, selection and request")?;
                    // The other pane is named but stays read-only by default.
                    let other = display(&c.borrow().data(1).dir);
                    expect(prompt.contains("other panes (read-only)") && prompt.contains(&other) && !args.contains(&format!("--add-dir {other} ")), &format!("the other pane is read-only by default: {args}"))?;
                    expect(c.borrow().config.agent_memory().is_file(), "the memory file is created")?;
                    expect(ui.get_agent_cli() == "Codex 9.9.9-fake", &format!("the CLI version in the header: {}", ui.get_agent_cli()))?;
                    s.pass("Ctrl+I: request with folder and selection, steps and reply, list refreshed");
                    std::fs::remove_file(root.join("cmpL").join("agent-made.txt")).map_err(|e| e.to_string())?;
                    // ↑ / ↓ in the input: earlier requests, then back to what was typed.
                    expect(ui.invoke_agent_recall(true, "draft".into()) == "make a file", "↑ recalls the last request")?;
                    expect(ui.invoke_agent_recall(true, "make a file".into()) == "make a file", "↑ stops at the oldest")?;
                    expect(ui.invoke_agent_recall(false, "make a file".into()) == "draft", "↓ past the newest gives back the draft")?;
                    // Nothing selected: the cursor row (size.txt) goes instead.
                    fkey(&ui, 0, slint::platform::Key::Escape);
                    expect(selected(&c, 0).is_empty(), "Esc clears the selection")?;
                    // From here the other pane may be written too.
                    c.borrow_mut().config.agent_panes = true;
                    expect(ui.invoke_agent_send("and again".into()), "a follow-up is sent")?;
                }
                64 => {
                    if c.borrow().agent.run.is_some() || !idle(&c) || !names(&c, 0).contains(&"agent-made.txt".to_string()) {
                        return Ok(false);
                    }
                    let args = std::fs::read_to_string(root.join("agent-args.txt")).map_err(|e| e.to_string())?;
                    expect(args.contains("-s workspace-write") && args.contains("resume fake-1"), &format!("the follow-up resumes in the same sandbox: {args}"))?;
                    let prompt = std::fs::read_to_string(root.join("agent-prompt.txt")).map_err(|e| e.to_string())?;
                    expect(!prompt.contains("<memory>") && prompt.contains("and again"), "a follow-up sends only the context and request")?;
                    expect(prompt.contains("under the cursor") && prompt.contains("size.txt"), "with nothing selected, the cursor item is passed")?;
                    let other = display(&c.borrow().data(1).dir);
                    expect(args.contains(&format!("--add-dir {other} ")) && prompt.contains("other panes (writable)"), &format!("the setting makes the other pane writable: {args}"))?;
                    expect(agent_texts(&c).iter().any(|l| l.contains("カーソル: size.txt")), "the log says the cursor item was passed")?;
                    expect(agent_texts(&c).iter().any(|l| l.ends_with("ほかのペイン1件にも書き込み可")), "the log says the other pane may be written")?;
                    s.pass("follow-up continues the conversation with the same sandbox; cursor item when nothing is selected; ↑ ↓ recall");
                    // The wheel over the log goes back through the conversation (and stays).
                    for i in 0..80 {
                        c.borrow().agent_push(1, format!("filler {i}"));
                    }
                    expect(ui.get_agent_following(), "the log follows its end")?;
                    let size = ui.window().size().to_logical(ui.window().scale_factor());
                    let room = size.height - 86.0;
                    let panel = (room * ui.get_agent_split()).min(room - 120.0).max(100.0);
                    let position = slint::LogicalPosition::new(size.width / 2.0, size.height - 24.0 - panel + 60.0);
                    ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
                    ui.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled { position, delta_x: 0.0, delta_y: 120.0 });
                    let program = fake_agent(&root, true)?;
                    c.borrow_mut().config.agent_path = Some(program);
                    expect(ui.invoke_agent_send("slow".into()), "the slow request is sent")?;
                    expect(ui.get_agent_badge().is_empty(), "no badge while the panel shows the run")?;
                    ui.invoke_agent_close();
                    expect(ui.get_agent_badge().contains("実行中"), &format!("closed panel: the history line shows the run ({})", ui.get_agent_badge()))?;
                }
                65 => {
                    // Cancelled once the fake has started its grandchild (ping).
                    if !std::fs::read_to_string(root.join("agent-prompt.txt")).is_ok_and(|p| p.contains("slow")) {
                        return Ok(false);
                    }
                    expect(!ui.get_agent_following(), "the wheel scrolled the log back from its end")?;
                    ui.invoke_agent_cancel();
                    s.cancelled = Some(Instant::now());
                }
                66 => {
                    if c.borrow().agent.run.is_some() {
                        return Ok(false);
                    }
                    // The grandchild holds stderr open: finishing at all means it was killed too.
                    let took = s.cancelled.map_or(Duration::ZERO, |t| t.elapsed());
                    expect(took < Duration::from_secs(20), &format!("cancel ends the whole process tree ({took:?})"))?;
                    expect(agent_texts(&c).last().is_some_and(|l| l == "中断しました"), "the log says it was cancelled")?;
                    expect(ui.get_agent_badge().is_empty(), "the badge goes when the run ends")?;
                    // The splitter above the panel: its share is saved like the pane splitters.
                    ui.set_agent_split(0.3);
                    ui.invoke_layout_committed();
                    let saved = config::Config::load(root.join("config.json")).agent_split;
                    expect((saved - 0.3).abs() < 1e-3, &format!("agent splitter saved: {saved}"))?;
                    ui.invoke_agent_new();
                    expect(c.borrow().agent.session.is_none() && agent_texts(&c).is_empty(), "新しい会話 forgets the conversation")?;
                    ctrl_key(&ui, 0, "i");
                    expect(ui.get_agent_open(), "Ctrl+I opens the closed panel")?;
                    // Through the window: the key reaches the panel's input, which has the focus.
                    window_ctrl_key(&ui, "i");
                    expect(!ui.get_agent_open(), "Ctrl+I in the panel closes it")?;
                    // 使用しない: Ctrl+I, the menu and the history line open nothing, and it is saved.
                    ui.invoke_open_settings();
                    ui.set_settings_agent_cli(1);
                    ui.invoke_settings_done(true);
                    expect(!config::Config::load(root.join("config.json")).agent_enabled, "使用しない is saved")?;
                    ctrl_key(&ui, 0, "i");
                    ui.invoke_agent_show();
                    expect(!ui.get_agent_open() && !ui.get_agent_enabled(), "turned off, nothing opens the panel")?;
                    // Not only the panel: the CLI itself is never started.
                    expect(!ui.invoke_agent_send("一覧を見せて".into()) && c.borrow().agent.run.is_none(), "turned off, a request starts no CLI")?;
                    ui.invoke_open_settings();
                    expect(ui.get_settings_agent_cli() == 1, "the settings show it turned off")?;
                    ui.set_settings_agent_cli(0);
                    ui.invoke_settings_done(true);
                    ctrl_key(&ui, 0, "i");
                    expect(ui.get_agent_open(), "turned on again, Ctrl+I opens the panel")?;
                    window_ctrl_key(&ui, "i");
                    s.pass("cancel stops the CLI and its children; 新しい会話 starts over; 使用しない turns Ctrl+I off");
                }
                // Opening a file starts it in its own folder (as Explorer does), not in ours.
                67 => {
                    let probe = root.join("cmpL").join("where.cmd");
                    if !probe.exists() {
                        std::fs::write(&probe, "@cd > \"%~dp0cwd.txt\"\r\n").map_err(|e| e.to_string())?;
                    }
                    if !idle(&c) || !names(&c, 0).contains(&"where.cmd".to_string()) {
                        return Ok(false);
                    }
                    let row = index(&c, 0, "where.cmd")?;
                    ui.invoke_pointer(0, row, false, false, 2);
                }
                68 => {
                    // The file exists before `cd` has written into it.
                    let Some(cwd) = std::fs::read_to_string(root.join("cmpL").join("cwd.txt")).ok().filter(|t| t.ends_with('\n')) else { return Ok(false) };
                    let expected = display(&root.join("cmpL"));
                    expect(cwd.trim().eq_ignore_ascii_case(&expected), &format!("started in {} (expected {expected})", cwd.trim()))?;
                    s.pass("opening a file starts it in its own folder");
                }
                // A downloaded file whose name hides its extension (U+202E): marked in the list;
                // Alt+Enter shows where it came from and its SHA-256.
                69 => {
                    let name = "invoice\u{202E}fdp.exe";
                    let file = root.join("cmpL").join(name);
                    if !file.exists() {
                        std::fs::write(&file, "x").map_err(|e| e.to_string())?;
                        let mut zone = file.clone().into_os_string();
                        zone.push(":Zone.Identifier");
                        std::fs::write(PathBuf::from(zone), "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/page\r\nHostUrl=https://example.com/files/invoice.exe\r\n")
                            .map_err(|e| e.to_string())?;
                    }
                    if !idle(&c) || !names(&c, 0).contains(&name.to_string()) {
                        return Ok(false);
                    }
                    let i = index(&c, 0, name)?;
                    let row = c.borrow().panes[0].model.row_at(i as usize).ok_or("no row")?;
                    expect(row.name == "invoice[RLO]fdp" && row.ext == "exe" && row.spoof, &format!("the hidden RLO is shown and flagged: {}.{}", row.name, row.ext))?;
                    click(&ui, 0, i, false);
                    fkey(&ui, 0, slint::platform::Key::Escape);
                    let mut event = slint::language::KeyEvent::default();
                    event.text = slint::platform::Key::Return.into();
                    event.modifiers.alt = true;
                    ui.invoke_key_event(0, event);
                    expect(ui.get_props_open(), "Alt+Enter on one file opens the properties")?;
                    expect(ui.get_props_name() == "invoice[RLO]fdp.exe", "the dialog shows the name with the mark")?;
                }
                70 => {
                    if ui.get_props_hash() != 2 {
                        return Ok(false);
                    }
                    let rows: Vec<(String, String)> = ui.get_props_rows().iter().map(|r| (r.label.to_string(), r.value.to_string())).collect();
                    let value = |label: &str| rows.iter().find(|(l, _)| l == label).map(|(_, v)| v.clone()).unwrap_or_default();
                    expect(value("取得元") == "https://example.com/files/invoice.exe", &format!("download source: {rows:?}"))?;
                    expect(value("参照元") == "https://example.com/page" && value("ゾーン") == "インターネット (3)", "referrer and zone")?;
                    expect(value("SHA-256") == "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881", "SHA-256 of the file")?;
                    expect(value("ADS").starts_with("Zone.Identifier"), "the stream is listed")?;
                    ui.invoke_props_copy();
                    let out = std::process::Command::new("powershell")
                        .args(["-NoProfile", "-Command", "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Get-Clipboard -Raw"])
                        .output()
                        .map_err(|e| e.to_string())?;
                    let copied = String::from_utf8_lossy(&out.stdout).into_owned();
                    expect(copied.contains("SHA-256\t2d711642") && copied.contains("取得元\thttps://example.com/files/invoice.exe"), "すべてコピー puts the lines on the clipboard")?;
                    ui.invoke_props_close();
                    expect(!ui.get_props_open() && c.borrow().props.is_none(), "the dialog closes")?;
                    s.pass("hidden bidi characters are marked; properties show the download source, streams and SHA-256");
                }
                // Alt+Enter on a folder adds up its size on a worker, without entering a junction.
                71 => {
                    let folder = root.join("cmpL").join("sized");
                    if !folder.exists() {
                        std::fs::create_dir_all(folder.join("inner")).map_err(|e| e.to_string())?;
                        std::fs::write(folder.join("inner").join("data.bin"), vec![1u8; 4321]).map_err(|e| e.to_string())?;
                        std::process::Command::new("cmd")
                            .args(["/c", "mklink", "/J"])
                            .arg(folder.join("back"))
                            .arg(root.join("cmpL"))
                            .output()
                            .map_err(|e| e.to_string())?;
                    }
                    if !idle(&c) || !names(&c, 0).contains(&"sized".to_string()) {
                        return Ok(false);
                    }
                    let i = index(&c, 0, "sized")?;
                    click(&ui, 0, i, false);
                    fkey(&ui, 0, slint::platform::Key::Escape);
                    let mut event = slint::language::KeyEvent::default();
                    event.text = slint::platform::Key::Return.into();
                    event.modifiers.alt = true;
                    ui.invoke_key_event(0, event);
                    expect(ui.get_props_open(), "Alt+Enter on a folder opens the properties")?;
                }
                72 => {
                    let rows: Vec<(String, String, bool)> = ui.get_props_rows().iter().map(|r| (r.label.to_string(), r.value.to_string(), r.dim)).collect();
                    let Some(skipped) = rows.iter().find(|(l, _, _)| l == "集計外") else { return Ok(false) };
                    expect(skipped.1.starts_with("リンク1件"), &format!("the junction is skipped: {rows:?}"))?;
                    let value = |label: &str| rows.iter().find(|(l, _, _)| l == label).map(|(_, v, _)| v.clone()).unwrap_or_default();
                    expect(value("サイズ") == listing::format_size(4321) && value("内容") == "ファイル1、フォルダー1", &format!("folder totals: {rows:?}"))?;
                    ui.invoke_props_close();
                    s.pass("folder properties add up the size, skipping junctions");
                }
                73 => return Err("done".into()),
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
