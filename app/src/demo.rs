//! `--demo <root> <frames>`: records the README animation. The same folders are built in
//! `<root>` every time, a fixed script drives the real key handling and settings, and each
//! state is written once (`fNNNN.ppm`) with how long it is shown (`frames.ffconcat`).
//! `app\Demo.ps1` turns that into `docs/demo.webp`. Durations come from the script, not
//! the clock, so the result does not depend on the machine's speed.
//! To show a new feature, add a scene to `script`.
use super::*;
use slint::platform::{Key, WindowEvent};
use std::{io::Write, time::SystemTime};

/// Window size of the recording (physical pixels; `Demo.ps1` sets the scale factor to 1).
pub const SIZE: slint::PhysicalSize = slint::PhysicalSize { width: 1200, height: 700 };
/// Present in a demo root, so `prepare` only ever clears a folder it made itself.
const MARKER: &str = "enyfile-demo.txt";

/// Shown as a date, never as "today", so the frames are the same on any day.
fn at(day: u32, hour: u32, minute: u32) -> SystemTime {
    let time = chrono::NaiveDate::from_ymd_opt(2026, 9, day).unwrap().and_hms_opt(hour, minute, 0).unwrap();
    SystemTime::from(time.and_local_timezone(chrono::Local).unwrap())
}
/// (path, size, (day, hour, minute)). Projects and Backup are alike except where the
/// comparison has something to show; Photos and Music fill the four-pane layout.
fn files() -> Vec<(String, u64, (u32, u32, u32))> {
    let project: &[(&str, u64, (u32, u32, u32))] = &[
        ("docs/guide.md", 12_800, (12, 9, 30)),
        ("docs/api.md", 31_400, (12, 9, 41)),
        ("docs/faq.md", 5_210, (8, 17, 2)),
        ("src/main.rs", 96_300, (18, 13, 55)),
        ("src/ui.slint", 52_800, (18, 13, 40)),
        ("src/config.rs", 11_950, (15, 11, 20)),
        ("assets/logo.png", 48_210, (3, 10, 0)),
        ("assets/banner.jpg", 312_400, (3, 10, 4)),
        ("CHANGELOG.md", 9_120, (18, 14, 2)),
        ("build.ps1", 1_860, (10, 19, 12)),
        ("demo.mp4", 2_640_000, (16, 21, 30)),
        ("notes.txt", 1_024, (14, 8, 45)),
        ("release.zip", 1_482_000, (18, 14, 10)),
    ];
    let mut out = Vec::new();
    for (path, size, time) in project {
        out.push((format!("Projects/{path}"), *size, *time));
        out.push((format!("Backup/{path}"), *size, *time));
    }
    out.extend([
        ("Projects/README.md".to_string(), 4_380, (18, 14, 5)),
        ("Backup/README.md".to_string(), 4_120, (11, 16, 48)),
        ("Projects/settings.json".to_string(), 2_210, (17, 22, 3)),
        ("Backup/settings.json".to_string(), 2_210, (9, 12, 0)),
        ("Projects/todo.md".to_string(), 640, (18, 9, 15)),
        ("Backup/old-notes.txt".to_string(), 2_048, (2, 18, 30)),
    ]);
    for (name, size, day) in [("IMG_0412.jpg", 3_210_000, 6), ("IMG_0413.jpg", 2_980_000, 6), ("IMG_0420.png", 1_120_000, 7), ("panorama.jpg", 8_400_000, 7), ("screen.png", 412_000, 13)] {
        out.push((format!("Photos/{name}"), size, (day, 15, 20)));
    }
    for (name, size, day) in [("01 Intro.flac", 18_200_000, 1), ("02 Morning.mp3", 6_300_000, 1), ("03 Rain.mp3", 7_100_000, 1), ("playlist.m3u", 410, 2)] {
        out.push((format!("Music/{name}"), size, (day, 20, 0)));
    }
    out
}
/// Builds the demo folders afresh (refusing a non-empty folder it did not make).
pub fn prepare(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if root.exists() {
        let empty = std::fs::read_dir(root)?.next().is_none();
        if !empty && !root.join(MARKER).exists() {
            return Err(format!("{} is not a demo folder ({MARKER} is missing)", root.display()).into());
        }
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root)?;
    std::fs::write(root.join(MARKER), "Made by enyfile --demo; cleared on every run.")?;
    let files = files();
    for (path, size, _) in &files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        // Content does not matter; the size is what the list shows.
        std::fs::write(&path, vec![b'.'; *size as usize])?;
    }
    // Downloaded, as a browser leaves it: the properties dialog shows where from. Before the
    // times below, since writing the stream touches the file.
    std::fs::write(
        root.join("Projects/release.zip:Zone.Identifier"),
        "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/download\r\nHostUrl=https://example.com/files/release.zip\r\n",
    )?;
    for (path, _, (day, hour, minute)) in &files {
        use std::os::windows::fs::FileTimesExt;
        // Created as well: the properties dialog shows it.
        let times = std::fs::FileTimes::new().set_modified(at(*day, *hour, *minute)).set_created(at(*day, *hour, *minute));
        std::fs::File::options().write(true).open(root.join(path))?.set_times(times)?;
    }
    // Folders last (writing files touched them): one time, so the comparison shows no
    // folder differences.
    use std::os::windows::fs::OpenOptionsExt;
    let mut folders: Vec<PathBuf> = files.iter().filter_map(|(p, _, _)| Path::new(p).parent().map(Path::to_path_buf)).collect();
    folders.sort();
    folders.dedup();
    for folder in folders.iter().rev() {
        let dir = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(root.join(folder))?;
        dir.set_modified(at(1, 9, 0))?;
    }
    Ok(())
}
pub fn start_folders(root: &Path) -> (PathBuf, PathBuf) {
    (root.join("Projects"), root.join("Backup"))
}

