//! The pane list as a Slint model: "..", the entries, then the drives, shown one page at
//! a time; comparing, the rows shared with the other pane instead.
use crate::{Row, compare, icons, listing::{self, Drive, Entry, Selection}};
use slint::{Model, ModelNotify, ModelTracker};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
};

pub type Positions = Arc<HashMap<Arc<PathBuf>, usize>>;
#[derive(Default, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub rows: Vec<Entry>,
    pub positions: Positions,
    pub volume: Option<Arc<listing::Volume>>,
    /// All entries while a filter hides some, so changing the filter needs no disk read.
    pub unfiltered: Option<Arc<Vec<Entry>>>,
}
impl Snapshot {
    pub fn new(dir: PathBuf, rows: Vec<Entry>, volume: Option<Arc<listing::Volume>>) -> Self {
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
    pub fn offset(&self) -> usize {
        self.dir.parent().is_some() as usize
    }
}
/// Where the cursor rests when it is not on an entry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Special {
    Parent,
    Drive(usize),
    /// Comparing: a row where only the other pane has an entry.
    Gap(usize),
}
/// What a model row shows.
pub enum Slot {
    Parent,
    Entry(usize),
    Drive(usize),
    Gap,
}
/// Model rows: ".." (when the folder has a parent), the entries, then the drives.
/// Selection works on entry indexes; the cursor can also rest on ".." or a drive.
/// Comparing, the rows are those of `compare` instead (no "..", no drives).
pub struct RowsModel {
    /// Ctrl+Shift+D: the rows shared with the other pane, and this pane's side (0 old, 1 new).
    pub compare: RefCell<Option<(Rc<compare::Aligned>, usize)>>,
    pub data: RefCell<Arc<Snapshot>>,
    pub selection: RefCell<Selection>,
    pub drives: RefCell<Arc<Vec<Drive>>>,
    /// The cursor is on ".." or a drive rather than on `selection.current`.
    pub special: Cell<Option<Special>>,
    /// The list is shown one page at a time: model rows are page-relative.
    pub page_start: Cell<usize>,
    pub page_size: Cell<usize>,
    /// Lower-case extension -> name color (Settings > カラー).
    pub ext_colors: RefCell<Arc<HashMap<String, slint::Color>>>,
    pub notify: ModelNotify,
}
impl RowsModel {
    pub fn slot(&self, row: usize) -> Option<Slot> {
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
    /// The first entry row met going from `from` toward `to`, both included.
    pub fn entry_toward(&self, from: usize, to: usize) -> Option<usize> {
        let rows: Box<dyn Iterator<Item = usize>> = if from <= to { Box::new(from..=to) } else { Box::new((to..=from).rev()) };
        rows.into_iter().find(|&row| matches!(self.slot(row), Some(Slot::Entry(_))))
    }
    pub fn special_row(&self, special: Special) -> usize {
        let data = self.data.borrow();
        match special {
            Special::Parent => 0,
            Special::Drive(i) => data.offset() + data.rows.len() + i,
            Special::Gap(row) => row,
        }
    }
    /// Global row of entry `index`.
    pub fn entry_row(&self, index: usize) -> usize {
        match &*self.compare.borrow() {
            Some((aligned, side)) => aligned.row_of(*side, index).unwrap_or(0),
            None => index + self.data.borrow().offset(),
        }
    }
    pub fn on_parent(&self) -> bool {
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
    pub fn total(&self) -> usize {
        if let Some((aligned, _)) = &*self.compare.borrow() {
            return aligned.pairs.len();
        }
        let data = self.data.borrow();
        data.rows.len() + data.offset() + self.drives.borrow().len()
    }
    /// Shows the page holding global row `row`; true if the page changed.
    pub fn show_page_of(&self, row: usize) -> bool {
        let size = self.page_size.get().max(1);
        let start = row.min(self.total().saturating_sub(1)) / size * size;
        let changed = start != self.page_start.get();
        self.page_start.set(start);
        changed
    }
    pub fn pages(&self) -> (usize, usize) {
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
    pub fn row_at(&self, row: usize) -> Option<Row> {
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
                    name: listing::visible(&drive.name).as_ref().into(),
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
        let [location, name, ext] = [&entry.location, &entry.stem, &entry.ext].map(|s| listing::visible(s));
        let spoof = [&location, &name, &ext].iter().any(|s| matches!(s, Cow::Owned(_)));
        Some(Row {
            icon: icon_image(&entry.icon, &entry.type_icon),
            hidden: entry.hidden(),
            readonly: entry.attributes & 0x1 != 0,
            tinted: tint.is_some(),
            tint: tint.unwrap_or_default(),
            location: location.as_ref().into(),
            name: name.as_ref().into(),
            ext: ext.as_ref().into(),
            spoof,
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
