#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod compare;
mod config;
mod ctxmenu;
mod demo;
mod finder;
mod icons;
mod listing;
mod paste;
mod pathid;
mod shell;
mod smoke;
mod theme;
use listing::{Drive, Entry, Selection};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Model, ModelNotify, ModelRc, ModelTracker};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};
use windows::Win32::Foundation::HWND;
slint::slint! { export { App, Row, AskInfo, PaneInfo, BookmarkRow, PlaceRow, ColorRow, ExtRow, Theme, Columns } from "src/ui.slint"; }

type Positions = Arc<HashMap<Arc<PathBuf>, usize>>;
#[derive(Default, Clone)]
struct Snapshot {
    dir: PathBuf,
    rows: Vec<Entry>,
    positions: Positions,
    volume: Option<Arc<listing::Volume>>,
    /// All entries while a filter hides some, so changing the filter needs no disk read.
    unfiltered: Option<Arc<Vec<Entry>>>,
}
impl Snapshot {
    fn new(dir: PathBuf, rows: Vec<Entry>, volume: Option<Arc<listing::Volume>>) -> Self {
        let positions = Arc::new(
            rows.iter()
                .enumerate()
                .map(|(i, e)| (e.path.clone(), i))
                .collect(),
        );
        Self {
            dir,
            rows,
            positions,
            volume,
            unfiltered: None,
        }
    }
    /// The list shows a ".." row first whenever the folder has a parent.
    fn offset(&self) -> usize {
        self.dir.parent().is_some() as usize
    }
}
/// Where the cursor rests when it is not on an entry.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Special {
    Parent,
    Drive(usize),
    /// Comparing: a row where only the other pane has an entry.
    Gap(usize),
}
/// What a model row shows.
enum Slot {
    Parent,
    Entry(usize),
    Drive(usize),
    Gap,
}
/// Model rows: ".." (when the folder has a parent), the entries, then the drives.
/// Selection works on entry indexes; the cursor can also rest on ".." or a drive.
/// Comparing, the rows are those of `compare` instead (no "..", no drives).
struct RowsModel {
    /// Ctrl+Shift+D: the rows shared with the other pane, and this pane's side (0 old, 1 new).
    compare: RefCell<Option<(Rc<compare::Aligned>, usize)>>,
    data: RefCell<Arc<Snapshot>>,
    selection: RefCell<Selection>,
    drives: RefCell<Arc<Vec<Drive>>>,
    /// The cursor is on ".." or a drive rather than on `selection.current`.
    special: Cell<Option<Special>>,
    /// The list is shown one page at a time: model rows are page-relative.
    page_start: Cell<usize>,
    page_size: Cell<usize>,
    /// Lower-case extension -> name color (Settings > カラー).
    ext_colors: RefCell<Arc<HashMap<String, slint::Color>>>,
    notify: ModelNotify,
}
impl RowsModel {
    fn slot(&self, row: usize) -> Option<Slot> {
        if let Some((aligned, side)) = &*self.compare.borrow() {
            return aligned.pairs.get(row).map(|pair| pair[*side].map_or(Slot::Gap, Slot::Entry));
        }
        let data = self.data.borrow();
        let (offset, entries) = (data.offset(), data.rows.len());
        if row < offset {
            Some(Slot::Parent)
        } else if row < offset + entries {
            Some(Slot::Entry(row - offset))
        } else {
            let drive = row - offset - entries;
            (drive < self.drives.borrow().len()).then_some(Slot::Drive(drive))
        }
    }
    fn special_row(&self, special: Special) -> usize {
        let data = self.data.borrow();
        match special {
            Special::Parent => 0,
            Special::Drive(i) => data.offset() + data.rows.len() + i,
            Special::Gap(row) => row,
        }
    }
    /// Global row of entry `index`.
    fn entry_row(&self, index: usize) -> usize {
        match &*self.compare.borrow() {
            Some((aligned, side)) => aligned.row_of(*side, index).unwrap_or(0),
            None => index + self.data.borrow().offset(),
        }
    }
    fn on_parent(&self) -> bool {
        self.special.get() == Some(Special::Parent)
    }
}
thread_local! {
    /// Slint images made from icon pixels, once per icon key (UI thread only).
    static IMAGES: RefCell<HashMap<Arc<str>, slint::Image>> = RefCell::new(HashMap::new());
}
/// The icon for `key`, or the `fallback` (type icon) while a per-file icon is still loading.
fn icon_image(key: &Arc<str>, fallback: &Arc<str>) -> slint::Image {
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        for key in [key, fallback] {
            if let Some(image) = images.get(key) {
                return image.clone();
            }
            if let Some(pixels) = icons::get(key) {
                let image = slint::Image::from_rgba8(pixels);
                images.insert(key.clone(), image.clone());
                return image;
            }
        }
        slint::Image::default()
    })
}
impl RowsModel {
    /// All rows: "..", entries, drives.
    fn total(&self) -> usize {
        if let Some((aligned, _)) = &*self.compare.borrow() {
            return aligned.pairs.len();
        }
        let data = self.data.borrow();
        data.rows.len() + data.offset() + self.drives.borrow().len()
    }
    /// Shows the page holding global row `row`; true if the page changed.
    fn show_page_of(&self, row: usize) -> bool {
        let size = self.page_size.get().max(1);
        let start = row.min(self.total().saturating_sub(1)) / size * size;
        let changed = start != self.page_start.get();
        self.page_start.set(start);
        changed
    }
    fn pages(&self) -> (usize, usize) {
        let size = self.page_size.get().max(1);
        (self.page_start.get() / size + 1, self.total().div_ceil(size).max(1))
    }
}
impl Model for RowsModel {
    type Data = Row;
    fn row_count(&self) -> usize {
        self.total().saturating_sub(self.page_start.get()).min(self.page_size.get().max(1))
    }
    fn row_data(&self, row: usize) -> Option<Row> {
        if row >= self.row_count() {
            return None;
        }
        self.row_at(self.page_start.get() + row)
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
}
impl RowsModel {
    /// Row `row` of the whole list.
    fn row_at(&self, row: usize) -> Option<Row> {
        let index = match self.slot(row)? {
            Slot::Parent => {
                let folder: Arc<str> = icons::type_key("", true);
                return Some(Row {
                    name: "..".into(),
                    size: "[DIR]".into(),
                    directory: true,
                    parent: true,
                    current: self.on_parent(),
                    icon: icon_image(&folder, &folder),
                    ..Row::default()
                });
            }
            Slot::Drive(i) => {
                let drives = self.drives.borrow();
                let drive = &drives[i];
                return Some(Row {
                    name: drive.name.clone().into(),
                    // No medium (empty optical drive): no bar, no free space.
                    kind: if drive.total > 0 { format!("{} フリー", listing::format_size(drive.free)) } else { String::new() }.into(),
                    drive: true,
                    used: if drive.total > 0 { 1.0 - drive.free as f32 / drive.total as f32 } else { 0.0 },
                    current: self.special.get() == Some(Special::Drive(i)),
                    icon: icon_image(&drive.icon, &drive.icon),
                    ..Row::default()
                });
            }
            Slot::Gap => {
                return Some(Row {
                    compare: true,
                    gap: true,
                    current: self.special.get() == Some(Special::Gap(row)),
                    ..Row::default()
                });
            }
            Slot::Entry(index) => index,
        };
        let diff = self.compare.borrow().as_ref().map(|(aligned, _)| aligned.diffs[row]);
        let data = self.data.borrow();
        let entry = data.rows.get(index)?;
        let selection = self.selection.borrow();
        let (date, time) = listing::format_date(entry.modified, chrono::Local::now().date_naive());
        let tint = (!entry.directory).then(|| self.ext_colors.borrow().get(&entry.ext.to_lowercase()).copied()).flatten();
        Some(Row {
            icon: icon_image(&entry.icon, &entry.type_icon),
            hidden: entry.hidden(),
            readonly: entry.attributes & 0x1 != 0,
            tinted: tint.is_some(),
            tint: tint.unwrap_or_default(),
            location: entry.location.clone().into(),
            name: entry.stem.clone().into(),
            ext: entry.ext.clone().into(),
            size: if entry.directory {
                "[DIR]".into()
            } else {
                listing::format_size(entry.size).into()
            },
            date: date.into(),
            time: time.into(),
            attr: listing::format_attributes(entry.attributes).into(),
            kind: entry.type_name.as_ref().into(),
            directory: entry.directory,
            selected: selection.contains(&entry.path),
            current: self.special.get().is_none() && selection.current == Some(index),
            compare: diff.is_some(),
            diff: diff.map_or(0, |d| d.kind),
            size_diff: diff.is_some_and(|d| d.size),
            time_diff: diff.is_some_and(|d| d.time),
            attr_diff: diff.is_some_and(|d| d.attr),
            ..Row::default()
        })
    }
}
#[derive(Clone)]
struct Saved {
    path: PathBuf,
    selection: Selection,
    current: Option<Arc<PathBuf>>,
    anchor: Option<Arc<PathBuf>>,
}
#[derive(Clone)]
enum Mode {
    New(Option<Arc<PathBuf>>),
    Back,
    Forward,
    Refresh,
    Sort,
    /// Refresh after paste: select the pasted items (and anything new), like Explorer.
    Pasted(Positions, Arc<Vec<PathBuf>>),
    /// Back from search results to the folder as it was.
    Return(Saved),
}
struct Job {
    generation: u64,
    path: PathBuf,
    source: Option<Arc<Snapshot>>,
    column: usize,
    descending: bool,
    show_hidden: bool,
    filter: Option<listing::Pattern>,
}
/// Rename / new file / new folder, waiting for a name in the name dialog.
enum NameKind {
    Rename(PathBuf),
    NewFile,
    NewFolder,
}
struct Naming {
    pane: usize,
    dir: PathBuf,
    kind: NameKind,
    /// Lower-cased names already in the folder (the renamed item itself excluded).
    taken: HashSet<String>,
    before: Positions,
}
enum Reply {
    /// Per-file icons (exe, lnk, ...) of the listing in this pane are ready.
    Icons(usize),
    Drives(Arc<Vec<Drive>>),
    Menu(ctxmenu::Outcome),
    /// Rename or creation done: (pane, folder, listing before, result path).
    Made(usize, PathBuf, Positions, Result<PathBuf, String>),
    Listing(usize, u64, Result<Arc<Snapshot>, String>),
    Changed(usize, PathBuf),
    Pasted(usize, PathBuf, Positions, Vec<PathBuf>, Result<PasteOutcome, String>),
    Ask(usize, paste::Question, AnswerTx),
    Deleted(Result<bool, String>),
    Opened(Result<(), String>),
    /// Folder chosen in the settings dialog for place `usize`.
    Picked(usize, PathBuf),
    /// From the recursive search walk `u64` of a pane.
    Finder(usize, u64, finder::Event),
}

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<RefCell<Controller>>>> = const { RefCell::new(None) };
}
/// Hands a worker result to the UI thread; the event loop wakes only when something arrives.
fn post(reply: Reply) -> bool {
    slint::invoke_from_event_loop(move || {
        if let Some(c) = CONTROLLER.with(|c| c.borrow().clone()) {
            c.borrow_mut().handle(reply);
        }
    })
    .is_ok()
}