/// A light palette in the settings' color list order, and darker extension colors that
/// read on white.
fn light_colors() -> Vec<slint::Color> {
    let light = [
        ("pane", "#ffffff"),
        ("text", "#1f2328"),
        ("folder-text", "#1f2328"),
        ("drive-text", "#1a5fb4"),
        ("readonly-text", "#9a6700"),
        ("hidden-text", "#8c959f"),
        ("size-text", "#3d444d"),
        ("time-text", "#3d444d"),
        ("attr-text", "#6e7781"),
        ("kind", "#1a5fb4"),
        ("selected-bg", "#cfe2fb"),
        ("cursor-bg", "#2f6fd0"),
        ("accent", "#1a5fb4"),
        ("chrome", "#e8eaee"),
        ("bar", "#f3f4f6"),
        ("diff-add-bg", "#d5f5dc"),
        ("diff-del-bg", "#fbdcdc"),
        ("diff-change-bg", "#fbecb0"),
        ("selected-text", "#0d1117"),
        ("cursor-text", "#ffffff"),
        ("cursor-selected-bg", "#1d56ad"),
        ("cursor-inactive-bg", "#e3e7ec"),
        ("cursor-inactive-line", "#b8bfc8"),
        ("dim", "#656d76"),
        ("border", "#c9ced6"),
        ("dialog", "#f7f8fa"),
    ];
    theme::BASE
        .iter()
        .map(|(key, _, default)| {
            let hex = light.iter().find(|(k, _)| k == key).map_or(*default, |(_, v)| v);
            theme::parse(hex).unwrap()
        })
        .collect()
}
fn light_extensions() -> Vec<(String, slint::Color)> {
    let darker = |c: slint::Color| {
        let f = |v: u8| (v as f32 * 0.6) as u8;
        slint::Color::from_rgb_u8(f(c.red()), f(c.green()), f(c.blue()))
    };
    config::Config::default_extension_colors().into_iter().map(|(e, c)| (e, darker(c))).collect()
}

