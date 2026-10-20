use chrono::{DateTime, Local, NaiveDate};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};
use windows::{
    Win32::{Storage::FileSystem::*, UI::Shell::*},
    core::PCWSTR,
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: Arc<PathBuf>,
    pub name: String,
    pub name_fold: String,
    /// Display split: "name" + "ext" (folders and dot-files have no extension).
    pub stem: String,
    pub ext: String,
    pub size: u64,
    pub modified: SystemTime,
    pub attributes: u32,
    /// Windows' file type name, e.g. "JSON ソース ファイル".
    pub type_name: Arc<str>,
    pub directory: bool,
    pub wide_name: Vec<u16>,
    /// Icon of the kind (extension / folder), always available after `read`.
    pub type_icon: Arc<str>,
    /// Icon of this very file (exe, lnk, ...), filled in later; equals `type_icon` otherwise.
    pub icon: Arc<str>,
}
impl Entry {
    pub fn hidden(&self) -> bool {
        self.attributes & 0x2 != 0
    }
}
/// Call on a thread with COM initialized (type names and icons come from the Shell).
pub fn read(path: &Path, show_hidden: bool) -> Result<(PathBuf, Vec<Entry>), String> {
    let canonical = fs::canonicalize(path).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for entry in fs::read_dir(&canonical).map_err(|e| e.to_string())? {
        // An entry can vanish between enumeration and stat; skip it rather than fail the folder.
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if !show_hidden && meta.file_attributes() & 0x2 != 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let wide_name = entry.file_name().encode_wide().chain(Some(0)).collect();
        let directory = meta.is_dir();
        let (stem, ext) = match name.rfind('.') {
            Some(dot) if dot > 0 && !directory => (name[..dot].to_string(), name[dot + 1..].to_string()),
            _ => (name.clone(), String::new()),
        };
        let type_icon = crate::icons::type_key(&ext, directory);
        crate::icons::ensure_type(&type_icon, directory);
        let path = Arc::new(entry.path());
        let icon = if !directory && crate::icons::per_file(&ext) {
            path.to_string_lossy().as_ref().into()
        } else {
            type_icon.clone()
        };
        rows.push(Entry {
            path,
            icon,
            type_icon,
            name_fold: name.to_lowercase(),
            type_name: type_name(&ext, directory),
            stem,
            ext,
            name,
            size: meta.len(),
            modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            attributes: meta.file_attributes(),
            directory,
            wide_name,
        });
    }
    Ok((canonical, rows))
}

/// Type names by extension, looked up once (by extension only, no disk access).
fn type_name(ext: &str, directory: bool) -> Arc<str> {
    static CACHE: OnceLock<Mutex<HashMap<(String, bool), Arc<str>>>> = OnceLock::new();
    let key = (ext.to_lowercase(), directory);
    let cache = CACHE.get_or_init(Default::default);
    if let Some(name) = cache.lock().unwrap().get(&key) {
        return name.clone();
    }
    let probe: Vec<u16> = format!("x.{}", key.0).encode_utf16().chain(Some(0)).collect();
    let mut info = SHFILEINFOW::default();
    let attributes = if directory { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(probe.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        )
    } != 0;
    let len = info.szTypeName.iter().position(|&c| c == 0).unwrap_or(0);
    let name: Arc<str> = if ok && len > 0 {
        String::from_utf16_lossy(&info.szTypeName[..len]).into()
    } else if directory {
        "ファイル フォルダー".into()
    } else {
        "ファイル".into()
    };
    cache.lock().unwrap().insert(key, name.clone());
    name
}

