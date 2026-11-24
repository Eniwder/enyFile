//! Ctrl+Shift+D: two folders side by side, with a metadata check of matched folders.
//! The old side is the left / upper pane.
use crate::listing::{self, Entry};
use std::{
    cmp::Ordering,
    collections::HashMap,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::Duration,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const FILE_DIFF_ATTRIBUTES: u32 = 0x27;
#[derive(Clone, Copy)]
pub enum FolderState {
    Pending,
    Changed,
}

pub type FolderStates = HashMap<Arc<PathBuf>, FolderState>;

/// Only matched, ordinary folders get a child listing.
pub fn folder_pairs(old: &[Entry], new: &[Entry]) -> Vec<[Arc<PathBuf>; 2]> {
    let mut pairs = Vec::new();
    let mut new = new.iter().peekable();
    for a in old.iter().take_while(|entry| entry.directory) {
        while new
            .peek()
            .is_some_and(|b| listing::name_order(a, b) == Ordering::Greater)
        {
            new.next();
        }
        let Some(b) = new.next_if(|b| a.directory == b.directory && a.name_fold == b.name_fold)
        else {
            continue;
        };
        if a.path != b.path && (a.attributes | b.attributes) & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
            pairs.push([a.path.clone(), b.path.clone()]);
        }
    }
    pairs
}

pub fn children_differ(old: &Path, new: &Path, show_hidden: bool, stop: &AtomicBool) -> bool {
    let children = |dir: &Path| {
        // Check again at the point of use, in case a folder was replaced after listing.
        let attributes = std::fs::symlink_metadata(dir).ok()?.file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return None;
        }
        let mut rows: Vec<_> = listing::scan(dir, show_hidden)
            .ok()?
            .into_iter()
            .map(|row| (row.name.to_string_lossy().to_lowercase(), row))
            .collect();
        rows.sort_unstable_by(|(a, ar), (b, br)| a.cmp(b).then_with(|| ar.name.cmp(&br.name)));
        Some(rows)
    };
    let mut pending = vec![(old.to_path_buf(), new.to_path_buf())];
    while let Some((old, new)) = pending.pop() {
        if stop.load(AtomicOrdering::Relaxed) {
            return false;
        }
        let (Some(a), Some(b)) = (children(&old), children(&new)) else {
            continue;
        };
        if a.len() != b.len() {
            return true;
        }
        for ((an, a), (bn, b)) in a.into_iter().zip(b) {
            let attr_diff = a.attributes ^ b.attributes;
            if an != bn
                || a.directory != b.directory
                || attr_diff & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                || (!a.directory
                    && (a.size != b.size
                        || time_differs(a.modified, b.modified)
                        || attr_diff & FILE_DIFF_ATTRIBUTES != 0))
            {
                return true;
            }
            if a.directory && a.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
                pending.push((old.join(a.name), new.join(b.name)));
            }
        }
    }
    false
}

/// How one row differs. `kind`: 0 same, 1 only on the new side ("+"), 2 only on the old
/// side ("-"), 3 on both sides but different (the differing cells are flagged).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Diff {
    pub kind: i32,
    pub pending: bool,
    pub size: bool,
    pub time: bool,
    pub attr: bool,
}
/// The rows both panes show: for each row the entry of each side, if any.
#[derive(Debug, Default)]
pub struct Aligned {
    pub pairs: Vec<[Option<usize>; 2]>,
    pub diffs: Vec<Diff>,
    /// Entry index -> row, per side.
    pub rows: [Vec<usize>; 2],
    /// Rows only on the old side, only on the new side, differing, the same.
    pub counts: [usize; 4],
    pub pending: usize,
}
impl Aligned {
    pub fn row_of(&self, side: usize, entry: usize) -> Option<usize> {
        self.rows[side].get(entry).copied()
    }
}