/// Answers like `codex exec --json` (UTF-8 through chcp 65001): one command that takes a
/// while, its effect, and a reply.
const FAKE_CODEX: &str = "@echo off\r\nif \"%~1\"==\"--version\" (echo codex-cli 0.162.0& exit /b 0)\r\nchcp 65001 >nul\r\nfindstr \"^\" >nul\r\n\
echo {\"type\":\"thread.started\",\"thread_id\":\"demo-1\"}\r\n\
echo {\"type\":\"item.started\",\"item\":{\"id\":\"1\",\"type\":\"command_execution\",\"command\":\"powershell.exe -Command Get-ChildItem *.jpg,*.png | ForEach-Object { $d = $_.LastWriteTime.ToString('yyyy-MM-dd'); mkdir -Force $d; Move-Item $_ $d }\",\"status\":\"in_progress\"}}\r\n\
ping -n 3 127.0.0.1 >nul\r\n\
mkdir 2026-09-06 2026-09-07 2026-09-13\r\n\
move IMG_0412.jpg 2026-09-06 >nul\r\nmove IMG_0413.jpg 2026-09-06 >nul\r\n\
move IMG_0420.png 2026-09-07 >nul\r\nmove panorama.jpg 2026-09-07 >nul\r\nmove screen.png 2026-09-13 >nul\r\n\
echo {\"type\":\"item.completed\",\"item\":{\"id\":\"1\",\"type\":\"command_execution\",\"command\":\"powershell.exe -Command Get-ChildItem *.jpg,*.png | ForEach-Object { $d = $_.LastWriteTime.ToString('yyyy-MM-dd'); mkdir -Force $d; Move-Item $_ $d }\",\"exit_code\":0,\"status\":\"completed\"}}\r\n\
echo {\"type\":\"item.completed\",\"item\":{\"id\":\"2\",\"type\":\"agent_message\",\"text\":\"5 枚を更新日ごとに 3 つのフォルダへ移動しました（2026-09-06 に 2 枚、2026-09-07 に 2 枚、2026-09-13 に 1 枚）。写真の撮影日（Exif）で分けたいときや、フォルダ名を「2026年9月」のように月単位にしたいときは、続けて指示してください。\"}}\r\n";

type Act = Box<dyn Fn(&App, &Rc<RefCell<Controller>>)>;
enum Step {
    /// Runs on the UI thread with nothing borrowed.
    Do(Act),
    /// Records the current state, shown for this many milliseconds (once the panes are idle).
    Show(u32),
    /// Lets this much real time pass without recording (icons from the Shell settle).
    Wait(u32),
}
#[derive(Default)]
struct Script(Vec<Step>);
impl Script {
    fn act(&mut self, act: impl Fn(&App, &Rc<RefCell<Controller>>) + 'static) {
        self.0.push(Step::Do(Box::new(act)));
    }
    fn show(&mut self, ms: u32) {
        self.0.push(Step::Show(ms));
    }
    /// The caption: the keys (highlighted) and what they do.
    fn caption(&mut self, keys: &'static str, text: &'static str) {
        self.act(move |ui, _| {
            ui.set_demo_keys(keys.into());
            ui.set_demo_caption(text.into());
        });
    }
    /// A key to the active pane, e.g. "Down", "Ctrl+Shift+F".
    fn key(&mut self, spec: &'static str, ms: u32) {
        self.act(move |ui, c| {
            let p = c.borrow().active as i32;
            let _ = ui.invoke_key_event(p, key_event(spec));
        });
        self.show(ms);
    }
    /// Typing into whatever has the focus (the find bar), one shown frame per character.
    fn typed(&mut self, text: &'static str, ms: u32) {
        for ch in text.chars() {
            self.act(move |ui, _| press(ui, ch.to_string().into()));
            self.show(ms);
        }
    }
    /// A key through the window, to the focused text field.
    fn window_key(&mut self, key: Key, ms: u32) {
        self.act(move |ui, _| press(ui, key.into()));
        self.show(ms);
    }
}
fn press(ui: &App, text: slint::SharedString) {
    ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window().dispatch_event(WindowEvent::KeyReleased { text });
}
fn key_event(spec: &str) -> slint::language::KeyEvent {
    let mut event = slint::language::KeyEvent::default();
    let mut parts: Vec<&str> = spec.split('+').collect();
    let name = parts.pop().unwrap_or_default();
    for modifier in parts {
        match modifier {
            "Ctrl" => event.modifiers.control = true,
            "Shift" => event.modifiers.shift = true,
            "Alt" => event.modifiers.alt = true,
            _ => {}
        }
    }
    event.text = match name {
        "Up" => Key::UpArrow.into(),
        "Down" => Key::DownArrow.into(),
        "Left" => Key::LeftArrow.into(),
        "Right" => Key::RightArrow.into(),
        "Home" => Key::Home.into(),
        "Enter" => Key::Return.into(),
        "Esc" => Key::Escape.into(),
        "Tab" => Key::Tab.into(),
        "Backspace" => Key::Backspace.into(),
        letter => letter.into(),
    };
    event
}