struct Mail {
    pending: Option<Job>,
    closed: bool,
}
/// One listing/sort worker per pane. A newer request replaces an unstarted older one.
struct Worker {
    mail: Arc<(Mutex<Mail>, Condvar)>,
}
impl Worker {
    fn new(pane: usize) -> Self {
        let mail = Arc::new((
            Mutex::new(Mail {
                pending: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let worker = mail.clone();
        std::thread::spawn(move || {
            // File type names come from the Shell, which needs COM on this thread.
            let _com = shell::Apartment::new();
            loop {
                let job = {
                    let (lock, cv) = &*worker;
                    let mut m = lock.lock().unwrap();
                    while m.pending.is_none() && !m.closed {
                        m = cv.wait(m).unwrap();
                    }
                    if m.closed {
                        return;
                    }
                    m.pending.take().unwrap()
                };
                let fresh = job.source.is_none();
                let result = if let Some(source) = job.source {
                    let rows = source.unfiltered.as_ref().map_or_else(|| source.rows.clone(), |all| all.as_ref().clone());
                    Ok((source.dir.clone(), rows, source.volume.clone()))
                } else {
                    listing::read(&job.path, job.show_hidden)
                        .map(|(dir, rows)| {
                            let volume = listing::volume(&dir).map(Arc::new);
                            (dir, rows, volume)
                        })
                };
                let result = result.map(|(dir, mut rows, volume)| {
                    listing::sort(&mut rows, job.column, job.descending);
                    let Some(filter) = &job.filter else {
                        return Arc::new(Snapshot::new(dir, rows, volume));
                    };
                    let shown = rows.iter().filter(|e| filter.matches_entry(e)).cloned().collect();
                    let mut snapshot = Snapshot::new(dir, shown, volume);
                    snapshot.unfiltered = Some(Arc::new(rows));
                    Arc::new(snapshot)
                });
                // Files with their own icon (exe, lnk, ...) are looked up after the list shows.
                let per_file: Vec<(Arc<str>, Arc<PathBuf>)> = match &result {
                    Ok(snapshot) if fresh => snapshot
                        .unfiltered
                        .as_deref()
                        .unwrap_or(&snapshot.rows)
                        .iter()
                        .filter(|e| e.icon != e.type_icon && icons::get(&e.icon).is_none())
                        .map(|e| (e.icon.clone(), e.path.clone()))
                        .collect(),
                    _ => Vec::new(),
                };
                // Kind icons the Shell failed to give (it can be busy): asked again below.
                let mut missing: Vec<(Arc<str>, bool)> = match &result {
                    Ok(snapshot) => snapshot
                        .unfiltered
                        .as_deref()
                        .unwrap_or(&snapshot.rows)
                        .iter()
                        .filter(|e| icons::get(&e.type_icon).is_none())
                        .map(|e| (e.type_icon.clone(), e.directory))
                        .collect(),
                    _ => Vec::new(),
                };
                missing.sort();
                missing.dedup();
                if !post(Reply::Listing(pane, job.generation, result)) {
                    return;
                }
                if !per_file.is_empty() {
                    for (key, path) in &per_file {
                        // A newer request wins: stop and let it run.
                        if worker.0.lock().unwrap().pending.is_some() {
                            break;
                        }
                        icons::ensure_path(key, &shell::shell_path(path));
                    }
                    if !post(Reply::Icons(pane)) {
                        return;
                    }
                }
                for _ in 0..3 {
                    if missing.is_empty() {
                        break;
                    }
                    // Waits on the mailbox, so a new request is not held up by the retry.
                    let (lock, cv) = &*worker;
                    let waiting = lock.lock().unwrap();
                    let waiting = cv
                        .wait_timeout_while(waiting, icons::RETRY, |m| m.pending.is_none() && !m.closed)
                        .unwrap()
                        .0;
                    if waiting.pending.is_some() || waiting.closed {
                        break;
                    }
                    drop(waiting);
                    for (key, directory) in &missing {
                        icons::ensure_type(key, *directory);
                    }
                    let before = missing.len();
                    missing.retain(|(key, _)| icons::get(key).is_none());
                    if missing.len() < before && !post(Reply::Icons(pane)) {
                        return;
                    }
                }
            }
        });
        Self { mail }
    }
    fn submit(&self, job: Job) {
        let (lock, cv) = &*self.mail;
        lock.lock().unwrap().pending = Some(job);
        cv.notify_one();
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let (lock, cv) = &*self.mail;
        lock.lock().unwrap().closed = true;
        cv.notify_one();
    }
}
struct Pane {
    model: Rc<RowsModel>,
    worker: Worker,
    generation: u64,
    pending: Option<Mode>,
    /// A change notification arrived while a listing was in flight.
    stale: bool,
    watch: Option<shell::Watch>,
    back: Vec<Saved>,
    forward: Vec<Saved>,
    column: usize,
    descending: bool,
    prefix: String,
    typed: Instant,
    /// Ctrl+Shift+F: the text and its pattern. Cleared when another folder opens.
    filter: Option<(String, listing::Pattern)>,
    /// Ctrl+Alt+Shift+F: the walk of the tree, kept while the search bar or results are open.
    finder: Option<Session>,
    /// Ctrl+Alt+Shift+F: the list shows search results instead of the folder.
    found: Option<Found>,
}
struct Session {
    id: u64,
    finder: finder::Finder,
    root: PathBuf,
    /// Too many items to keep in memory: queries run on Enter only.
    overflow: bool,
}
/// Search results of query `id`; they grow while the walk runs.
struct Found {
    id: u64,
    text: String,
    root: PathBuf,
    /// The folder view to return to.
    saved: Saved,
    /// Results not in the list yet: they wait while a sort or filter job is in flight,
    /// because that job's snapshot would replace them.
    backlog: Vec<Entry>,
    running: bool,
    /// The query is answered: sort once the backlog is in. Some(hit the limit).
    finished: Option<bool>,
    /// The list still shows the previous query (or the folder) until the first results
    /// come in, so typing does not blank it on every key.
    replace: bool,
}
/// Ctrl+Shift+D: two panes compared row by row.
struct Comparing {
    /// Old ("-", left / upper) and new ("+") pane.
    panes: [usize; 2],
    /// Second Ctrl+Shift+D: rows that are the same are left out.
    only_diff: bool,
    /// The listings as loaded; the panes show the rows the comparison keeps.
    base: [Arc<Snapshot>; 2],
    /// Sort order before the comparison, which shows both sides by name.
    sort: [(usize, bool); 2],
    counts: [usize; 4],
}
/// Typing in the recursive search bar: the query runs once the keys pause this long.
const TYPING: Duration = Duration::from_millis(120);
/// The find bar is open for `pane`.
struct Finding {
    pane: usize,
    /// 1 search (Ctrl+F), 2 filter (Ctrl+Shift+F), 3 recursive search (Ctrl+Alt+Shift+F).
    mode: i32,
    /// Entry the incremental search starts from.
    origin: usize,
}
struct Controller {
    ui: slint::Weak<App>,
    /// Always four; the layout (2, 3 or 4 panes) decides how many are shown.
    panes: [Pane; 4],
    active: usize,
    operations: usize,
    /// Conflict questions from paste workers; the front one is on screen.
    asking: VecDeque<(usize, paste::Question, AnswerTx)>,
    config: config::Config,
    /// Places being edited in the settings dialog.
    /// 0..4 toolbar places, 4..6 start folders (left, right).
    settings: [Option<PathBuf>; 7],
    /// Colors and font being edited in the settings dialog.
    settings_colors: Vec<slint::Color>,
    settings_ext: Vec<(String, slint::Color)>,
    settings_font: (String, f32),
    drives: Arc<Vec<Drive>>,
    menus: Option<ctxmenu::Menus>,
    /// Pane the open context menu belongs to.
    menu_pane: usize,
    finding: Option<Finding>,
    /// Last Ctrl+F text, for Ctrl+G and the next Ctrl+F.
    search: String,
    /// Last recursive search text and the id of the latest search.
    tree_search: String,
    searches: u64,
    typing: slint::Timer,
    /// `finder::INDEX_LIMIT`; the smoke test lowers it.
    index_limit: usize,
    naming: Option<Naming>,
    compare: Option<Comparing>,
    /// The pane active before the current one: what Ctrl+Shift+D compares with.
    previous: usize,
    /// `--demo`: drives and volume names (this machine's) are not shown.
    demo: bool,
}
fn display(path: &Path) -> String {
    shell::shell_path(path).to_string_lossy().into_owned()
}
fn hwnd(ui: &App) -> Option<HWND> {
    let handle = ui.window().window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}
impl Controller {
    fn message(&self, message: impl Into<slint::SharedString>) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_message(message.into());
        }
    }
    fn data(&self, p: usize) -> Arc<Snapshot> {
        self.panes[p].model.data.borrow().clone()
    }
    fn saved(&self, p: usize) -> Saved {
        let data = self.data(p);
        let selection = self.panes[p].model.selection.borrow().clone();
        Saved {
            path: data.dir.clone(),
            current: selection
                .current
                .and_then(|i| data.rows.get(i))
                .map(|e| e.path.clone()),
            anchor: selection
                .anchor
                .and_then(|i| data.rows.get(i))
                .map(|e| e.path.clone()),
            selection,
        }
    }
    fn selected_paths(&self, p: usize) -> Vec<PathBuf> {
        let data = self.data(p);
        let selection = self.panes[p].model.selection.borrow();
        data.rows
            .iter()
            .filter(|e| selection.contains(&e.path))
            .map(|e| e.path.as_ref().clone())
            .collect()
    }
    fn active(&mut self, p: usize) {
        if p != self.active {
            self.previous = self.active;
        }
        self.active = p;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_active_pane(p as i32);
            ui.set_active_hidden(self.config.show_hidden[p]);
        }
    }
    fn busy(&self, p: usize, busy: bool) {
        if let Some(ui) = self.ui.upgrade() {
            match p {
                0 => ui.set_p0_busy(busy),
                1 => ui.set_p1_busy(busy),
                2 => ui.set_p2_busy(busy),
                _ => ui.set_p3_busy(busy),
            }
        }
    }
    fn set_path(&self, p: usize, path: String) {
        if let Some(ui) = self.ui.upgrade() {
            let path = path.into();
            match p {
                0 => ui.set_p0_path(path),
                1 => ui.set_p1_path(path),
                2 => ui.set_p2_path(path),
                _ => ui.set_p3_path(path),
            }
        }
    }
    /// Panes shown by the current layout.
    fn visible(&self) -> usize {
        self.config.panes
    }
    /// View > 2 / 3 / 4 panes. A hidden active pane hands focus to the first one.
    /// `columns`: the 3-pane layout with three tall panes side by side (else stacked).
    fn set_layout(&mut self, count: usize, columns: bool) {
        self.config.panes = count.clamp(2, 4);
        self.config.three_columns = columns;
        self.save_config();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_pane_count(self.config.panes as i32);
            ui.set_three_columns(columns);
        }
        if self.active >= self.config.panes {
            self.active(0);
        }
        if self.compare.as_ref().is_some_and(|c| c.panes[1] >= self.config.panes) {
            self.end_compare(None);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.invoke_focus_pane(self.active as i32);
        }
        let shape = match (self.config.panes, columns) {
            (3, false) => "（横長・縦に3段）",
            (3, true) => "（縦長・横に3列）",
            _ => "",
        };
        self.message(format!("{} ペイン{shape}", self.config.panes));
    }
    fn toggle_attributes(&mut self) {
        self.toggle_column(3);
    }
    /// View > 列: size, date, time, attributes, type (saved).
    fn toggle_column(&mut self, column: usize) {
        if let Some(shown) = self.config.columns.get_mut(column) {
            *shown = !*shown;
        }
        self.save_config();
        self.sync_columns();
    }
    fn sync_columns(&self) {
        if let Some(ui) = self.ui.upgrade() {
            let [size, date, time, attr, kind] = self.config.columns;
            ui.set_columns(Columns { size, date, time, attr, kind });
        }
    }
    /// Selected entries, or the entry under the cursor when nothing is selected.
    fn targets(&self, p: usize) -> Vec<PathBuf> {
        let selected = self.selected_paths(p);
        if !selected.is_empty() || self.panes[p].model.special.get().is_some() {
            return selected;
        }
        let data = self.data(p);
        let current = self.panes[p].model.selection.borrow().current;
        current.and_then(|i| data.rows.get(i)).map(|e| vec![e.path.as_ref().clone()]).unwrap_or_default()
    }
    /// Edit > 絶対パスのコピー / ファイル名のコピー: one per line.
    fn copy_text(&mut self, p: usize, names: bool) {
        let lines: Vec<String> = self
            .targets(p)
            .iter()
            .map(|path| if names { path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()) } else { display(path) })
            .collect();
        if lines.is_empty() {
            self.message("項目が選択されていません");
            return;
        }
        let text = lines.join("\r\n");
        let result = self
            .ui
            .upgrade()
            .and_then(|ui| hwnd(&ui))
            .ok_or_else(|| "ウィンドウがまだ準備できていません".to_string())
            .and_then(|h| shell::set_clipboard_text(h, &text));
        match result {
            Ok(()) => self.message(format!("{} 件の{}をコピーしました", lines.len(), if names { "ファイル名" } else { "パス" })),
            Err(e) => self.message(e),
        }
    }
    /// Edit > 同じ拡張子をすべて選択: the extension of the item under the cursor.
    fn select_same_extension(&mut self, p: usize) {
        let data = self.data(p);
        let model = self.panes[p].model.clone();
        let Some(current) = model.selection.borrow().current.filter(|_| model.special.get().is_none()) else {
            return;
        };
        let Some(entry) = data.rows.get(current).filter(|e| !e.directory) else { return };
        let ext = entry.ext.to_lowercase();
        let paths: Vec<_> = data
            .rows
            .iter()
            .filter(|e| !e.directory && e.ext.to_lowercase() == ext)
            .map(|e| e.path.clone())
            .collect();
        let count = paths.len();
        let mut selection = Selection::only(paths);
        selection.current = Some(current);
        selection.anchor = Some(current);
        *model.selection.borrow_mut() = selection;
        self.repaint(p);
        let label = if ext.is_empty() { "拡張子なし".to_string() } else { format!(".{ext}") };
        self.message(format!("{label} を {count} 件選択"));
    }
    /// The find bar for pane `p`, in `Finding::mode`.
    fn open_find(&mut self, p: usize, mode: i32) {
        // The compared rows are the two folders as they are: no filter or search results.
        if mode != 1 && self.compare_side(p).is_some() {
            self.message("比較中は使えません（Esc で比較を終了）");
            return;
        }
        let origin = self.panes[p].model.selection.borrow().current.unwrap_or(0);
        let text = match mode {
            2 => self.panes[p].filter.as_ref().map_or(String::new(), |(text, _)| text.clone()),
            3 => self.panes[p].found.as_ref().map_or_else(|| self.tree_search.clone(), |f| f.text.clone()),
            _ => self.search.clone(),
        };
        self.finding = Some(Finding { pane: p, mode, origin });
        // The walk starts now, so the first keys find a good part of the tree read.
        if mode == 3 {
            self.session(p, false);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_find_mode(mode);
            ui.set_find_text(text.into());
            ui.invoke_focus_find();
        }
    }
    /// Search moves the cursor while typing; the filter and the recursive search wait for Enter.
    fn find_edited(&mut self, text: &str) {
        if let Some(Finding { pane, mode: 3, .. }) = self.finding {
            self.query_typed(pane, text);
            return;
        }
        let Some(Finding { pane, mode: 1, origin }) = self.finding else { return };
        let Some(pattern) = listing::Pattern::new(text) else { return };
        self.search = text.trim().to_string();
        let found = self.find(pane, &pattern, origin, true);
        self.message(if found { String::new() } else { format!("見つかりません: {}", text.trim()) });
    }
    fn find_done(&mut self, ok: bool, text: &str) {
        let Some(finding) = self.finding.take() else { return };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_find_mode(0);
            ui.invoke_focus_pane(finding.pane as i32);
        }
        if finding.mode == 3 {
            // Enter shows the text at once; Esc goes back to the folder.
            self.typing.stop();
            if ok {
                self.query(finding.pane, text);
            } else {
                self.close_found(finding.pane);
            }
            if self.panes[finding.pane].found.is_none() {
                self.panes[finding.pane].finder = None;
            }
            return;
        }
        if !ok {
            return;
        }
        if finding.mode == 2 {
            self.set_filter(finding.pane, text);
        } else if listing::Pattern::new(text).is_some() {
            self.search = text.trim().to_string();
            if self.ui.upgrade().is_some_and(|ui| ui.get_message().is_empty()) {
                self.message(format!("検索: {}（Ctrl+G で次へ、Ctrl+Shift+G で前へ）", self.search));
            }
        }
    }
    /// Ctrl+G in the search bar: further typing then refines from the new match.
    fn find_next_typing(&mut self, forward: bool) {
        let Some(Finding { pane, mode: 1, .. }) = self.finding else { return };
        self.find_next(pane, forward);
        let current = self.panes[pane].model.selection.borrow().current;
        if let (Some(finding), Some(current)) = (&mut self.finding, current) {
            finding.origin = current;
        }
    }
    /// Ctrl+G / Ctrl+Shift+G: the next / previous match of the last search.
    fn find_next(&mut self, p: usize, forward: bool) {
        let Some(pattern) = listing::Pattern::new(&self.search) else {
            self.message("Ctrl+F で検索してください");
            return;
        };
        let count = self.data(p).rows.len();
        if count == 0 {
            return;
        }
        let model = &self.panes[p].model;
        let current = model.selection.borrow().current.filter(|_| model.special.get().is_none());
        let start = match current {
            Some(i) if forward => i + 1,
            Some(i) => i + count - 1,
            None if forward => 0,
            None => count - 1,
        };
        if !self.find(p, &pattern, start % count, forward) {
            self.message(format!("見つかりません: {}", self.search));
        }
    }
    /// Puts the cursor on the first entry matching `pattern`, from `start` on (wrapping).
    fn find(&mut self, p: usize, pattern: &listing::Pattern, start: usize, forward: bool) -> bool {
        let data = self.data(p);
        let count = data.rows.len();
        let Some(index) = (0..count)
            .map(|step| if forward { (start + step) % count } else { (start + count - step) % count })
            .find(|&i| pattern.matches(&data.rows[i].name_fold))
        else {
            return false;
        };
        let model = &self.panes[p].model;
        model.special.set(None);
        model.selection.borrow_mut().select(&data.rows, index, false, false, false);
        self.reveal(p, index);
        self.repaint(p);
        true
    }
    /// Scrolls entry `index` into view (model rows are shifted by the ".." row; the first
    /// entry also reveals "..", so Home shows the top of the list).
    fn reveal(&self, p: usize, index: usize) {
        let model = &self.panes[p].model;
        let row = if index == 0 && model.compare.borrow().is_none() { 0 } else { model.entry_row(index) };
        self.reveal_row(p, row);
    }
    fn reveal_row(&self, p: usize, row: usize) {
        let model = &self.panes[p].model;
        if model.show_page_of(row) {
            model.notify.reset();
        }
    }
    /// Page size follows the list height (window resize, font size).
    fn page_resized(&mut self, p: usize, size: usize) {
        let model = self.panes[p].model.clone();
        if size == 0 || size == model.page_size.get() {
            return;
        }
        model.page_size.set(size);
        model.show_page_of(self.cursor_row(p));
        model.notify.reset();
        self.repaint(p);
    }
    /// Global row of the cursor.
    fn cursor_row(&self, p: usize) -> usize {
        let model = &self.panes[p].model;
        match model.special.get() {
            Some(special) => model.special_row(special),
            None => model.selection.borrow().current.map_or(0, |i| model.entry_row(i)),
        }
    }
    /// Mouse wheel: one page per notch, the cursor moves along.
    fn page_step(&mut self, p: usize, forward: bool) {
        self.active(p);
        let key = if forward { slint::platform::Key::PageDown } else { slint::platform::Key::PageUp };
        let mut event = slint::language::KeyEvent::default();
        event.text = key.into();
        self.key(p, event);
    }
    /// Breadcrumb segments ("D:", "Game", ...) and the ancestor each one opens.
    fn crumbs(dir: &Path) -> Vec<(String, PathBuf)> {
        let mut crumbs: Vec<_> = dir
            .ancestors()
            .map(|a| {
                let label = a.file_name().map_or_else(
                    || display(a).trim_end_matches('\\').to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                (label, a.to_path_buf())
            })
            .collect();
        crumbs.reverse();
        crumbs
    }
    fn info(&self, p: usize) -> PaneInfo {
        let data = self.data(p);
        let volume = data.volume.as_ref().filter(|_| !self.demo);
        let folders = data.rows.iter().filter(|e| e.directory).count();
        let total: u64 = data.rows.iter().filter(|e| !e.directory).map(|e| e.size).sum();
        let selected = self.panes[p].model.selection.borrow().count();
        PaneInfo {
            folders: folders.to_string().into(),
            files: (data.rows.len() - folders).to_string().into(),
            total: listing::format_size(total).into(),
            volume: volume.map_or(String::new(), |v| v.label.clone()).into(),
            free: volume.map_or(String::new(), |v| listing::format_size(v.free)).into(),
            selected: if selected > 0 { selected.to_string() } else { String::new() }.into(),
            filter: self.panes[p].filter.as_ref().map_or(String::new(), |(text, _)| text.clone()).into(),
            search: self.panes[p].found.as_ref().map_or(String::new(), |f| f.text.clone()).into(),
            compare: match self.compare.as_ref().filter(|c| c.panes.contains(&p)) {
                Some(c) => {
                    let [old, new, differ, same] = c.counts;
                    let only = if c.only_diff { "（差分のみ）" } else { "" };
                    format!("-{old} +{new} ≠{differ} ={same}{only}")
                }
                None => String::new(),
            }
            .into(),
            page: {
                let (page, pages) = self.panes[p].model.pages();
                if pages > 1 { format!("{page}/{pages}") } else { String::new() }.into()
            },
        }
    }
    fn repaint(&self, p: usize) {
        self.paint(p);
        // Comparing, the other pane's cursor and page follow the active one.
        let Some(c) = self.compare.as_ref().filter(|c| p == self.active && c.panes.contains(&p)) else { return };
        let other = if c.panes[0] == p { c.panes[1] } else { c.panes[0] };
        let row = self.cursor_row(p);
        let model = &self.panes[other].model;
        match model.slot(row) {
            Some(Slot::Entry(i)) => {
                model.special.set(None);
                model.selection.borrow_mut().current = Some(i);
            }
            _ => model.special.set(Some(Special::Gap(row))),
        }
        let page = self.panes[p].model.page_start.get();
        if model.page_start.get() != page {
            model.page_start.set(page);
            model.notify.reset();
        }
        self.paint(other);
    }
    fn paint(&self, p: usize) {
        if let Some(ui) = self.ui.upgrade() {
            // Update the rows of the page in place; a reset would destroy mouse/double-click state.
            let model = &self.panes[p].model;
            for i in 0..model.row_count() {
                model.notify.row_changed(i);
            }
            let info = self.info(p);
            match p {
                0 => ui.set_p0_info(info),
                1 => ui.set_p1_info(info),
                2 => ui.set_p2_info(info),
                _ => ui.set_p3_info(info),
            }
        }
    }
    fn navigate(&mut self, p: usize, path: PathBuf, mode: Mode) {
        let show_hidden = self.config.show_hidden[p];
        let pane = &mut self.panes[p];
        // Any listing ends the search: its results are not a folder that can be re-read.
        // Back to the searched folder keeps the walk for the search bar still open.
        pane.found = None;
        if !matches!(mode, Mode::Return(_)) {
            pane.finder = None;
        }
        // Opening a folder drops the filter (see `complete`); refreshes keep it.
        let filter = match mode {
            Mode::New(_) | Mode::Back | Mode::Forward | Mode::Return(_) => None,
            _ => pane.filter.as_ref().map(|(_, f)| f.clone()),
        };
        pane.generation += 1;
        pane.pending = Some(mode);
        pane.worker.submit(Job {
            generation: pane.generation,
            path,
            source: None,
            column: pane.column,
            descending: pane.descending,
            show_hidden,
            filter,
        });
        self.busy(p, true);
    }
    fn typed_path(&mut self, p: usize, text: String) {
        self.active(p);
        let path = PathBuf::from(text.trim().trim_matches('"'));
        let path = if path.is_absolute() {
            path
        } else {
            self.data(p).dir.join(path)
        };
        self.navigate(p, path, Mode::New(None));
    }
    fn command(&mut self, p: usize, c: i32) {
        self.active(p);
        let data = self.data(p);
        match c {
            0 => {
                if let Some(saved) = self.panes[p].back.last() {
                    self.navigate(p, saved.path.clone(), Mode::Back);
                }
            }
            1 => {
                if let Some(saved) = self.panes[p].forward.last() {
                    self.navigate(p, saved.path.clone(), Mode::Forward);
                }
            }
            // In search results ".." and F5 go back to the folder and search again.
            2 if self.panes[p].found.is_some() => self.close_found(p),
            3 if self.panes[p].found.is_some() => {
                let text = self.panes[p].found.as_ref().map(|f| f.text.clone()).unwrap_or_default();
                self.search_tree(p, &text);
            }
            2 => {
                if let Some(parent) = data.dir.parent() {
                    self.navigate(
                        p,
                        parent.to_path_buf(),
                        Mode::New(Some(Arc::new(data.dir.clone()))),
                    );
                }
            }
            3 => self.navigate(p, data.dir.clone(), Mode::Refresh),
            4 => self.set_path(p, display(&data.dir)),
            _ => {}
        }
    }
    fn sort(&mut self, p: usize, column: usize) {
        if self.compare_side(p).is_some() {
            self.message("比較中は名前順で表示します");
            return;
        }
        if self.panes[p]
            .pending
            .as_ref()
            .is_some_and(|m| !matches!(m, Mode::Sort))
        {
            return;
        }
        let pane = &mut self.panes[p];
        pane.descending = if pane.column == column {
            !pane.descending
        } else {
            false
        };
        pane.column = column;
        let order = if pane.descending { "降順" } else { "昇順" };
        let key = ["名前", "サイズ", "更新日時", "拡張子"][column.min(3)];
        self.rearrange(p);
        self.message(format!("{key}の{order}で並べ替え"));
    }
    /// Sorts and filters the entries already in memory again (no disk read).
    fn rearrange(&mut self, p: usize) {
        let source = self.data(p);
        let show_hidden = self.config.show_hidden[p];
        let pane = &mut self.panes[p];
        pane.generation += 1;
        pane.pending = Some(Mode::Sort);
        pane.worker.submit(Job {
            generation: pane.generation,
            path: source.dir.clone(),
            source: Some(source),
            column: pane.column,
            descending: pane.descending,
            show_hidden,
            filter: pane.filter.as_ref().map(|(_, f)| f.clone()),
        });
        self.busy(p, true);
    }
    /// Ctrl+Shift+F (empty text: Esc) sets or clears the filter of pane `p`.
    fn set_filter(&mut self, p: usize, text: &str) {
        let filter = listing::Pattern::new(text).map(|f| (text.trim().to_string(), f));
        if filter.is_none() && self.panes[p].filter.is_none() {
            return;
        }
        self.message(match &filter {
            Some((text, _)) => format!("フィルター: {text}（Esc で解除）"),
            None => "フィルターを解除しました".to_string(),
        });
        self.panes[p].filter = filter;
        match &self.panes[p].pending {
            // A folder is loading: re-read it with the new filter once it is in.
            Some(mode) if !matches!(mode, Mode::Sort) => self.panes[p].stale = true,
            _ => self.rearrange(p),
        }
        self.repaint(p);
    }
    /// The walk below the folder shown (or below the root of the results), started when the
    /// search bar opens and reused for every query until `fresh` asks to read the disk again.
    fn session(&mut self, p: usize, fresh: bool) -> bool {
        let root = match &self.panes[p].found {
            Some(found) => found.root.clone(),
            None => self.data(p).dir.clone(),
        };
        if root.as_os_str().is_empty() {
            return false;
        }
        if !fresh && self.panes[p].finder.as_ref().is_some_and(|s| s.root == root) {
            return true;
        }
        self.searches += 1;
        let id = self.searches;
        let (show_hidden, follow) = (self.config.show_hidden[p], self.config.search_links);
        let finder = finder::Finder::start(root.clone(), show_hidden, follow, self.index_limit, move |event| {
            post(Reply::Finder(p, id, event))
        });
        self.panes[p].finder = Some(Session { id, finder, root, overflow: false });
        true
    }
    /// Walks the tree again (F5, Alt+Z) and searches `text`.
    fn search_tree(&mut self, p: usize, text: &str) {
        if !self.session(p, true) {
            return;
        }
        if let Some(found) = &mut self.panes[p].found {
            found.text.clear();
        }
        self.query(p, text);
    }
    /// Typing in the recursive search bar: the query waits for a pause in the keys.
    /// Past the index limit every query reads the disk, so it waits for Enter.
    fn query_typed(&mut self, p: usize, text: &str) {
        if self.panes[p].finder.as_ref().is_some_and(|s| s.overflow) {
            return;
        }
        let text = text.to_string();
        self.typing.start(slint::TimerMode::SingleShot, TYPING, move || {
            if let Some(c) = CONTROLLER.with(|c| c.borrow().clone()) {
                let mut c = c.borrow_mut();
                if c.finding.as_ref().is_some_and(|f| f.pane == p && f.mode == 3) {
                    c.query(p, &text);
                }
            }
        });
    }
    /// Shows what matches `text` below the folder in pane `p`; empty text goes back.
    fn query(&mut self, p: usize, text: &str) {
        let text = text.trim();
        let Some(pattern) = listing::Pattern::new(text) else {
            self.close_found(p);
            return;
        };
        if self.panes[p].found.as_ref().is_some_and(|f| f.text == text) {
            return;
        }
        if self.panes[p].pending.as_ref().is_some_and(|m| !matches!(m, Mode::Sort)) {
            self.message("フォルダーの読み込み中です");
            return;
        }
        if !self.session(p, false) {
            return;
        }
        self.searches += 1;
        let id = self.searches;
        self.tree_search = text.to_string();
        let (dir, saved) = (self.data(p).dir.clone(), self.saved(p));
        let pane = &mut self.panes[p];
        if let Some(session) = &pane.finder {
            session.finder.query(id, pattern);
        }
        // A sort in flight is of the old results: its reply is dropped by the new generation.
        pane.generation += 1;
        pane.pending = None;
        pane.stale = false;
        match &mut pane.found {
            Some(found) => {
                found.id = id;
                found.text = text.to_string();
                found.backlog.clear();
                found.running = true;
                found.finished = None;
                found.replace = true;
            }
            None => {
                pane.filter = None;
                pane.found = Some(Found {
                    id,
                    text: text.to_string(),
                    root: dir,
                    saved,
                    backlog: Vec::new(),
                    running: true,
                    finished: None,
                    replace: true,
                });
            }
        }
        self.busy(p, true);
        self.repaint(p);
    }
    /// Leaves the search results for the folder as it was (Esc, "..", Backspace).
    fn close_found(&mut self, p: usize) {
        // The walk stays while the bar is open: the next keys search it again.
        if !self.finding.as_ref().is_some_and(|f| f.pane == p && f.mode == 3) {
            self.panes[p].finder = None;
        }
        let Some(found) = self.panes[p].found.take() else { return };
        if found.running {
            self.message("検索を中止しました");
        }
        self.navigate(p, found.root, Mode::Return(found.saved));
    }
    fn found(&mut self, p: usize, id: u64, batch: Vec<Entry>, done: Option<bool>) {
        let pane = &mut self.panes[p];
        let Some(found) = pane.found.as_mut().filter(|f| f.id == id) else { return };
        found.backlog.extend(batch);
        if done.is_some() {
            found.running = false;
            found.finished = done;
        }
        if pane.pending.is_none() {
            self.flush_found(p);
        }
    }
    /// Adds the results that came in to the list; once the query is answered, sorts them.
    fn flush_found(&mut self, p: usize) {
        let pane = &mut self.panes[p];
        let Some(found) = pane.found.as_mut() else { return };
        let batch = std::mem::take(&mut found.backlog);
        let finished = found.finished.take();
        let (text, running, root) = (found.text.clone(), found.running, found.root.clone());
        let replace = found.replace && (!batch.is_empty() || finished.is_some());
        if replace {
            found.replace = false;
        }
        let model = &pane.model;
        let filter = pane.filter.as_ref().map(|(_, f)| f);
        if replace {
            let (rows, unfiltered) = match filter {
                Some(filter) => (batch.iter().filter(|e| filter.matches_entry(e)).cloned().collect(), Some(Arc::new(batch))),
                None => (batch, None),
            };
            let volume = model.data.borrow().volume.clone();
            let mut snapshot = Snapshot::new(root, rows, volume);
            snapshot.unfiltered = unfiltered;
            let mut selection = Selection::default();
            selection.current = (!snapshot.rows.is_empty()).then_some(0);
            model.special.set((snapshot.rows.is_empty() && snapshot.offset() == 1).then_some(Special::Parent));
            *model.selection.borrow_mut() = selection;
            *model.data.borrow_mut() = Arc::new(snapshot);
            model.page_start.set(0);
            model.notify.reset();
        } else if !batch.is_empty() {
            let first = {
                let mut data = model.data.borrow_mut();
                let snapshot = Arc::make_mut(&mut data);
                let shown: Vec<Entry> = match (&mut snapshot.unfiltered, filter) {
                    (Some(all), Some(filter)) => {
                        let shown = batch.iter().filter(|e| filter.matches_entry(e)).cloned().collect();
                        Arc::make_mut(all).extend(batch);
                        shown
                    }
                    _ => batch,
                };
                let start = snapshot.rows.len();
                let positions = Arc::make_mut(&mut snapshot.positions);
                for (i, e) in shown.iter().enumerate() {
                    positions.insert(e.path.clone(), start + i);
                }
                snapshot.rows.extend(shown);
                if model.selection.borrow().current.is_none() && !snapshot.rows.is_empty() {
                    model.selection.borrow_mut().current = Some(0);
                    model.special.set(None);
                }
                start + snapshot.offset()
            };
            // Rows past the page shown change nothing on screen (a reset would lose clicks).
            if first < model.page_start.get() + model.page_size.get().max(1) {
                model.notify.reset();
            }
        }
        match finished {
            Some(limited) => {
                let count = self.data(p).rows.len();
                self.message(if limited {
                    format!("検索: {text} … {} 件で打ち切りました（Esc で戻る）", finder::RESULT_LIMIT)
                } else {
                    format!("検索: {text} … {count} 件（Esc で戻る）")
                });
                // The final sort; its reply ends the busy state.
                self.rearrange(p);
            }
            None => self.busy(p, running || self.panes[p].pending.is_some()),
        }
        self.repaint(p);
    }
    /// Ctrl+Shift+D: compare -> only the differences -> off.
    fn toggle_compare(&mut self) {
        match &mut self.compare {
            None => self.start_compare(),
            Some(c) if !c.only_diff => {
                c.only_diff = true;
                self.recompare();
                self.message("差分のみ表示（Ctrl+Shift+D でもう一度押すと比較を終了）");
            }
            Some(_) => self.end_compare(None),
        }
    }
    /// The active pane with the one active before it; the lower pane number is the old side.
    fn start_compare(&mut self) {
        let a = self.active;
        let b = if self.previous != a && self.previous < self.visible() { self.previous } else { (a + 1) % self.visible() };
        let panes = [a.min(b), a.max(b)];
        if panes.iter().any(|&p| self.panes[p].found.is_some()) {
            self.message("検索結果は比較できません");
            return;
        }
        if panes.iter().any(|&p| self.panes[p].pending.as_ref().is_some_and(|m| !matches!(m, Mode::Sort))) {
            self.message("フォルダーの読み込み中です");
            return;
        }
        let sort = panes.map(|p| (self.panes[p].column, self.panes[p].descending));
        self.compare = Some(Comparing { panes, only_diff: false, base: panes.map(|p| self.data(p)), sort, counts: [0; 4] });
        // Both sides by name, so the rows line up; the reply of the sort compares.
        for p in panes {
            if (self.panes[p].column, self.panes[p].descending) != (0, false) {
                self.panes[p].column = 0;
                self.panes[p].descending = false;
                self.rearrange(p);
            }
        }
        self.recompare();
        self.message("比較中（Ctrl+Shift+D で差分のみ、Esc で終了）");
    }
    fn compare_side(&self, p: usize) -> Option<usize> {
        self.compare.as_ref().and_then(|c| c.panes.iter().position(|&q| q == p))
    }
    /// Lines up the two listings again (after any reload of either side).
    fn recompare(&mut self) {
        let Some(c) = &mut self.compare else { return };
        if c.panes.iter().any(|&p| self.panes[p].pending.is_some()) {
            return;
        }
        let (aligned, shown) = compare::align(&c.base[0].rows, &c.base[1].rows, c.only_diff);
        c.counts = aligned.counts;
        let aligned = Rc::new(aligned);
        // The cursor stays on its item (or its row, when the item is gone).
        let side = if self.active == c.panes[1] { 1 } else { 0 };
        let model = &self.panes[c.panes[side]].model;
        let old_row = match model.special.get() {
            Some(special) => model.special_row(special),
            None => model.selection.borrow().current.map_or(0, |i| model.entry_row(i)),
        };
        let focus = (model.special.get().is_none())
            .then(|| model.selection.borrow().current.and_then(|i| model.data.borrow().rows.get(i).map(|e| e.path.clone())))
            .flatten();
        for (s, rows) in shown.into_iter().enumerate() {
            let p = c.panes[s];
            let base = &c.base[s];
            let snapshot = Snapshot::new(base.dir.clone(), rows, base.volume.clone());
            let model = &self.panes[p].model;
            let old = model.selection.borrow().clone();
            *model.selection.borrow_mut() = Selection::only(snapshot.rows.iter().filter(|e| old.contains(&e.path)).map(|e| e.path.clone()));
            *model.data.borrow_mut() = Arc::new(snapshot);
            *model.compare.borrow_mut() = Some((aligned.clone(), s));
        }
        let row = focus
            .and_then(|path| self.panes[c.panes[side]].model.data.borrow().positions.get(&path).copied())
            .and_then(|i| aligned.row_of(side, i))
            .unwrap_or(old_row)
            .min(aligned.pairs.len().saturating_sub(1));
        let panes = c.panes;
        for p in panes {
            let model = &self.panes[p].model;
            match model.slot(row) {
                Some(Slot::Entry(i)) => {
                    model.special.set(None);
                    model.selection.borrow_mut().current = Some(i);
                }
                _ => model.special.set(Some(Special::Gap(row))),
            }
            model.show_page_of(row);
            model.notify.reset();
            self.paint(p);
        }
    }
    /// Back to the folders as loaded. `keep`: a pane that opened another folder (its list
    /// is already the new one).
    fn end_compare(&mut self, keep: Option<usize>) {
        let Some(c) = self.compare.take() else { return };
        for (s, &p) in c.panes.iter().enumerate() {
            let model = &self.panes[p].model;
            *model.compare.borrow_mut() = None;
            if keep == Some(p) {
                continue;
            }
            let current = (model.special.get().is_none())
                .then(|| model.selection.borrow().current.and_then(|i| model.data.borrow().rows.get(i).map(|e| e.path.clone())))
                .flatten();
            let base = c.base[s].clone();
            {
                let mut selection = model.selection.borrow_mut();
                selection.current = current
                    .and_then(|path| base.positions.get(&path).copied())
                    .or_else(|| (!base.rows.is_empty()).then_some(0));
                selection.anchor = None;
            }
            model.special.set((base.rows.is_empty() && base.offset() == 1).then_some(Special::Parent));
            *model.data.borrow_mut() = base;
            model.page_start.set(0);
            model.notify.reset();
            if c.sort[s] != (0, false) {
                (self.panes[p].column, self.panes[p].descending) = c.sort[s];
                self.rearrange(p);
            } else if let Some(i) = self.panes[p].model.selection.borrow().current {
                self.reveal(p, i);
            }
            self.paint(p);
        }
        self.message("比較を終了しました");
    }
    /// Breadcrumb click: open that ancestor with the folder we came from focused.
    fn crumb(&mut self, p: usize, i: usize) {
        self.active(p);
        let crumbs = Self::crumbs(&self.data(p).dir);
        let Some((_, target)) = crumbs.get(i) else { return };
        let focus = crumbs.get(i + 1).map(|(_, path)| Arc::new(path.clone()));
        self.navigate(p, target.clone(), Mode::New(focus));
    }
    /// Toolbar places (desktop, documents, pictures, videos, music) open in the active pane.
    fn place(&mut self, i: usize) {
        match self.config.place(i) {
            Some(path) => self.navigate(self.active, path, Mode::New(None)),
            None => self.message("このフォルダーは設定されていません"),
        }
    }
    /// Settings dialog (File > 設定): edits a copy of the places until saved.
    fn open_settings(&mut self) {
        let [desktop, documents, pictures, videos, music] = self.config.places.clone();
        let [left, right] = self.config.start.clone();
        self.settings = [desktop, documents, pictures, videos, music, left, right];
        self.settings_rows();
        self.settings_colors = self.config.colors.clone();
        self.settings_ext = self.config.extension_colors.clone();
        self.settings_font = (self.config.font_family.clone(), self.config.font_size);
        self.settings_color_rows();
        if let Some(ui) = self.ui.upgrade() {
            if ui.get_fonts().row_count() == 0 {
                let fonts: Vec<slint::SharedString> = theme::font_families().into_iter().map(Into::into).collect();
                ui.set_fonts(ModelRc::new(slint::VecModel::from(fonts)));
            }
            ui.set_settings_font_family(self.settings_font.0.clone().into());
            ui.set_settings_font_size(self.settings_font.1.round() as i32);
            ui.set_settings_opacity((self.config.opacity * 100.0).round() as i32);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_settings_open(true);
            ui.invoke_focus_settings();
        }
        self.apply_opacity();
    }
    /// Rebuilds the rows, so edited fields show new values (browse / reset).
    fn settings_rows(&self) {
        let labels = ["デスクトップ", "ドキュメント", "画像", "動画", "音楽", "起動時の左", "起動時の右"];
        let sandbox = sandbox_dir();
        let rows: Vec<PlaceRow> = (0..7)
            .map(|i| PlaceRow {
                label: labels[i].into(),
                value: self.settings[i].as_ref().map_or(String::new(), |p| display(p)).into(),
                fallback: match (i, &sandbox) {
                    (5, Some(d)) => display(&d.join("left")),
                    (6, Some(d)) => display(&d.join("right")),
                    // Released builds start at the Desktop / Documents (see `default_start`).
                    (5 | 6, None) => config::known_folder(i - 5).map_or(String::new(), |p| display(&p)),
                    _ => config::known_folder(i).map_or(String::new(), |p| display(&p)),
                }
                .into(),
            })
            .collect();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_settings_places(ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    fn setting_edited(&mut self, i: usize, text: &str) {
        let text = text.trim().trim_matches('"');
        self.settings[i] = (!text.is_empty()).then(|| PathBuf::from(text));
    }
    fn setting_reset(&mut self, i: usize) {
        self.settings[i] = None;
        self.settings_rows();
    }
    /// The Windows folder picker runs on its own thread so the window keeps responding.
    fn setting_browse(&mut self, i: usize) {
        let start = self.settings[i].clone().or_else(|| if i < 5 { config::known_folder(i) } else { None });
        std::thread::spawn(move || {
            if let Some(path) = shell::pick_folder(start.as_deref()) {
                post(Reply::Picked(i, path));
            }
        });
    }
    /// Copies colors, extension colors and font from the config into the UI.
    fn apply_appearance(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let theme = ui.global::<Theme>();
        for ((key, _, _), color) in theme::BASE.iter().zip(&self.config.colors) {
            let color = *color;
            match *key {
                "pane" => theme.set_pane(color),
                "text" => theme.set_text(color),
                "folder-text" => theme.set_folder_text(color),
                "drive-text" => theme.set_drive_text(color),
                "readonly-text" => theme.set_readonly_text(color),
                "hidden-text" => theme.set_hidden_text(color),
                "size-text" => theme.set_size_text(color),
                "time-text" => theme.set_time_text(color),
                "attr-text" => theme.set_attr_text(color),
                "diff-add-bg" => theme.set_diff_add_bg(color),
                "diff-del-bg" => theme.set_diff_del_bg(color),
                "diff-change-bg" => theme.set_diff_change_bg(color),
                "selected-text" => theme.set_selected_text(color),
                "cursor-text" => theme.set_cursor_text(color),
                "cursor-selected-bg" => theme.set_cursor_selected_bg(color),
                "cursor-inactive-bg" => theme.set_cursor_inactive_bg(color),
                "cursor-inactive-line" => theme.set_cursor_inactive_line(color),
                "dim" => theme.set_dim(color),
                "border" => theme.set_border(color),
                "dialog" => theme.set_dialog(color),
                "kind" => theme.set_kind(color),
                "selected-bg" => theme.set_selected_bg(color),
                "cursor-bg" => theme.set_cursor_bg(color),
                "accent" => theme.set_accent(color),
                "chrome" => theme.set_chrome(color),
                "bar" => theme.set_bar(color),
                _ => {}
            }
        }
        ui.set_font_family(self.config.font_family.clone().into());
        ui.set_font_size(self.config.font_size);
        let map = Arc::new(theme::extension_map(&self.config.extension_colors));
        for pane in &self.panes {
            *pane.model.ext_colors.borrow_mut() = map.clone();
            pane.model.notify.reset();
        }
    }
    fn settings_color_rows(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let base: Vec<ColorRow> = theme::BASE
            .iter()
            .zip(&self.settings_colors)
            .map(|((_, label, _), color)| ColorRow {
                label: (*label).into(),
                color: *color,
                r: color.red() as i32,
                g: color.green() as i32,
                b: color.blue() as i32,
            })
            .collect();
        let ext: Vec<ExtRow> = self
            .settings_ext
            .iter()
            .map(|(exts, color)| ExtRow {
                exts: exts.clone().into(),
                color: *color,
                r: color.red() as i32,
                g: color.green() as i32,
                b: color.blue() as i32,
            })
            .collect();
        ui.set_settings_colors(ModelRc::new(slint::VecModel::from(base)));
        ui.set_settings_ext(ModelRc::new(slint::VecModel::from(ext)));
    }
    fn color_base(&mut self, i: usize, color: slint::Color) {
        if let Some(c) = self.settings_colors.get_mut(i) {
            *c = color;
        }
        self.settings_color_rows();
    }
    /// `i` out of range adds a rule.
    fn color_ext(&mut self, i: usize, exts: &str, color: slint::Color) {
        let exts = theme::split_extensions(exts).iter().map(|e| e.to_uppercase()).collect::<Vec<_>>().join(";");
        if exts.is_empty() {
            self.message("拡張子を入力してください");
            return;
        }
        match self.settings_ext.get_mut(i) {
            Some(rule) => *rule = (exts, color),
            None => self.settings_ext.push((exts, color)),
        }
        self.settings_color_rows();
    }
    fn color_ext_delete(&mut self, i: usize) {
        if i < self.settings_ext.len() {
            self.settings_ext.remove(i);
        }
        self.settings_color_rows();
    }
    fn colors_reset(&mut self) {
        self.settings_colors = config::Config::default_colors();
        self.settings_ext = config::Config::default_extension_colors();
        self.settings_color_rows();
    }
    fn font_set(&mut self, family: &str, size: f32) {
        if !family.trim().is_empty() {
            self.settings_font = (family.trim().to_string(), size.clamp(8.0, 32.0));
        }
    }
    fn settings_done(&mut self, save: bool) {
        if save {
            let [desktop, documents, pictures, videos, music, left, right] = self.settings.clone();
            self.config.places = [desktop, documents, pictures, videos, music];
            self.config.start = [left, right];
            if let Some(ui) = self.ui.upgrade() {
                self.config.opacity = ui.get_settings_opacity() as f32 / 100.0;
            }
            self.config.colors = self.settings_colors.clone();
            self.config.extension_colors = self.settings_ext.clone();
            (self.config.font_family, self.config.font_size) = self.settings_font.clone();
            self.apply_appearance();
            self.save_config();
            self.message("設定を保存しました");
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_settings_open(false);
            ui.invoke_focus_pane(self.active as i32);
        }
        self.apply_opacity();
    }
    /// A splitter was released: keep every layout's positions.
    fn layout_committed(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        self.config.split = ui.get_split().clamp(0.15, 0.85);
        self.config.split3_columns = [ui.get_col_a(), ui.get_col_b()];
        self.config.split3_rows = [ui.get_row_a(), ui.get_row_b()];
        self.config.split4 = [ui.get_quad_x(), ui.get_quad_y()];
        self.save_config();
    }
    fn sync_bookmarks(&self) {
        if let Some(ui) = self.ui.upgrade() {
            let rows: Vec<BookmarkRow> = self
                .config
                .bookmarks
                .iter()
                .map(|b| BookmarkRow {
                    name: b.name.clone().into(),
                    path: b.path.to_string_lossy().into_owned().into(),
                })
                .collect();
            ui.set_bookmarks(ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    /// The window is closing: keep its placement for the next start. While maximized the
    /// restored rectangle is unknown, so the previous one is kept.
    fn remember_window(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let window = ui.window();
        if window.is_minimized() {
            return;
        }
        let placement = if window.is_maximized() {
            match self.config.window {
                Some(previous) => config::Placement { maximized: true, ..previous },
                None => return,
            }
        } else {
            let (position, size) = (window.position(), window.size());
            config::Placement { x: position.x, y: position.y, width: size.width, height: size.height, maximized: false }
        };
        if self.config.window != Some(placement) {
            self.config.window = Some(placement);
            self.save_config();
        }
    }
    fn save_config(&mut self) {
        if let Err(e) = self.config.save() {
            self.message(format!("設定を保存できません: {e}"));
        }
        self.sync_bookmarks();
    }
    fn open_bookmark(&mut self, i: usize) {
        if let Some(b) = self.config.bookmarks.get(i) {
            let path = b.path.clone();
            self.navigate(self.active, path, Mode::New(None));
        }
    }
    /// Adds the active pane's folder; it can be renamed or reordered in the edit dialog.
    fn add_bookmark(&mut self) {
        // The saved path has no `\\?\`, so it must still name this folder without it.
        let dir = match shell::os_path(&self.data(self.active).dir) {
            Ok(dir) => dir,
            Err(e) => return self.message(e),
        };
        if dir.as_os_str().is_empty() {
            return;
        }
        let name = dir
            .file_name()
            .map_or_else(|| display(&dir), |n| n.to_string_lossy().into_owned());
        self.message(format!("ブックマークに追加: {name}"));
        self.config.bookmarks.push(config::Bookmark { name, path: dir });
        self.save_config();
    }
    fn edit_bookmark(&mut self, i: usize) {
        let Some(b) = self.config.bookmarks.get(i) else { return };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_bookmark_index(i as i32);
            ui.set_bookmark_name(b.name.clone().into());
            ui.set_bookmark_path(b.path.to_string_lossy().into_owned().into());
            ui.set_bookmark_editing(true);
            ui.invoke_focus_bookmark();
        }
    }
    fn close_bookmark_dialog(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_bookmark_editing(false);
            ui.invoke_focus_pane(self.active as i32);
        }
    }
    fn save_bookmark(&mut self, i: usize, name: &str, path: &str) {
        let (name, path) = (name.trim(), path.trim().trim_matches('"'));
        if name.is_empty() || path.is_empty() {
            self.message("名前とパスを入力してください");
            return;
        }
        if let Some(b) = self.config.bookmarks.get_mut(i) {
            *b = config::Bookmark { name: name.into(), path: path.into() };
            self.save_config();
        }
        self.close_bookmark_dialog();
    }
    fn delete_bookmark(&mut self, i: usize) {
        if i < self.config.bookmarks.len() {
            let removed = self.config.bookmarks.remove(i);
            self.message(format!("ブックマークを削除: {}", removed.name));
            self.save_config();
        }
        self.close_bookmark_dialog();
    }
    fn move_bookmark(&mut self, i: usize, down: bool) {
        let j = if down { i + 1 } else { i.wrapping_sub(1) };
        if i < self.config.bookmarks.len() && j < self.config.bookmarks.len() {
            self.config.bookmarks.swap(i, j);
            self.save_config();
            if let Some(ui) = self.ui.upgrade() {
                ui.set_bookmark_index(j as i32);
            }
        }
    }
    fn handle(&mut self, reply: Reply) {
        match reply {
            Reply::Listing(p, generation, result) => self.complete(p, generation, result),
            Reply::Finder(p, session, event) => {
                let Some(current) = self.panes[p].finder.as_mut().filter(|s| s.id == session) else { return };
                match event {
                    finder::Event::Found(id, batch, done) => self.found(p, id, batch, done),
                    finder::Event::Overflow => {
                        current.overflow = true;
                        self.message(format!(
                            "{} 件を超えるため入力中の絞り込みを止めました。Enter で検索します",
                            self.index_limit
                        ));
                    }
                    finder::Event::Icons => self.repaint(p),
                }
            }
            Reply::Changed(p, dir) => {
                // Search results stay as found; F5 searches again.
                if self.data(p).dir != dir || self.panes[p].found.is_some() {
                    return;
                }
                if self.panes[p].pending.is_some() {
                    self.panes[p].stale = true;
                } else {
                    self.navigate(p, dir, Mode::Refresh);
                }
            }
            Reply::Ask(p, question, reply) => self.ask(p, question, reply),
            Reply::Pasted(p, dir, before, targets, result) => {
                self.operations -= 1;
                match result {
                    Ok(outcome) => {
                        let verb = if outcome.moved { "移動" } else { "コピー" };
                        self.message(if outcome.cancelled {
                            "貼り付けを取り消しました".to_string()
                        } else if outcome.aborted {
                            format!("{verb}は途中で中止されました")
                        } else if outcome.inside > 0 {
                            format!("フォルダーをその中へは貼り付けられないため {} 件をスキップしました → {}", outcome.inside, display(&dir))
                        } else if outcome.skipped {
                            format!("{verb}しました（一部スキップ） → {}", display(&dir))
                        } else {
                            format!("{verb}しました → {}", display(&dir))
                        });
                    }
                    Err(e) => self.message(format!("貼り付けできません: {e}")),
                }
                if self.data(p).dir == dir && self.panes[p].pending.is_none() && self.panes[p].found.is_none() {
                    self.navigate(p, dir, Mode::Pasted(before, Arc::new(targets)));
                }
            }
            Reply::Deleted(result) => {
                self.operations -= 1;
                match result {
                    Ok(false) => self.message("削除しました"),
                    Ok(true) => self.message("削除は中止またはスキップを含みます"),
                    Err(e) => self.message(format!("削除できません: {e}")),
                }
            }
            Reply::Icons(p) => self.repaint(p),
            Reply::Drives(_) if self.demo => {}
            Reply::Drives(drives) => self.set_drives(drives),
            Reply::Menu(outcome) => {
                let p = self.menu_pane;
                match outcome {
                    ctxmenu::Outcome::Paste => self.paste(p),
                    ctxmenu::Outcome::Rename => {
                        let data = self.data(p);
                        let current = self.panes[p].model.selection.borrow().current;
                        if let Some(entry) = current.and_then(|i| data.rows.get(i)) {
                            self.open_name(p, NameKind::Rename(entry.path.as_ref().clone()));
                        }
                    }
                    ctxmenu::Outcome::Done => {}
                    ctxmenu::Outcome::Failed(e) => self.message(format!("メニューを実行できません: {e}")),
                }
            }
            Reply::Made(p, dir, before, result) => match result {
                Ok(path) => {
                    self.message(format!("完了: {}", display(&path)));
                    if self.data(p).dir == dir && self.panes[p].pending.is_none() && self.panes[p].found.is_none() {
                        self.navigate(p, dir, Mode::Pasted(before, Arc::new(vec![path])));
                    }
                }
                Err(e) => self.message(format!("できませんでした: {e}")),
            },
            Reply::Picked(i, path) => {
                if self.ui.upgrade().is_some_and(|ui| ui.get_settings_open()) {
                    self.settings[i] = Some(path);
                    self.settings_rows();
                }
            }
            Reply::Opened(result) => {
                if let Err(e) = result {
                    self.message(format!("開けません: {e}"));
                }
            }
        }
    }
    fn complete(&mut self, p: usize, generation: u64, result: Result<Arc<Snapshot>, String>) {
        if generation != self.panes[p].generation {
            return;
        }
        let mode = self.panes[p].pending.take().unwrap_or(Mode::Refresh);
        let focus_list = matches!(&mode, Mode::New(_) | Mode::Back | Mode::Forward | Mode::Return(_));
        self.busy(p, false);
        let data = match result {
            Ok(data) => data,
            Err(e) => {
                self.message(format!("フォルダを開けません: {e}"));
                return;
            }
        };
        if focus_list {
            // The job for an opened folder ran without the filter.
            self.panes[p].filter = None;
        }
        // Comparing: this pane shows its plain list until `recompare` lines it up again.
        *self.panes[p].model.compare.borrow_mut() = None;
        let old = self.saved(p);
        let old_current = self.panes[p].model.selection.borrow().current;
        let changed = old.path != data.dir;
        let restore = match mode {
            Mode::Back => {
                let saved = self.panes[p].back.pop();
                self.panes[p].forward.push(old.clone());
                saved
            }
            Mode::Forward => {
                let saved = self.panes[p].forward.pop();
                self.panes[p].back.push(old.clone());
                saved
            }
            Mode::Refresh | Mode::Sort => Some(old.clone()),
            Mode::Return(saved) => Some(saved),
            Mode::Pasted(before, targets) => {
                let added: Vec<_> = data
                    .rows
                    .iter()
                    .filter(|e| !before.contains_key(&e.path) || targets.contains(&e.path))
                    .map(|e| e.path.clone())
                    .collect();
                if added.is_empty() {
                    Some(old.clone())
                } else {
                    Some(Saved {
                        path: data.dir.clone(),
                        current: added.first().cloned(),
                        anchor: added.first().cloned(),
                        selection: Selection::only(added),
                    })
                }
            }
            Mode::New(focus) => {
                if changed && !old.path.as_os_str().is_empty() {
                    self.panes[p].back.push(old.clone());
                    self.panes[p].forward.clear();
                }
                if !changed {
                    Some(old.clone())
                } else {
                    focus.map(|path| Saved {
                        path: data.dir.clone(),
                        selection: Selection::only([path.clone()]),
                        current: Some(path),
                        anchor: None,
                    })
                }
            }
        };
        let mut selection = restore
            .as_ref()
            .map(|s| s.selection.clone())
            .unwrap_or_default();
        selection.current = restore
            .as_ref()
            .and_then(|s| s.current.as_ref())
            .and_then(|p| data.positions.get(p).copied())
            // The focused item was removed (moved away, deleted): stay at the same position.
            .or_else(|| (!changed).then_some(old_current).flatten())
            .map(|i| i.min(data.rows.len().saturating_sub(1)))
            .or_else(|| (!data.rows.is_empty()).then_some(0))
            .filter(|_| !data.rows.is_empty());
        selection.anchor = restore
            .as_ref()
            .and_then(|s| s.anchor.as_ref())
            .and_then(|p| data.positions.get(p).copied());
        selection
            .exceptions
            .retain(|p| data.positions.contains_key(p));
        selection.deferred = None;
        selection.selected_count = if selection.all {
            data.rows.iter().filter(|e| selection.contains(&e.path)).count()
        } else {
            selection.exceptions.len()
        };
        // Keep the cursor on ".." or a drive across refresh; an empty folder puts it on "..".
        let drives = self.drives.len();
        let special = if data.offset() == 1 && data.rows.is_empty() {
            Some(Special::Parent)
        } else if changed {
            None
        } else {
            self.panes[p].model.special.get().filter(|s| match s {
                Special::Parent => data.offset() == 1,
                Special::Drive(i) => *i < drives,
                Special::Gap(_) => false,
            })
        };
        let reveal = if special.is_some() { None } else { selection.current };
        let pane = &mut self.panes[p];
        pane.model.special.set(special);
        *pane.model.selection.borrow_mut() = selection;
        *pane.model.data.borrow_mut() = data.clone();
        if changed {
            pane.model.page_start.set(0);
        }
        pane.model.show_page_of(pane.model.page_start.get());
        pane.model.notify.reset();
        if pane.back.len() > 64 {
            pane.back.remove(0);
        }
        if pane.forward.len() > 64 {
            pane.forward.remove(0);
        }
        if changed || pane.watch.is_none() {
            let dir = data.dir.clone();
            pane.watch = shell::watch(data.dir.clone(), move || {
                post(Reply::Changed(p, dir.clone()));
            });
        }
        let stale = std::mem::take(&mut pane.stale);
        self.set_path(p, display(&data.dir));
        if let Some(ui) = self.ui.upgrade() {
            let crumbs: Vec<slint::SharedString> =
                Self::crumbs(&data.dir).into_iter().map(|(label, _)| label.into()).collect();
            let crumbs = ModelRc::new(slint::VecModel::from(crumbs));
            match p {
                0 => ui.set_p0_crumbs(crumbs),
                1 => ui.set_p1_crumbs(crumbs),
                2 => ui.set_p2_crumbs(crumbs),
                _ => ui.set_p3_crumbs(crumbs),
            }
            if focus_list && self.active == p {
                ui.invoke_focus_pane(p as i32);
            }
        }
        if let Some(i) = reveal {
            self.reveal(p, i);
        }
        if let Some(special) = special {
            self.reveal_row(p, self.panes[p].model.special_row(special));
        }
        self.repaint(p);
        // Comparing: another folder ends it; a reload (watch, paste, F5) compares again.
        if let Some(side) = self.compare_side(p) {
            if changed {
                self.end_compare(Some(p));
            } else {
                if let Some(c) = &mut self.compare {
                    c.base[side] = data.clone();
                }
                self.recompare();
            }
        }
        if stale {
            self.navigate(p, data.dir.clone(), Mode::Refresh);
        }
        // Results that came in during a sort or filter of the search results.
        if self.panes[p].found.is_some() {
            self.flush_found(p);
        }
    }
    /// `row` is a model row. kind: 0 press, 1 release, 2 double click, 3 right click.
    fn pointer(&mut self, p: usize, row: usize, ctrl: bool, shift: bool, kind: i32) {
        self.active(p);
        let row = row + self.panes[p].model.page_start.get();
        let data = self.data(p);
        let model = self.panes[p].model.clone();
        let index = match model.slot(row) {
            None => return,
            Some(Slot::Entry(index)) => index,
            Some(slot) => {
                // "..", a drive or an empty compare row: a click puts the cursor there
                // (a plain click clears the selection).
                let special = match slot {
                    Slot::Drive(i) => Special::Drive(i),
                    Slot::Gap => Special::Gap(row),
                    _ => Special::Parent,
                };
                match kind {
                    0 | 3 => {
                        model.special.set(Some(special));
                        if !ctrl && !shift {
                            model.selection.borrow_mut().clear();
                        }
                        self.repaint(p);
                        if kind == 3 {
                            let target = match special {
                                Special::Drive(i) => ctxmenu::Target::Items(vec![self.drives[i].root.clone()]),
                                Special::Parent | Special::Gap(_) => ctxmenu::Target::Background(data.dir.clone()),
                            };
                            self.show_menu(p, target, shift);
                        }
                    }
                    2 => self.enter_special(p, special),
                    _ => {}
                }
                return;
            }
        };
        model.special.set(None);
        let mut selection = model.selection.borrow_mut();
        match kind {
            0 => selection.select(&data.rows, index, ctrl, shift, true),
            1 => selection.release(&data.rows),
            2 => {
                drop(selection);
                self.activate(p, index);
                return;
            }
            3 => {
                // Right click: an unselected item becomes the selection, as in Explorer.
                if !selection.contains(&data.rows[index].path) {
                    selection.select(&data.rows, index, false, false, false);
                }
                selection.current = Some(index);
                drop(selection);
                self.repaint(p);
                let items = self.selected_paths(p);
                self.show_menu(p, ctxmenu::Target::Items(items), shift);
                return;
            }
            _ => {}
        }
        drop(selection);
        self.repaint(p);
    }
    /// Enter / double click on "..", or on a drive in the drive list.
    fn enter_special(&mut self, p: usize, special: Special) {
        match special {
            Special::Parent => self.command(p, 2),
            Special::Drive(i) => {
                if let Some(drive) = self.drives.get(i) {
                    let root = drive.root.clone();
                    self.navigate(p, root, Mode::New(None));
                }
            }
            Special::Gap(_) => {}
        }
    }
    fn show_menu(&mut self, p: usize, target: ctxmenu::Target, extended: bool) {
        let owner = self.ui.upgrade().and_then(|ui| hwnd(&ui)).map_or(0, |h| h.0 as isize);
        self.menu_pane = p;
        match &self.menus {
            Some(menus) => menus.show(ctxmenu::Request { target, owner, extended }),
            None => self.message("コンテキストメニューを開けません"),
        }
    }
    /// Right click on the empty part of the list: the folder background menu.
    fn blank_menu(&mut self, p: usize, extended: bool) {
        self.active(p);
        let dir = self.data(p).dir.clone();
        if !dir.as_os_str().is_empty() {
            self.show_menu(p, ctxmenu::Target::Background(dir), extended);
        }
    }
    /// Name dialog for rename (F2), new file (Ctrl+N) or new folder (F7).
    fn open_name(&mut self, p: usize, kind: NameKind) {
        let data = self.data(p);
        if data.dir.as_os_str().is_empty() {
            return;
        }
        // Search results come from many folders: rename only, in the item's own folder.
        let dir = match &kind {
            NameKind::Rename(path) => path.parent().map_or_else(|| data.dir.clone(), Path::to_path_buf),
            _ if self.panes[p].found.is_some() => {
                self.message("検索結果では作成できません（Esc で戻る）");
                return;
            }
            _ => data.dir.clone(),
        };
        let mut taken: HashSet<String> = data
            .rows
            .iter()
            .filter(|e| e.path.parent() == Some(dir.as_path()))
            .map(|e| e.name_fold.clone())
            .collect();
        let unique = |name: &str, folder: bool, taken: &HashSet<String>| {
            if taken.contains(&name.to_lowercase()) {
                paste::auto_name(name, folder, taken)
            } else {
                name.to_string()
            }
        };
        let (title, initial, folder) = match &kind {
            NameKind::Rename(path) => {
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                taken.remove(&name.to_lowercase());
                ("名前の変更", name, path.is_dir())
            }
            NameKind::NewFile => ("新規ファイル", unique("新しいテキスト ドキュメント.txt", false, &taken), false),
            NameKind::NewFolder => ("新規フォルダー", unique("新しいフォルダー", true, &taken), true),
        };
        let stem = paste::stem_len(&initial, folder);
        self.naming = Some(Naming {
            pane: p,
            dir,
            kind,
            taken,
            before: data.positions.clone(),
        });
        if let Some(ui) = self.ui.upgrade() {
            ui.set_name_title(title.into());
            ui.set_name_initial(initial.into());
            ui.set_name_stem(stem as i32);
            ui.set_name_open(true);
            ui.invoke_focus_name();
        }
        self.apply_opacity();
    }
    fn name_error(&self, name: &str) -> &'static str {
        self.naming
            .as_ref()
            .and_then(|n| paste::name_error(name, &n.taken))
            .unwrap_or("")
    }
    fn name_done(&mut self, ok: bool, name: String) {
        if ok && !self.name_error(&name).is_empty() {
            return;
        }
        let Some(naming) = self.naming.take() else { return };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_name_open(false);
            ui.invoke_focus_pane(naming.pane as i32);
        }
        self.apply_opacity();
        if !ok {
            return;
        }
        let Naming { pane, dir, kind, before, .. } = naming;
        if let NameKind::Rename(old) = &kind {
            if old.file_name().is_some_and(|n| n == name.as_str()) {
                return;
            }
        }
        std::thread::spawn(move || {
            let target = dir.join(&name);
            let result = match kind {
                NameKind::Rename(old) => shell::rename(&old, &name).map(|_| target),
                NameKind::NewFolder => std::fs::create_dir(&target).map(|_| target).map_err(|e| e.to_string()),
                NameKind::NewFile => std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .map(|_| target)
                    .map_err(|e| e.to_string()),
            };
            post(Reply::Made(pane, dir, before, result));
        });
    }
    /// New drive list from the drive thread: shown under the entries in both panes.
    fn set_drives(&mut self, drives: Arc<Vec<Drive>>) {
        let same_count = drives.len() == self.drives.len();
        self.drives = drives.clone();
        for pane in &self.panes {
            let model = &pane.model;
            *model.drives.borrow_mut() = drives.clone();
            if let Some(Special::Drive(i)) = model.special.get() {
                if i >= drives.len() {
                    model.special.set(None);
                }
            }
            // Compared panes show no drive list.
            if model.compare.borrow().is_some() {
                continue;
            }
            if same_count {
                // Only the numbers changed: update those rows in place (keeps click state).
                let data = model.data.borrow();
                let start = data.offset() + data.rows.len();
                let page = model.page_start.get()..model.page_start.get() + model.row_count();
                for i in 0..drives.len() {
                    if page.contains(&(start + i)) {
                        model.notify.row_changed(start + i - page.start);
                    }
                }
            } else {
                model.notify.reset();
            }
        }
    }
    /// Alt+Z: show or hide hidden files (saved in the config).
    /// Alt+Z: hidden files on or off for one pane (saved).
    fn toggle_hidden(&mut self, p: usize) {
        self.config.show_hidden[p] = !self.config.show_hidden[p];
        self.save_config();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_active_hidden(self.config.show_hidden[self.active]);
        }
        let side = if p == 0 { "左" } else { "右" };
        let state = if self.config.show_hidden[p] { "表示" } else { "非表示" };
        self.message(format!("隠しファイルを{state}（{side}ペイン）"));
        if let Some(text) = self.panes[p].found.as_ref().map(|f| f.text.clone()) {
            self.search_tree(p, &text);
            return;
        }
        self.panes[p].finder = None;
        let dir = self.data(p).dir.clone();
        if !dir.as_os_str().is_empty() {
            self.navigate(p, dir, Mode::Refresh);
        }
    }
    /// Edit > 検索でリンク先も辿る (saved); results on screen keep their walk until F5.
    fn toggle_search_links(&mut self) {
        self.config.search_links = !self.config.search_links;
        self.save_config();
        for pane in &mut self.panes {
            if pane.found.is_none() {
                pane.finder = None;
            }
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_search_links(self.config.search_links);
        }
        self.message(if self.config.search_links {
            "検索でジャンクションとシンボリックリンクの先も辿ります"
        } else {
            "検索でジャンクションとシンボリックリンクの先は辿りません"
        });
    }
    /// Path bar > パスのコピー.
    fn copy_path(&mut self, p: usize) {
        let path = display(&self.data(p).dir);
        let result = self
            .ui
            .upgrade()
            .and_then(|ui| hwnd(&ui))
            .ok_or_else(|| "ウィンドウがまだ準備できていません".to_string())
            .and_then(|h| shell::set_clipboard_text(h, &path));
        match result {
            Ok(()) => self.message(format!("パスをコピー: {path}")),
            Err(e) => self.message(e),
        }
    }
    /// The settings and name dialogs are always shown opaque; the configured opacity returns when they close.
    fn apply_opacity(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        if let Some(h) = hwnd(&ui) {
            let opaque = ui.get_settings_open() || ui.get_name_open();
            shell::set_opacity(h, if opaque { 1.0 } else { self.config.opacity });
        }
    }
    fn activate(&mut self, p: usize, index: usize) {
        if self.panes[p].pending.is_some() {
            return;
        }
        let data = self.data(p);
        let Some(entry) = data.rows.get(index) else {
            return;
        };
        if entry.directory {
            self.navigate(p, entry.path.as_ref().clone(), Mode::New(None));
        } else {
            let path = entry.path.as_ref().clone();
            std::thread::spawn(move || post(Reply::Opened(shell::open(&path))));
        }
    }
    fn copy(&mut self, p: usize, cut: bool) {
        let paths = self.selected_paths(p);
        if paths.is_empty() {
            self.message("項目が選択されていません");
            return;
        }
        let result = self
            .ui
            .upgrade()
            .and_then(|ui| hwnd(&ui))
            .ok_or_else(|| "ウィンドウがまだ準備できていません".to_string())
            .and_then(|h| shell::set_clipboard(h, &paths, cut));
        match result {
            Ok(()) => self.message(format!(
                "{} 件を{}しました",
                paths.len(),
                if cut { "切り取り" } else { "コピー" }
            )),
            Err(e) => self.message(e),
        }
    }
    fn paste(&mut self, p: usize) {
        let data = self.data(p);
        if data.dir.as_os_str().is_empty() {
            return;
        }
        if !self.asking.is_empty() {
            return;
        }
        if self.panes[p].found.is_some() {
            self.message("検索結果には貼り付けできません（Esc で戻る）");
            return;
        }
        let dir = data.dir.clone();
        let before = data.positions.clone();
        self.operations += 1;
        self.message(format!("貼り付け中 → {}", display(&dir)));
        std::thread::spawn(move || paste_job(p, dir, before));
    }
    fn ask(&mut self, p: usize, question: paste::Question, reply: AnswerTx) {
        self.asking.push_back((p, question, reply));
        if self.asking.len() == 1 {
            self.show_question();
        }
    }
    fn show_question(&mut self) {
        let Some((_, q, _)) = self.asking.front() else {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_ask_open(false);
                ui.invoke_focus_pane(self.active as i32);
            }
            return;
        };
        let detail = |m: &paste::Meta, newer: bool| {
            let kind = if m.directory {
                "フォルダー".to_string()
            } else {
                format!("{} バイト", m.size)
            };
            let date = chrono::DateTime::<chrono::Local>::from(m.modified).format("%Y-%m-%d %H:%M:%S");
            format!("{kind}  ·  {date}{}", if newer { "  （新しい）" } else { "" })
        };
        let name = q.target.file_name().unwrap().to_string_lossy().into_owned();
        let info = AskInfo {
            replace: q.replace,
            kind: match q.kind {
                paste::Kind::File => 0,
                paste::Kind::Folder => 1,
                paste::Kind::Mismatch => 2,
            },
            stem: paste::stem_len(&q.auto_name, q.from.directory) as i32,
            existing_path: display(&q.target).into(),
            existing_detail: detail(&q.to, q.to.modified > q.from.modified).into(),
            incoming_label: if q.moved { "移動する項目:" } else { "コピーする項目:" }.into(),
            incoming_path: display(&q.source).into(),
            incoming_detail: detail(&q.from, q.from.modified > q.to.modified).into(),
            rename: q.auto_name.clone().into(),
            name: name.into(),
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_ask(info);
            ui.set_ask_open(true);
            ui.invoke_focus_ask();
        }
    }
    fn validate(&self, name: &str) -> &'static str {
        self.asking
            .front()
            .and_then(|(_, q, _)| paste::name_error(name, &q.taken))
            .unwrap_or("")
    }
    fn answer(&mut self, code: i32, name: String, all: bool) {
        use paste::{Answer, Kind};
        let Some((_, q, _)) = self.asking.front() else { return };
        let answer = match (code, q.kind) {
            (0, Kind::File) => Answer::Overwrite,
            (1, Kind::File) => Answer::Newer,
            (2, _) => Answer::Skip,
            (3, _) if self.validate(&name).is_empty() => Answer::Rename(name),
            (4, Kind::Folder) => Answer::Merge,
            (5, Kind::Folder) if q.replace => Answer::Replace,
            (6, _) => Answer::Cancel,
            _ => return,
        };
        let (_, _, reply) = self.asking.pop_front().unwrap();
        let _ = reply.send((answer, all));
        self.show_question();
    }
    fn delete(&mut self, p: usize, permanent: bool) {
        let paths = self.selected_paths(p);
        if paths.is_empty() {
            self.message("項目が選択されていません");
            return;
        }
        self.operations += 1;
        std::thread::spawn(move || post(Reply::Deleted(shell::delete(&paths, permanent))));
    }
    fn key(&mut self, p: usize, event: slint::language::KeyEvent) -> bool {
        use slint::platform::Key;
        self.active(p);
        let data = self.data(p);
        let is = |k: Key| event.text == slint::SharedString::from(k);
        let ctrl = event.modifiers.control;
        let shift = event.modifiers.shift;
        let alt = event.modifiers.alt;
        let letter = event.text.to_lowercase();
        if is(Key::Tab) {
            // Next shown pane, in order.
            let other = (p + 1) % self.visible();
            self.active(other);
            if let Some(ui) = self.ui.upgrade() {
                ui.invoke_focus_pane(other as i32);
            }
            return true;
        }
        // Ctrl+Shift+2 / 3 / 4: layouts. 3 again switches stacked <-> side by side.
        // Shift turns the digit into a symbol on most layouts (JP: " #  US: @ # 197121.
        if ctrl && shift && !alt {
            let layout = match letter.as_str() {
                "2" | "\"" | "@" => Some(2),
                "3" | "#" => Some(3),
                "4" | "$" => Some(4),
                _ => None,
            };
            if let Some(count) = layout {
                // First press: tall panes side by side; again: wide panes stacked; and so on.
                let columns = count == 3 && !(self.config.panes == 3 && self.config.three_columns);
                self.set_layout(count, columns);
                return true;
            }
        }
        // Ctrl+Shift+D: compare with the pane active before, only differences, off.
        if ctrl && shift && !alt && letter == "d" {
            self.toggle_compare();
            return true;
        }
        // Ctrl+Shift+N/S/T/E: sort by name, size, time, extension; again for reverse order.
        if ctrl && shift && !alt {
            let column = match letter.as_str() {
                "n" => Some(0),
                "s" => Some(1),
                "t" => Some(2),
                "e" => Some(3),
                _ => None,
            };
            if let Some(column) = column {
                self.sort(p, column);
                return true;
            }
        }
        // Ctrl+F search, Ctrl+Shift+F filter, Ctrl+Alt+Shift+F search below this folder,
        // Ctrl+G / Ctrl+Shift+G next / previous match.
        if ctrl && alt && shift && letter == "f" {
            self.open_find(p, 3);
            return true;
        }
        if ctrl && !alt && letter == "f" {
            self.open_find(p, if shift { 2 } else { 1 });
            return true;
        }
        if ctrl && !alt && letter == "g" {
            self.find_next(p, !shift);
            return true;
        }
        if ctrl && !alt {
            match letter.as_str() {
                "l" => {
                    if let Some(ui) = self.ui.upgrade() {
                        ui.invoke_focus_address(p as i32);
                    }
                    return true;
                }
                "c" => {
                    self.copy(p, false);
                    return true;
                }
                "x" => {
                    self.copy(p, true);
                    return true;
                }
                "v" => {
                    self.paste(p);
                    return true;
                }
                "a" => {
                    self.panes[p]
                        .model
                        .selection
                        .borrow_mut()
                        .select_all(data.positions.clone());
                    self.repaint(p);
                    return true;
                }
                _ => {}
            }
        }
        if ctrl && !alt && letter == "n" {
            self.open_name(p, NameKind::NewFile);
            return true;
        }
        if is(Key::F7) {
            self.open_name(p, NameKind::NewFolder);
            return true;
        }
        if alt && !ctrl && letter == "z" {
            self.toggle_hidden(p);
            return true;
        }
        if is(Key::Delete) && !ctrl && !alt {
            self.delete(p, shift);
            return true;
        }
        if is(Key::F5) {
            self.command(p, 3);
            return true;
        }
        if alt && is(Key::LeftArrow) {
            self.command(p, 0);
            return true;
        }
        if alt && is(Key::RightArrow) {
            self.command(p, 1);
            return true;
        }
        if (alt && is(Key::UpArrow)) || is(Key::Backspace) {
            self.command(p, 2);
            return true;
        }
        if is(Key::Escape) {
            if self.compare_side(p).is_some() {
                self.end_compare(None);
                return true;
            }
            if self.panes[p].filter.is_some() {
                self.set_filter(p, "");
                return true;
            }
            if self.panes[p].found.is_some() {
                self.close_found(p);
                return true;
            }
            self.panes[p].model.selection.borrow_mut().clear();
            self.panes[p].prefix.clear();
            self.repaint(p);
            return true;
        }
        // The cursor moves over model rows: "..", the entries, then the drives.
        let model = self.panes[p].model.clone();
        let rows = model.total();
        if rows == 0 {
            return false;
        }
        let entries = data.rows.len();
        // With no entry to rest on, the cursor sits on ".." or the first drive.
        let comparing = model.compare.borrow().is_some();
        let special = model.special.get().or_else(|| {
            (entries == 0).then(|| match () {
                _ if comparing => Special::Gap(0),
                _ if data.offset() == 1 => Special::Parent,
                _ => Special::Drive(0),
            })
        });
        let current = model
            .selection
            .borrow()
            .current
            .unwrap_or(0)
            .min(entries.saturating_sub(1));
        if is(Key::Return) {
            match special {
                Some(special) => self.enter_special(p, special),
                None => self.activate(p, current),
            }
            return true;
        }
        if is(Key::F2) {
            if special.is_none() && entries > 0 {
                let path = data.rows[current].path.as_ref().clone();
                self.open_name(p, NameKind::Rename(path));
            }
            return true;
        }
        let cursor = match special {
            Some(special) => model.special_row(special),
            None => model.entry_row(current),
        };
        let page = model.page_size.get().max(1);
        let target = if is(Key::UpArrow) {
            Some(cursor.saturating_sub(1))
        } else if is(Key::DownArrow) {
            Some((cursor + 1).min(rows - 1))
        } else if is(Key::Home) {
            Some(0)
        } else if is(Key::End) {
            Some(rows - 1)
        } else if is(Key::PageUp) || is(Key::LeftArrow) {
            Some(cursor.saturating_sub(page))
        } else if is(Key::PageDown) || is(Key::RightArrow) {
            Some((cursor + page).min(rows - 1))
        } else {
            None
        };
        if let Some(row) = target {
            self.panes[p].prefix.clear();
            match model.slot(row) {
                Some(Slot::Entry(index)) => {
                    model.special.set(None);
                    model.selection.borrow_mut().select(&data.rows, index, ctrl, shift, false);
                    self.reveal(p, index);
                }
                Some(slot) => {
                    model.special.set(Some(match slot {
                        Slot::Drive(i) => Special::Drive(i),
                        Slot::Gap => Special::Gap(row),
                        _ => Special::Parent,
                    }));
                    if !ctrl && !shift {
                        model.selection.borrow_mut().clear();
                    }
                    self.reveal_row(p, row);
                }
                None => {}
            }
            self.repaint(p);
            return true;
        }
        if special.is_some() && event.text == " " {
            return true;
        }
        if event.text == " " {
            let mut selection = self.panes[p].model.selection.borrow_mut();
            if ctrl {
                selection.toggle_current(&data.rows);
            } else {
                selection.select(&data.rows, current, false, shift, false);
            }
            drop(selection);
            self.repaint(p);
            return true;
        }
        // Shift+letter: that drive, when it exists (otherwise the letter searches names).
        if shift && !ctrl && !alt && letter.len() == 1 && letter.chars().all(|c| c.is_ascii_alphabetic()) {
            let prefix = format!("{}:", letter.to_uppercase());
            if let Some(root) = self.drives.iter().find(|d| d.root.to_string_lossy().starts_with(&prefix)).map(|d| d.root.clone()) {
                self.navigate(p, root, Mode::New(None));
                return true;
            }
        }
        if !ctrl && !alt && !event.text.is_empty() && !event.text.chars().any(char::is_control) {
            let pane = &mut self.panes[p];
            if pane.typed.elapsed() > Duration::from_millis(900) {
                pane.prefix.clear();
            }
            let cycle = pane.prefix == letter;
            if !cycle {
                pane.prefix.push_str(&letter);
            }
            pane.typed = Instant::now();
            let start = if cycle { current + 1 } else { current };
            if let Some(index) = (0..data.rows.len())
                .map(|offset| (start + offset) % data.rows.len())
                .find(|&i| data.rows[i].name_fold.starts_with(&pane.prefix))
            {
                pane.model.special.set(None);
                pane.model
                    .selection
                    .borrow_mut()
                    .select(&data.rows, index, false, false, false);
                self.reveal(p, index);
                self.repaint(p);
            }
            return true;
        }
        false
    }
}