/// Matches `old` and `new` by name (both in `listing::name_order`). Returns the rows and
/// the entries each side shows, in row order; with `only_diff` equal rows are left out.
pub fn align(
    old: &[Entry],
    new: &[Entry],
    only_diff: bool,
    folders: &FolderStates,
) -> (Aligned, [Vec<Entry>; 2]) {
    let mut aligned = Aligned::default();
    let mut shown: [Vec<Entry>; 2] = [Vec::new(), Vec::new()];
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let (a, b) = (old.get(i), new.get(j));
        let (pair, diff) = match (a, b) {
            (Some(a), Some(b)) if a.directory == b.directory && a.name_fold == b.name_fold => {
                i += 1;
                j += 1;
                let mut diff = differ(a, b);
                match folders.get(&a.path) {
                    Some(FolderState::Pending) => diff.pending = true,
                    Some(FolderState::Changed) => diff.kind = 3,
                    None => {}
                }
                ([Some(a), Some(b)], diff)
            }
            (Some(a), Some(b)) if listing::name_order(a, b) == Ordering::Greater => {
                j += 1;
                (
                    [None, Some(b)],
                    Diff {
                        kind: 1,
                        ..Diff::default()
                    },
                )
            }
            (Some(a), _) => {
                i += 1;
                (
                    [Some(a), None],
                    Diff {
                        kind: 2,
                        ..Diff::default()
                    },
                )
            }
            (None, Some(b)) => {
                j += 1;
                (
                    [None, Some(b)],
                    Diff {
                        kind: 1,
                        ..Diff::default()
                    },
                )
            }
            (None, None) => break,
        };
        let count = match diff.kind {
            2 => 0,
            1 => 1,
            3 => 2,
            _ => 3,
        };
        if diff.pending {
            aligned.pending += 1;
        } else {
            aligned.counts[count] += 1;
        }
        if only_diff && diff.kind == 0 && !diff.pending {
            continue;
        }
        let row = aligned.pairs.len();
        let mut indexes = [None, None];
        for side in 0..2 {
            if let Some(entry) = pair[side] {
                indexes[side] = Some(shown[side].len());
                aligned.rows[side].push(row);
                shown[side].push(entry.clone());
            }
        }
        aligned.pairs.push(indexes);
        aligned.diffs.push(diff);
    }
    (aligned, shown)
}

/// File size, modified time (FAT keeps even seconds), and A R H S attributes.
/// Matched folders are classified by their contents check.
fn differ(a: &Entry, b: &Entry) -> Diff {
    if a.directory {
        return Diff::default();
    }
    let size = a.size != b.size;
    let time = time_differs(a.modified, b.modified);
    let attr = (a.attributes ^ b.attributes) & FILE_DIFF_ATTRIBUTES != 0;
    Diff {
        kind: if size || time || attr { 3 } else { 0 },
        size,
        time,
        attr,
        pending: false,
    }
}