/// The recording, scene by scene. Keep each step slow enough to follow: about 0.35 s per
/// cursor move, 0.15 s per typed character, 1.5-2.5 s on each result.
fn script(root: &Path) -> Script {
    let mut s = Script::default();
    let (photos, music) = (root.join("Photos"), root.join("Music"));
    s.act(move |ui, c| {
        // The script starts on the left (the app starts on the right).
        ui.invoke_focus_pane(0);
        let mut c = c.borrow_mut();
        c.active(0);
        // Panes 3 and 4 show other folders once the layout has four.
        c.navigate(2, photos.clone(), Mode::New(None));
        c.navigate(3, music.clone(), Mode::New(None));
    });
    s.0.push(Step::Wait(1500));

    // Two panes: moving, opening, going back, switching sides.
    s.caption("", "enyFile — キーボードで操作する 2 ペインのファイラー");
    s.show(2400);
    s.caption("↑ ↓", "カーソル移動");
    s.key("Down", 380);
    s.key("Down", 380);
    s.key("Down", 380);
    s.key("Up", 600);
    s.caption("Enter", "フォルダを開く");
    s.key("Enter", 1500);
    s.caption("Backspace", "親フォルダへ");
    s.key("Backspace", 1300);
    s.caption("Tab", "反対側のペインへ");
    s.key("Tab", 1000);
    s.key("Down", 380);
    s.key("Down", 380);
    s.key("Down", 380);
    s.key("Tab", 900);

    // Filter.
    s.caption("Ctrl+Shift+F", "フィルター");
    s.key("Ctrl+Shift+F", 800);
    s.typed("*.md", 170);
    s.window_key(Key::Return, 2200);
    s.caption("Esc", "フィルターを解除");
    s.key("Esc", 1300);

    // Comparing the two folders.
    s.caption("Ctrl+Shift+D", "左右のフォルダを比較（- 左だけ / + 右だけ / 黄 違うところ）");
    s.key("Ctrl+Shift+D", 3200);
    s.caption("↓", "カーソルは左右で連動");
    s.key("Down", 420);
    s.key("Down", 420);
    s.key("Down", 900);
    s.caption("Ctrl+Shift+D", "もう一度で差分だけ");
    s.key("Ctrl+Shift+D", 2800);
    s.caption("Esc", "比較を終了");
    s.key("Esc", 1200);

    // Properties: where a download came from, and its SHA-256.
    s.caption("rel", "名前の頭文字でカーソル移動");
    s.key("r", 150);
    s.key("e", 150);
    s.key("l", 900);
    s.caption("Alt+Enter", "プロパティ（ダウンロード元と SHA-256）");
    s.act(|ui, c| {
        let p = c.borrow().active as i32;
        let _ = ui.invoke_key_event(p, key_event("Alt+Enter"));
    });
    // The hash of 1.4 MB is done in a moment.
    s.0.push(Step::Wait(500));
    s.show(3400);
    s.window_key(Key::Escape, 800);

    // Four panes (from the top of the list: the shorter panes show fewer rows per page).
    s.act(|ui, c| {
        let p = c.borrow().active as i32;
        let _ = ui.invoke_key_event(p, key_event("Home"));
    });
    s.caption("Ctrl+Shift+4", "4 ペイン");
    s.key("Ctrl+Shift+4", 2000);
    s.caption("Tab", "ペインを順に移動");
    s.key("Tab", 700);
    s.key("Tab", 700);
    s.key("Tab", 900);
    s.caption("Ctrl+Shift+2", "2 ペインに戻す");
    s.key("Ctrl+Shift+2", 1400);

    // The agent, with a stand-in for Codex that answers the same way (no model is asked).
    let (fake, photos, projects) = (root.join("fake-codex.cmd"), root.join("Photos"), root.join("Projects"));
    s.act(move |_, c| {
        let _ = std::fs::write(&fake, FAKE_CODEX);
        let mut c = c.borrow_mut();
        c.config.agent_path = Some(fake.clone());
        c.config.agent_access = agent::Access::Folder;
        let p = c.active;
        c.navigate(p, photos.clone(), Mode::New(None));
    });
    s.caption("Ctrl+I", "エージェントに頼む（Codex CLI を呼び出す）");
    s.show(1200);
    s.key("Ctrl+I", 900);
    s.typed("画像を日付ごとのフォルダに分けて", 110);
    s.window_key(Key::Return, 700);
    s.0.push(Step::Wait(600));
    s.caption("", "現在のフォルダと選択を渡し、サンドボックスの中で実行");
    s.show(1800);
    s.0.push(Step::Wait(2600));
    s.caption("", "終わると一覧を更新。続けて指示すると同じ会話の続きになる");
    s.show(3200);
    s.caption("Esc", "パネルを閉じる");
    s.window_key(Key::Escape, 1000);
    // Into one of the new folders, to show the pictures really moved.
    s.caption("", "画像は日付のフォルダへ移動済み");
    s.key("Home", 380);
    s.key("Down", 600);
    s.key("Enter", 2000);
    s.caption("ファイル > 設定", "エージェントが書き込める範囲とメモリ");
    s.act(|ui, c| {
        c.borrow_mut().open_settings();
        ui.set_settings_tab(4);
    });
    s.0.push(Step::Wait(300));
    // Not the stand-in, nor where this machine keeps Codex.
    s.act(|ui, _| {
        ui.set_settings_agent_path("".into());
        ui.set_settings_agent_detected("空欄: 自動で検出（PATH の codex）".into());
    });
    s.show(2600);
    s.act(|_, c| c.borrow_mut().settings_done(false));
    s.act(move |_, c| {
        let mut c = c.borrow_mut();
        c.agent_new();
        let p = c.active;
        c.navigate(p, projects.clone(), Mode::New(None));
    });
    s.show(800);

    // Colors and font, through the settings dialog as a user would.
    s.caption("ファイル > 設定", "配色とフォントを変更");
    s.act(|ui, c| {
        c.borrow_mut().open_settings();
        ui.set_settings_tab(1);
    });
    s.show(1500);
    s.act(|_, c| {
        let mut c = c.borrow_mut();
        c.settings_colors = light_colors();
        c.settings_ext = light_extensions();
        c.settings_color_rows();
    });
    s.show(1500);
    s.act(|ui, c| {
        let family = ["Yu Gothic UI", "Yu Gothic", "游ゴシック"]
            .into_iter()
            .find(|f| theme::font_families().iter().any(|n| n == f))
            .unwrap_or("Yu Gothic UI");
        ui.set_settings_tab(2);
        ui.set_settings_font_family(family.into());
        let size = c.borrow().settings_font.1;
        c.borrow_mut().font_set(family, size);
    });
    s.show(1600);
    s.act(|_, c| c.borrow_mut().settings_done(true));
    s.caption("", "ライト配色 + 游ゴシック");
    s.show(1500);
    s.key("Down", 420);
    s.key("Down", 420);
    s.key("Down", 420);
    s.key("Ctrl+Shift+D", 3000);
    s.key("Esc", 600);
    s.caption("", "");
    s.show(1500);
    s
}