/// 3 significant digits like "20.4 MB", "566 KB", "1.36 KB"; plain bytes below 1000.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1000 {
        return bytes.to_string();
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < 4 {
        value /= 1024.0;
        unit += 1;
    }
    let unit = ["", "KB", "MB", "GB", "TB"][unit];
    if value < 10.0 {
        format!("{value:.2} {unit}")
    } else if value < 100.0 {
        format!("{value:.1} {unit}")
    } else {
        format!("{value:.0} {unit}")
    }
}
/// ("今日" / "N 日以前" within a week / "YYYY-MM-DD", "HH:MM").
pub fn format_date(modified: SystemTime, today: NaiveDate) -> (String, String) {
    let local = DateTime::<Local>::from(modified);
    let days = (today - local.date_naive()).num_days();
    let date = match days {
        0 => "今日".to_string(),
        1..=6 => format!("{days} 日以前"),
        _ => local.format("%Y-%m-%d").to_string(),
    };
    (date, local.format("%H:%M").to_string())
}
/// Archive, Read-only, Hidden, System; "_" when not set.
pub fn format_attributes(attributes: u32) -> String {
    [(0x20, 'A'), (0x1, 'R'), (0x2, 'H'), (0x4, 'S')]
        .iter()
        .map(|&(bit, c)| if attributes & bit != 0 { c } else { '_' })
        .collect()
}

pub struct Volume {
    /// e.g. "D_SSD (D:)"
    pub label: String,
    pub free: u64,
}
/// Label and free space of the volume holding `dir`. Can be slow on network drives: worker only.
pub fn volume(dir: &Path) -> Option<Volume> {
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = [0u16; 512];
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut root) }.ok()?;
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(root.as_ptr()), Some(&mut free), None, None) }.ok()?;
    let mut name = [0u16; 261];
    let _ = unsafe {
        GetVolumeInformationW(PCWSTR(root.as_ptr()), Some(&mut name), None, None, None, None)
    };
    let text = |w: &[u16]| String::from_utf16_lossy(&w[..w.iter().position(|&c| c == 0).unwrap_or(w.len())]);
    let root = text(&root);
    let root = root.trim_start_matches(r"\\?\").trim_end_matches('\\');
    let name = text(&name);
    Some(Volume {
        label: if name.is_empty() { root.to_string() } else { format!("{name} ({root})") },
        free,
    })
}
#[derive(Clone, Debug, PartialEq)]
pub struct Drive {
    pub root: PathBuf,
    /// e.g. "C: C_SSD", "E: DVD RW ドライブ"
    pub name: String,
    pub free: u64,
    pub total: u64,
    pub icon: Arc<str>,
}
/// All drive letters with label and space. Slow for network or empty optical drives:
/// call on the drive thread only.
pub fn drives() -> Vec<Drive> {
    let mut buffer = [0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buffer)) } as usize;
    buffer[..len.min(buffer.len())]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|root| {
            let root_text = String::from_utf16_lossy(root);
            let wide: Vec<u16> = root.iter().copied().chain(Some(0)).collect();
            let letter = root_text.trim_end_matches('\\').to_string();
            // The Shell's display name, e.g. "C_SSD (C:)" or "DVD RW ドライブ (E:)".
            let mut info = SHFILEINFOW::default();
            unsafe {
                SHGetFileInfoW(
                    PCWSTR(wide.as_ptr()),
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    Some(&mut info),
                    std::mem::size_of::<SHFILEINFOW>() as u32,
                    SHGFI_DISPLAYNAME,
                )
            };
            let n = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(0);
            let display = String::from_utf16_lossy(&info.szDisplayName[..n]);
            let label = display.replace(&format!("({letter})"), "").trim().to_string();
            let (mut free, mut total) = (0u64, 0u64);
            let _ = unsafe {
                GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), Some(&mut total), None)
            };
            let icon: Arc<str> = format!("<drive {letter}>").into();
            crate::icons::ensure_path(&icon, Path::new(&root_text));
            Drive {
                root: PathBuf::from(&root_text),
                name: if label.is_empty() { letter.clone() } else { format!("{letter} {label}") },
                free,
                total,
                icon,
            }
        })
        .collect()
}
pub fn sort(rows: &mut [Entry], column: usize, descending: bool) {
    use windows::{Win32::UI::Shell::StrCmpLogicalW, core::PCWSTR};
    rows.sort_by(|a, b| {
        let folders = b.directory.cmp(&a.directory);
        if !folders.is_eq() {
            return folders;
        }
        let name = || {
            unsafe { StrCmpLogicalW(PCWSTR(a.wide_name.as_ptr()), PCWSTR(b.wide_name.as_ptr())) }
                .cmp(&0)
                .then(a.path.cmp(&b.path))
        };
        let order = match column {
            1 => a.size.cmp(&b.size).then_with(name),
            2 => a.modified.cmp(&b.modified).then_with(name),
            3 => a.ext.to_lowercase().cmp(&b.ext.to_lowercase()).then_with(name),
            _ => name(),
        };
        if descending { order.reverse() } else { order }
    });
}