fn time_differs(a: std::time::SystemTime, b: std::time::SystemTime) -> bool {
    a.duration_since(b).unwrap_or_else(|e| e.duration()) > Duration::from_secs(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::Arc, time::UNIX_EPOCH};
    fn align(old: &[Entry], new: &[Entry], only_diff: bool) -> (Aligned, [Vec<Entry>; 2]) {
        super::align(old, new, only_diff, &FolderStates::new())
    }

    fn entry(name: &str, directory: bool, size: u64, secs: u64) -> Entry {
        Entry {
            path: Arc::new(PathBuf::from(name)),
            name: name.into(),
            name_fold: name.to_lowercase(),
            location: String::new(),
            stem: name.into(),
            ext: String::new(),
            size,
            modified: UNIX_EPOCH + Duration::from_secs(secs),
            attributes: 0x20,
            type_name: "".into(),
            directory,
            wide_name: name.encode_utf16().chain(Some(0)).collect(),
            type_icon: "".into(),
            icon: "".into(),
        }
    }
    fn folder_changes(
        old: &[Entry],
        new: &[Entry],
        show_hidden: bool,
        stop: &AtomicBool,
    ) -> FolderStates {
        folder_pairs(old, new)
            .into_iter()
            .filter(|[a, b]| children_differ(a, b, show_hidden, stop))
            .map(|[a, _]| (a, FolderState::Changed))
            .collect()
    }

    #[test]
    fn pending_folders_stay_visible_until_resolved() {
        let mut old = [entry("folder", true, 0, 0)];
        let new = [entry("folder", true, 0, 0)];
        old[0].path = Arc::new(PathBuf::from("old/folder"));
        let [[a, _]]: [[Arc<PathBuf>; 2]; 1] = folder_pairs(&old, &new).try_into().unwrap();
        let mut states = FolderStates::from([(a.clone(), FolderState::Pending)]);
        let (aligned, _) = super::align(&old, &new, true, &states);
        assert!(aligned.diffs[0].pending);
        assert_eq!(aligned.pending, 1);
        assert_eq!(aligned.counts, [0; 4]);
        states.remove(&a);
        let (aligned, _) = super::align(&old, &new, true, &states);
        assert!(aligned.pairs.is_empty());
        assert_eq!(aligned.counts, [0, 0, 0, 1]);
    }

    #[test]
    fn folder_diff_checks_nested_metadata_and_skips_reparse_points() {
        let root = std::env::temp_dir().join(format!("enyfile-folder-diff-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Cleanup(root);
        let a = root.0.join("old");
        let b = root.0.join("new");
        std::fs::create_dir_all(a.join("sub")).unwrap();
        std::fs::create_dir_all(b.join("sub")).unwrap();
        let mut old = [entry("folder", true, 0, 0)];
        let mut new = [entry("folder", true, 0, 0)];
        old[0].path = Arc::new(a.clone());
        new[0].path = Arc::new(b.clone());
        let stop = AtomicBool::new(false);
        assert!(folder_changes(&old, &new, false, &stop).is_empty());

        let direct = a.join("direct.txt");
        std::fs::write(&direct, b"only on the old side").unwrap();
        let folders = folder_changes(&old, &new, false, &stop);
        let (aligned, _) = super::align(&old, &new, true, &folders);
        assert_eq!(aligned.diffs[0].kind, 3);
        std::fs::remove_file(direct).unwrap();

        std::fs::write(a.join("sub/file.txt"), b"old").unwrap();
        std::fs::write(b.join("sub/FILE.TXT"), b"longer").unwrap();
        let folders = folder_changes(&old, &new, false, &stop);
        let (aligned, _) = super::align(&old, &new, true, &folders);
        assert_eq!(aligned.counts, [0, 0, 1, 0]);
        assert_eq!(aligned.diffs[0].kind, 3);

        new[0].attributes |= FILE_ATTRIBUTE_REPARSE_POINT.0;
        assert!(folder_changes(&old, &new, false, &stop).is_empty());
    }

    #[test]
    fn rows_line_up_by_name() {
        let old = [
            entry("docs", true, 0, 0),
            entry("a.txt", false, 1, 100),
            entry("b.txt", false, 5, 100),
            entry("file2", false, 1, 100),
        ];
        let new = [
            entry("lib", true, 0, 0),
            entry("A.TXT", false, 1, 101),
            entry("b.txt", false, 6, 100),
            entry("file10", false, 1, 100),
        ];
        let (aligned, shown) = align(&old, &new, false);
        let names = |side: usize| -> Vec<String> {
            aligned
                .pairs
                .iter()
                .map(|p| p[side].map_or("".into(), |i| shown[side][i].name.clone()))
                .collect()
        };
        assert_eq!(names(0), ["docs", "", "a.txt", "b.txt", "file2", ""]);
        assert_eq!(names(1), ["", "lib", "A.TXT", "b.txt", "", "file10"]);
        let kinds: Vec<i32> = aligned.diffs.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            [2, 1, 0, 3, 2, 1],
            "a.txt: 1 s apart and a different case are the same"
        );
        assert_eq!(
            aligned.diffs[3],
            Diff {
                kind: 3,
                pending: false,
                size: true,
                time: false,
                attr: false
            }
        );
        assert_eq!(aligned.counts, [2, 2, 1, 1]);
        assert_eq!(
            aligned.row_of(1, 1),
            Some(2),
            "A.TXT is the second entry shown on the right, row 2"
        );
        let (only, shown) = align(&old, &new, true);
        assert_eq!(only.pairs.len(), 5);
        assert!(shown[0].iter().all(|e| e.name != "a.txt"));
    }
    #[test]
    fn a_folder_and_a_file_of_one_name_are_two_rows() {
        let (aligned, _) = align(
            &[entry("data", true, 0, 0)],
            &[entry("data", false, 1, 0)],
            false,
        );
        let kinds: Vec<i32> = aligned.diffs.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, [2, 1]);
        assert_eq!(aligned.pairs, [[Some(0), None], [None, Some(0)]]);
    }
    #[test]
    fn which_cells_differ() {
        let with = |mut e: Entry, attributes: u32| {
            e.attributes = attributes;
            e
        };
        let old = [
            entry("dir", true, 0, 100),
            entry("t2", false, 1, 100),
            entry("t3", false, 1, 100),
            with(entry("r", false, 1, 100), 0x20),
            with(entry("n", false, 1, 100), 0x20),
        ];
        let new = [
            with(entry("dir", true, 4096, 103), 0x21),
            entry("t2", false, 1, 98),
            entry("t3", false, 1, 103),
            with(entry("r", false, 1, 100), 0x21),
            with(entry("n", false, 1, 100), 0xA0),
        ];
        let (aligned, shown) = align(&old, &new, false);
        let diff = |name: &str| {
            aligned.diffs[aligned
                .pairs
                .iter()
                .position(|p| shown[0][p[0].unwrap()].name == name)
                .unwrap()]
        };
        assert_eq!(diff("dir"), Diff::default(), "folder metadata is ignored");
        assert_eq!(
            diff("t2").kind,
            0,
            "2 s apart (either way) is the same time"
        );
        assert_eq!(
            diff("t3"),
            Diff {
                kind: 3,
                pending: false,
                size: false,
                time: true,
                attr: false
            }
        );
        assert_eq!(
            diff("r"),
            Diff {
                kind: 3,
                pending: false,
                size: false,
                time: false,
                attr: true
            },
            "read-only differs"
        );
        assert_eq!(
            diff("n").kind,
            0,
            "attributes other than A R H S are ignored"
        );
    }
    #[test]
    fn one_side_empty() {
        let new = [entry("a", false, 1, 0), entry("b", false, 1, 0)];
        let (aligned, shown) = align(&[], &new, false);
        assert_eq!(aligned.counts, [0, 2, 0, 0]);
        assert!(shown[0].is_empty());
        assert_eq!(aligned.rows[1], [0, 1]);
        let (aligned, shown) = align(&new, &[], true);
        assert_eq!(aligned.counts, [2, 0, 0, 0]);
        assert_eq!(shown[0].len(), 2);
    }
}