struct Recorder {
    out: PathBuf,
    /// (file, shown for ms) per distinct frame.
    frames: Vec<(String, u32)>,
    last: Vec<u8>,
}
impl Recorder {
    fn record(&mut self, ui: &App, ms: u32) -> Result<(), String> {
        let shot = ui.window().take_snapshot().map_err(|e| e.to_string())?;
        let (w, h) = (shot.width(), shot.height());
        let rgb: Vec<u8> = shot.as_slice().iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        // An unchanged state only lengthens the frame before it.
        if rgb == self.last {
            if let Some(frame) = self.frames.last_mut() {
                frame.1 += ms;
                return Ok(());
            }
        }
        let name = format!("f{:04}.ppm", self.frames.len() + 1);
        let mut file = std::fs::File::create(self.out.join(&name)).map_err(|e| e.to_string())?;
        file.write_all(format!("P6\n{w} {h}\n255\n").as_bytes()).map_err(|e| e.to_string())?;
        file.write_all(&rgb).map_err(|e| e.to_string())?;
        self.frames.push((name, ms));
        self.last = rgb;
        Ok(())
    }
    /// ffmpeg's concat list; the last file is repeated so its duration counts.
    fn finish(&self) -> Result<(), String> {
        let mut list = String::from("ffconcat version 1.0\n");
        for (name, ms) in &self.frames {
            list += &format!("file '{name}'\nduration {:.3}\n", *ms as f64 / 1000.0);
        }
        if let Some((name, _)) = self.frames.last() {
            list += &format!("file '{name}'\n");
        }
        std::fs::write(self.out.join("frames.ffconcat"), list).map_err(|e| e.to_string())
    }
}