/// `all` + exceptions makes Ctrl+A independent of the number of directory entries.
#[derive(Clone, Default, Debug)]
pub struct Selection {
    pub all: bool,
    pub universe: Option<Arc<HashMap<Arc<PathBuf>, usize>>>,
    pub selected_count: usize,
    pub exceptions: HashSet<Arc<PathBuf>>,
    pub current: Option<usize>,
    pub anchor: Option<usize>,
    pub deferred: Option<usize>,
}
impl Selection {
    fn base(&self, path: &Arc<PathBuf>) -> bool {
        self.all && self.universe.as_ref().is_some_and(|u| u.contains_key(path))
    }
    pub fn contains(&self, path: &Arc<PathBuf>) -> bool {
        self.base(path) != self.exceptions.contains(path)
    }
    fn set(&mut self, path: Arc<PathBuf>, selected: bool) {
        let old = self.contains(&path);
        if old != selected {
            if selected {
                self.selected_count += 1;
            } else {
                self.selected_count = self.selected_count.saturating_sub(1);
            }
        }
        if selected != self.base(&path) {
            self.exceptions.insert(path);
        } else {
            self.exceptions.remove(&path);
        }
    }
    pub fn count(&self) -> usize {
        self.selected_count
    }
    pub fn clear(&mut self) {
        self.all = false;
        self.universe = None;
        self.selected_count = 0;
        self.exceptions.clear();
        self.deferred = None;
    }
    pub fn select_all(&mut self, universe: Arc<HashMap<Arc<PathBuf>, usize>>) {
        self.clear();
        self.all = true;
        self.selected_count = universe.len();
        self.universe = Some(universe);
    }
    /// Selects exactly `paths` (used after paste, like Explorer selecting the pasted items).
    pub fn only(paths: impl IntoIterator<Item = Arc<PathBuf>>) -> Self {
        let exceptions: HashSet<_> = paths.into_iter().collect();
        Self {
            selected_count: exceptions.len(),
            exceptions,
            ..Self::default()
        }
    }
    pub fn select(&mut self, rows: &[Entry], index: usize, ctrl: bool, shift: bool, mouse: bool) {
        if index >= rows.len() {
            return;
        }
        self.deferred = None;
        if mouse && !ctrl && !shift && self.contains(&rows[index].path) {
            self.deferred = Some(index);
            self.current = Some(index);
            return;
        }
        if shift {
            let anchor = self.anchor.unwrap_or(self.current.unwrap_or(index));
            if !ctrl {
                self.clear();
            }
            for row in &rows[anchor.min(index)..=anchor.max(index)] {
                self.set(row.path.clone(), true);
            }
            self.anchor = Some(anchor);
        } else if ctrl {
            if mouse {
                self.set(rows[index].path.clone(), !self.contains(&rows[index].path));
                self.anchor = Some(index);
            }
        } else {
            self.clear();
            self.set(rows[index].path.clone(), true);
            self.anchor = Some(index);
        }
        self.current = Some(index);
    }
    pub fn release(&mut self, rows: &[Entry]) {
        if let Some(index) = self.deferred.take() {
            self.select(rows, index, false, false, false);
        }
    }
    pub fn toggle_current(&mut self, rows: &[Entry]) {
        if let Some(i) = self.current {
            self.set(rows[i].path.clone(), !self.contains(&rows[i].path));
            self.anchor = Some(i);
        }
    }
    #[cfg(test)]
    pub fn remap(&mut self, old: &[Entry], new: &[Entry]) {
        let current = self.current.and_then(|i| old.get(i)).map(|e| &e.path);
        let anchor = self.anchor.and_then(|i| old.get(i)).map(|e| &e.path);
        self.current = current
            .and_then(|p| new.iter().position(|e| &e.path == p))
            .or_else(|| (!new.is_empty()).then_some(0));
        self.anchor = anchor.and_then(|p| new.iter().position(|e| &e.path == p));
        let present: HashSet<_> = new.iter().map(|e| e.path.clone()).collect();
        self.exceptions.retain(|p| present.contains(p));
        self.deferred = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rows() -> Vec<Entry> {
        (0..8)
            .map(|i| Entry {
                path: Arc::new(PathBuf::from(format!("C:/demo/{i}"))),
                name: i.to_string(),
                name_fold: i.to_string(),
                stem: i.to_string(),
                ext: String::new(),
                size: i,
                modified: std::time::UNIX_EPOCH,
                attributes: 0,
                type_name: "".into(),
                type_icon: "".into(),
                icon: "".into(),
                directory: false,
                wide_name: vec![48 + i as u16, 0],
            })
            .collect()
    }
    #[test]
    fn explorer_selection_and_sort_identity() {
        let mut rows = rows();
        let mut s = Selection::default();
        s.select(&rows, 1, false, false, true);
        s.release(&rows);
        s.select(&rows, 3, true, false, true);
        s.release(&rows);
        assert_eq!(s.count(), 2);
        s.select(&rows, 1, false, false, true);
        assert_eq!(s.count(), 2); // pressing a selected row keeps the group until release
        s.release(&rows);
        assert_eq!(s.count(), 1);
        s.select(&rows, 3, true, false, true);
        s.select(&rows, 5, false, true, false);
        assert_eq!(s.count(), 3); // anchor was Ctrl+click at 3
        s.select(&rows, 7, true, false, false);
        assert_eq!(s.count(), 3); // Ctrl+arrow only moves focus
        s.toggle_current(&rows);
        assert_eq!(s.count(), 4);
        let old = rows.clone();
        sort(&mut rows, 0, true);
        s.remap(&old, &rows);
        assert_eq!(rows[s.current.unwrap()].name, "7");
        assert_eq!(s.count(), 4);
        s.select_all(Arc::new(
            rows.iter()
                .enumerate()
                .map(|(i, e)| (e.path.clone(), i))
                .collect(),
        ));
        s.select(&rows, 0, true, false, true);
        assert_eq!(s.count(), 7);
    }
    #[test]
    fn display_formats() {
        assert_eq!(format_size(757), "757");
        assert_eq!(format_size(1393), "1.36 KB");
        assert_eq!(format_size(566 * 1024), "566 KB");
        assert_eq!(format_size(21_390_000), "20.4 MB");
        assert_eq!(format_attributes(0x20), "A___");
        assert_eq!(format_attributes(0x10), "____");
        let today = NaiveDate::from_ymd_opt(2026, 10, 19).unwrap();
        let at = |d: u32| {
            let t = NaiveDate::from_ymd_opt(2026, 10, d).unwrap().and_hms_opt(9, 5, 0).unwrap();
            SystemTime::from(t.and_local_timezone(Local).unwrap())
        };
        assert_eq!(format_date(at(19), today), ("今日".into(), "09:05".into()));
        assert_eq!(format_date(at(15), today).0, "4 日以前");
        assert_eq!(format_date(at(1), today).0, "2026-10-01");
    }
    #[test]
    fn select_all_does_not_select_later_files() {
        let mut rows = rows();
        let mut selection = Selection::default();
        selection.select_all(Arc::new(
            rows.iter()
                .enumerate()
                .map(|(i, e)| (e.path.clone(), i))
                .collect(),
        ));
        let mut added = rows[0].clone();
        added.path = Arc::new(PathBuf::from("C:/demo/new-file"));
        rows.push(added);
        assert!(!selection.contains(&rows[8].path));
        assert_eq!(selection.count(), 8);
        selection.select(&rows, 8, true, false, true);
        assert!(selection.contains(&rows[8].path));
        assert_eq!(selection.count(), 9);
    }
}