#[derive(Default)]
struct PasteOutcome {
    moved: bool,
    aborted: bool,
    skipped: bool,
    cancelled: bool,
    /// Folders left out because the destination was the folder itself or inside it.
    inside: usize,
}
type AnswerTx = mpsc::SyncSender<(paste::Answer, bool)>;
/// Paste worker: read the clipboard, plan (asking the UI about each conflict), then run.
fn paste_job(p: usize, dir: PathBuf, before: Positions) {
    let ole = shell::Apartment::new();
    let result = ole.as_ref().map_err(Clone::clone).and_then(|_| run_paste(p, &dir));
    let (targets, result) = match result {
        Ok((targets, outcome)) => (targets, Ok(outcome)),
        Err(e) => (Vec::new(), Err(e)),
    };
    post(Reply::Pasted(p, dir, before, targets, result));
}
fn run_paste(p: usize, dir: &Path) -> Result<(Vec<PathBuf>, PasteOutcome), String> {
    let clip = shell::read_clip()?;
    let moved = clip.moved;
    let done = |aborted| (Vec::new(), PasteOutcome { moved, aborted, ..Default::default() });
    let Some(paths) = clip.paths else {
        // Shell items without paths: the Windows copy engine resolves conflicts itself.
        return shell::paste_shell_items(dir, moved).map(done);
    };
    let mut ids = pathid::Idents::default();
    let same_folder = !paths.is_empty() && paths.iter().all(|path| path.parent().is_some_and(|d| ids.same(d, dir)));
    if same_folder {
        if moved {
            return Err("切り取った項目は既にこのフォルダーにあります".into());
        }
        return shell::duplicate(&paths, dir).map(done);
    }
    // Blocks this worker until the user answers; a closed window counts as Cancel.
    let mut ask = |question: paste::Question| {
        let (tx, rx) = mpsc::sync_channel(1);
        if !post(Reply::Ask(p, question, tx)) {
            return (paste::Answer::Cancel, false);
        }
        rx.recv().unwrap_or((paste::Answer::Cancel, false))
    };
    let Some(plan) = paste::plan(&paths, dir, moved, &mut ask)? else {
        let cancelled = PasteOutcome { moved, cancelled: true, ..Default::default() };
        return Ok((Vec::new(), cancelled));
    };
    // `transfer` verifies the plan against the disk right before it changes anything.
    let aborted = shell::transfer(&plan)?;
    if moved && !aborted {
        plan.remove_emptied();
        if !plan.skipped {
            shell::finish_cut(&paths);
        }
    }
    let outcome = PasteOutcome { moved, aborted, skipped: plan.skipped, cancelled: false, inside: plan.inside };
    Ok((plan.targets, outcome))
}