/// Plays the script and quits. Errors go to `<frames>\demo-error.txt`.
pub fn start(c: Rc<RefCell<Controller>>, root: PathBuf, out: Option<PathBuf>) -> slint::Timer {
    let out = out.unwrap_or_else(|| root.join("frames"));
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::create_dir_all(&out);
    let steps = RefCell::new(script(&root).0.into_iter().collect::<VecDeque<_>>());
    let recorder = RefCell::new(Recorder { out: out.clone(), frames: Vec::new(), last: Vec::new() });
    let waiting = Cell::new(None::<Instant>);
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(40), move || {
        let fail = |e: String| {
            let _ = std::fs::write(out.join("demo-error.txt"), e);
            let _ = slint::quit_event_loop();
        };
        if started.elapsed() > Duration::from_secs(180) {
            return fail("timeout".into());
        }
        let Some(ui) = c.borrow().ui.upgrade() else { return };
        let Some(step) = steps.borrow_mut().pop_front() else {
            if let Err(e) = recorder.borrow().finish() {
                return fail(e);
            }
            let _ = slint::quit_event_loop();
            return;
        };
        match step {
            Step::Do(act) => act(&ui, &c),
            Step::Wait(ms) => {
                let since = waiting.get().unwrap_or_else(Instant::now);
                waiting.set(Some(since));
                if since.elapsed() < Duration::from_millis(ms as u64) {
                    steps.borrow_mut().push_front(Step::Wait(ms));
                } else {
                    waiting.set(None);
                }
            }
            Step::Show(ms) => {
                let busy = {
                    let c = c.borrow();
                    c.operations > 0 || c.panes.iter().any(|p| p.pending.is_some() || p.stale)
                };
                if busy {
                    steps.borrow_mut().push_front(Step::Show(ms));
                } else if let Err(e) = recorder.borrow_mut().record(&ui, ms) {
                    fail(e);
                }
            }
        }
    });
    timer
}