/// The development sandbox folder, if running from the crate.
fn sandbox_dir() -> Option<PathBuf> {
    app_dir().map(|app| app.join("data").join("sandbox"))
}
/// The crate folder (`app`) when running from `app\target\<profile>`.
fn app_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("src").is_dir())
        .map(Path::to_path_buf)
}
/// Settings: `app\data\config.json` while developing (smoke runs use their own copy),
/// otherwise `config.json` next to the exe (portable: the unzipped folder).
fn config_file(smoke_root: Option<&Path>) -> PathBuf {
    if let Some(root) = smoke_root {
        return root.join("config.json");
    }
    if let Some(app) = app_dir() {
        return app.join("data").join("config.json");
    }
    let exe = std::env::current_exe().unwrap_or_default();
    exe.with_file_name("config.json")
}
/// Default start folders while in development: `app\data\sandbox` next to the crate
/// (the exe lives in `app\target\<profile>`), never a system or user folder.
/// Created with a few sample files on first use.
fn sandbox() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let app = app_dir().ok_or("app フォルダーが見つかりません。開くフォルダーを引数で指定してください")?;
    let root = app.join("data").join("sandbox");
    if !root.join("left").is_dir() {
        std::fs::create_dir_all(root.join("left").join("フォルダー A"))?;
        std::fs::create_dir_all(root.join("right"))?;
        for (name, text) in [
            ("メモ.txt", "サンプル"),
            ("file 1.txt", "one"),
            ("file 2.txt", "two"),
            ("file 10.txt", "ten"),
        ] {
            std::fs::write(root.join("left").join(name), text)?;
        }
        std::fs::write(root.join("right").join("file 1.txt"), "right side")?;
    }
    Ok(root)
}
/// Start folder when none is configured: the sandbox while developing,
/// otherwise the Desktop (left, 0) and Documents (right, 1).
fn default_start(side: usize) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if app_dir().is_some() {
        return Ok(sandbox()?.join(["left", "right"][side]));
    }
    config::known_folder(side).ok_or_else(|| "既定のフォルダーが見つかりません。開くフォルダーを引数で指定してください".into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let smoke = args.iter().position(|a| a.as_os_str() == "--smoke");
    // `--demo <root> <frames>`: records the README animation (demo.rs) in a fresh `<root>`.
    let demo = args.iter().position(|a| a.as_os_str() == "--demo");
    if let Some(i) = demo {
        let root = args.get(i + 1).ok_or("--demo <root> <frames>")?;
        demo::prepare(root)?;
    }
    let own_root = smoke.or(demo).map(|i| args[i + 1].as_path());
    let config = config::Config::load(config_file(own_root));
    let (left, right) = match smoke {
        Some(i) => {
            let root = args.get(i + 1).ok_or("--smoke <root> <log>")?;
            (root.join("left"), root.join("right"))
        }
        None if demo.is_some() => demo::start_folders(own_root.unwrap()),
        // No arguments: the configured start folders, else the development sandbox.
        None if args.is_empty() => {
            let [start_left, start_right] = config.start.clone();
            let left = match start_left { Some(p) => p, None => default_start(0)? };
            let right = match start_right { Some(p) => p, None => default_start(1)? };
            (left, right)
        }
        None => (args[0].clone(), args.get(1).unwrap_or(&args[0]).clone()),
    };
    let _ole = shell::Apartment::new()?;
    let ui = App::new()?;
    let panes = std::array::from_fn(|p| Pane {
        model: Rc::new(RowsModel {
            compare: RefCell::new(None),
            data: RefCell::new(Arc::new(Snapshot::default())),
            selection: RefCell::new(Selection::default()),
            special: Default::default(),
            page_start: Cell::new(0),
            page_size: Cell::new(30),
            ext_colors: Default::default(),
            drives: RefCell::new(Arc::new(Vec::new())),
            notify: ModelNotify::default(),
        }),
        worker: Worker::new(p),
        generation: 0,
        pending: None,
        stale: false,
        watch: None,
        back: Vec::new(),
        forward: Vec::new(),
        column: 0,
        descending: false,
        prefix: String::new(),
        typed: Instant::now(),
        filter: None,
        finder: None,
        found: None,
    });
    ui.set_p0_rows(ModelRc::from(panes[0].model.clone()));
    ui.set_p1_rows(ModelRc::from(panes[1].model.clone()));
    ui.set_p2_rows(ModelRc::from(panes[2].model.clone()));
    ui.set_p3_rows(ModelRc::from(panes[3].model.clone()));
    let controller = Rc::new(RefCell::new(Controller {
        ui: ui.as_weak(),
        panes,
        active: 0,
        operations: 0,
        asking: VecDeque::new(),
        config,
        settings: Default::default(),
        settings_colors: Vec::new(),
        settings_ext: Vec::new(),
        settings_font: (String::new(), 0.0),
        drives: Arc::new(Vec::new()),
        menus: ctxmenu::Menus::start(|outcome| {
            post(Reply::Menu(outcome));
        }),
        menu_pane: 0,
        naming: None,
        finding: None,
        search: String::new(),
        tree_search: String::new(),
        searches: 0,
        typing: slint::Timer::default(),
        index_limit: finder::INDEX_LIMIT,
        compare: None,
        previous: 1,
        demo: demo.is_some(),
    }));
    CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));
    {
        let c = controller.clone();
        ui.on_navigate(move |p, path| c.borrow_mut().typed_path(p as usize, path.to_string()));
    }
    {
        let c = controller.clone();
        ui.on_command(move |p, command| c.borrow_mut().command(p as usize, command));
    }
    {
        let c = controller.clone();
        ui.on_sort(move |p, column| c.borrow_mut().sort(p as usize, column as usize));
    }
    {
        let c = controller.clone();
        ui.on_crumb(move |p, i| c.borrow_mut().crumb(p as usize, i as usize));
    }
    {
        let c = controller.clone();
        ui.on_place(move |i| c.borrow_mut().place(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_bookmark_open(move |i| c.borrow_mut().open_bookmark(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_bookmark_add(move || c.borrow_mut().add_bookmark());
    }
    {
        let c = controller.clone();
        ui.on_bookmark_edit(move |i| c.borrow_mut().edit_bookmark(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_bookmark_save(move |i, name, path| c.borrow_mut().save_bookmark(i as usize, &name, &path));
    }
    {
        let c = controller.clone();
        ui.on_bookmark_delete(move |i| c.borrow_mut().delete_bookmark(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_bookmark_move(move |i, down| c.borrow_mut().move_bookmark(i as usize, down));
    }
    {
        let c = controller.clone();
        ui.on_bookmark_cancel(move || c.borrow().close_bookmark_dialog());
    }
    {
        let c = controller.clone();
        ui.on_open_settings(move || c.borrow_mut().open_settings());
    }
    {
        let c = controller.clone();
        ui.on_setting_edited(move |i, text| c.borrow_mut().setting_edited(i as usize, &text));
    }
    {
        let c = controller.clone();
        ui.on_setting_reset(move |i| c.borrow_mut().setting_reset(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_setting_browse(move |i| c.borrow_mut().setting_browse(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_settings_done(move |save| c.borrow_mut().settings_done(save));
    }
    {
        let c = controller.clone();
        ui.on_layout_committed(move || c.borrow_mut().layout_committed());
    }
    controller.borrow().sync_bookmarks();
    {
        let config = &controller.borrow().config;
        ui.set_split(config.split);
        ui.set_col_a(config.split3_columns[0]);
        ui.set_col_b(config.split3_columns[1]);
        ui.set_row_a(config.split3_rows[0]);
        ui.set_row_b(config.split3_rows[1]);
        ui.set_quad_x(config.split4[0]);
        ui.set_quad_y(config.split4[1]);
    }
    {
        let c = controller.clone();
        ui.on_color_base(move |i, color| c.borrow_mut().color_base(i as usize, color));
    }
    {
        let c = controller.clone();
        ui.on_color_ext(move |i, exts, color| c.borrow_mut().color_ext(i as usize, exts.as_str(), color));
    }
    {
        let c = controller.clone();
        ui.on_color_ext_delete(move |i| c.borrow_mut().color_ext_delete(i as usize));
    }
    {
        let c = controller.clone();
        ui.on_colors_reset(move || c.borrow_mut().colors_reset());
    }
    {
        let c = controller.clone();
        ui.on_font_set(move |family, size| c.borrow_mut().font_set(family.as_str(), size as f32));
    }
    controller.borrow().apply_appearance();
    {
        let c = controller.clone();
        ui.on_name_validate(move |name| c.borrow().name_error(&name).into());
    }
    {
        let c = controller.clone();
        ui.on_name_done(move |ok, name| c.borrow_mut().name_done(ok, name.to_string()));
    }
    {
        let c = controller.clone();
        ui.on_menu_command(move |command| {
            let mut c = c.borrow_mut();
            let p = c.active;
            match command {
                0 => c.open_name(p, NameKind::NewFile),
                1 => c.open_name(p, NameKind::NewFolder),
                2 => c.toggle_hidden(p),
                3 => c.copy(p, false),
                4 => c.paste(p),
                5 => c.delete(p, false),
                6 => c.copy_text(p, false),
                7 => c.copy_text(p, true),
                8 => c.select_same_extension(p),
                9 => c.toggle_attributes(),
                20..=24 => c.toggle_column(command as usize - 20),
                10 => c.set_layout(2, false),
                11 => c.set_layout(3, false),
                12 => c.set_layout(4, false),
                13 => c.set_layout(3, true),
                30 => c.open_find(p, 1),
                31 => c.find_next(p, true),
                32 => c.open_find(p, 2),
                33 => c.set_filter(p, ""),
                34 => c.open_find(p, 3),
                35 => c.toggle_search_links(),
                40 => c.toggle_compare(),
                _ => {}
            }
        });
    }
    {
        let c = controller.clone();
        ui.on_find_edited(move |text| c.borrow_mut().find_edited(&text));
    }
    {
        let c = controller.clone();
        ui.on_find_done(move |ok, text| c.borrow_mut().find_done(ok, &text));
    }
    {
        let c = controller.clone();
        ui.on_find_next(move |forward| c.borrow_mut().find_next_typing(forward));
    }
    {
        let c = controller.clone();
        ui.on_page_resized(move |p, size| c.borrow_mut().page_resized(p as usize, size.max(1) as usize));
    }
    {
        let c = controller.clone();
        ui.on_page_step(move |p, forward| c.borrow_mut().page_step(p as usize, forward));
    }
    {
        let c = controller.clone();
        ui.on_copy_path(move |p| c.borrow_mut().copy_path(p as usize));
    }
    {
        let c = controller.clone();
        ui.on_blank_menu(move |p, extended| c.borrow_mut().blank_menu(p as usize, extended));
    }
    // Drive list: refreshed in the background (labels and free space), sent only on change.
    std::thread::spawn(|| {
        let _com = shell::Apartment::new();
        let mut last = Vec::new();
        loop {
            let drives = listing::drives();
            if drives != last {
                last = drives.clone();
                if !post(Reply::Drives(Arc::new(drives))) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
    {
        let c = controller.clone();
        ui.on_pointer(move |p, index, ctrl, shift, kind| {
            c.borrow_mut()
                .pointer(p as usize, index as usize, ctrl, shift, kind)
        });
    }
    {
        let c = controller.clone();
        ui.on_key_event(move |p, event| c.borrow_mut().key(p as usize, event));
    }
    {
        let c = controller.clone();
        ui.on_blank(move |p| {
            let mut c = c.borrow_mut();
            c.active(p as usize);
            c.panes[p as usize].model.selection.borrow_mut().clear();
            c.repaint(p as usize);
        });
    }
    {
        let c = controller.clone();
        ui.on_ask_answer(move |code, name, all| c.borrow_mut().answer(code, name.to_string(), all));
    }
    {
        let c = controller.clone();
        ui.on_ask_validate(move |name| c.borrow().validate(&name).into());
    }
    {
        let c = controller.clone();
        ui.window().on_close_requested(move || {
            if let Ok(mut c) = c.try_borrow_mut() {
                c.remember_window();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    // Set before show, so the window opens in place instead of jumping there.
    if demo.is_some() {
        ui.window().set_size(demo::SIZE);
    } else if let Some(w) = controller.borrow().config.window.filter(|w| shell::on_screen(w.x, w.y, w.width)) {
        ui.window().set_position(slint::PhysicalPosition::new(w.x, w.y));
        ui.window().set_size(slint::PhysicalSize::new(w.width, w.height));
        ui.window().set_maximized(w.maximized);
    }
    ui.show()?;
    // winit creates the native window once the event loop runs; poll briefly for its HWND.
    let revoke = Rc::new(slint::Timer::default());
    {
        let weak = ui.as_weak();
        let stop = Rc::downgrade(&revoke);
        let start = Instant::now();
        revoke.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            let handle = weak.upgrade().and_then(|ui| hwnd(&ui));
            if let Some(h) = handle {
                shell::revoke_drop_target(h);
                shell::dark_title_bar(h);
                shell::window_icon(h);
                if let Some(c) = CONTROLLER.with(|c| c.borrow().clone()) {
                    c.borrow().apply_opacity();
                }
            }
            if handle.is_some() || start.elapsed() > Duration::from_secs(5) {
                if let Some(timer) = stop.upgrade() {
                    timer.stop();
                }
            }
        });
    }
    {
        let mut c = controller.borrow_mut();
        // Panes 3 and 4 (3/4-pane layouts) start where the left and right ones do.
        c.navigate(0, left.clone(), Mode::New(None));
        c.navigate(1, right.clone(), Mode::New(None));
        c.navigate(2, left, Mode::New(None));
        c.navigate(3, right, Mode::New(None));
        let ui = c.ui.upgrade().unwrap();
        ui.set_pane_count(c.config.panes as i32);
        ui.set_three_columns(c.config.three_columns);
        ui.set_active_hidden(c.config.show_hidden[0]);
        ui.set_search_links(c.config.search_links);
        c.sync_columns();
    }
    ui.invoke_focus_pane(0);
    let _smoke = smoke.map(|i| smoke::start(controller.clone(), args[i + 1].clone(), args.get(i + 2).cloned()));
    let _demo = demo.map(|i| demo::start(controller.clone(), args[i + 1].clone(), args.get(i + 2).cloned()));
    slint::run_event_loop()?;
    CONTROLLER.with(|c| c.borrow_mut().take());
    Ok(())
}
