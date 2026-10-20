//! Ctrl+Shift+D: two folders side by side, row by row (only the entries in them; the
//! contents of subfolders are not compared). The old side is the left / upper pane.
use crate::listing::{self, Entry};
use std::{cmp::Ordering, time::Duration};

/// How one row differs. `kind`: 0 same, 1 only on the new side ("+"), 2 only on the old
/// side ("-"), 3 on both sides but different (the differing cells are flagged).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Diff {
    pub kind: i32,
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
}
impl Aligned {
    pub fn row_of(&self, side: usize, entry: usize) -> Option<usize> {
        self.rows[side].get(entry).copied()
    }
}

/// Matches `old` and `new` by name (both in `listing::name_order`). Returns the rows and
/// the entries each side shows, in row order; with `only_diff` equal rows are left out.
pub fn align(old: &[Entry], new: &[Entry], only_diff: bool) -> (Aligned, [Vec<Entry>; 2]) {
    let mut aligned = Aligned::default();
    let mut shown: [Vec<Entry>; 2] = [Vec::new(), Vec::new()];
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let (a, b) = (old.get(i), new.get(j));
        let (pair, diff) = match (a, b) {
            (Some(a), Some(b)) if a.directory == b.directory && a.name_fold == b.name_fold => {
                i += 1;
                j += 1;
                ([Some(a), Some(b)], differ(a, b))
            }
            (Some(a), Some(b)) if listing::name_order(a, b) == Ordering::Greater => {
                j += 1;
                ([None, Some(b)], Diff { kind: 1, ..Diff::default() })
            }
            (Some(a), _) => {
                i += 1;
                ([Some(a), None], Diff { kind: 2, ..Diff::default() })
            }
            (None, Some(b)) => {
                j += 1;
                ([None, Some(b)], Diff { kind: 1, ..Diff::default() })
            }
            (None, None) => break,
        };
        let count = match diff.kind {
            2 => 0,
            1 => 1,
            3 => 2,
            _ => 3,
        };
        aligned.counts[count] += 1;
        if only_diff && diff.kind == 0 {
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

/// Size (files only), modified time (2 s apart counts as the same: FAT keeps even seconds)
/// and the A R H S attributes.
fn differ(a: &Entry, b: &Entry) -> Diff {
    let apart = a.modified.duration_since(b.modified).unwrap_or_else(|e| e.duration());
    let size = !a.directory && a.size != b.size;
    let time = apart > Duration::from_secs(2);
    let attr = (a.attributes ^ b.attributes) & 0x27 != 0;
    Diff { kind: if size || time || attr { 3 } else { 0 }, size, time, attr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::Arc, time::UNIX_EPOCH};
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
    #[test]
    fn rows_line_up_by_name() {
        let old = [entry("docs", true, 0, 0), entry("a.txt", false, 1, 100), entry("b.txt", false, 5, 100), entry("file2", false, 1, 100)];
        let new = [entry("lib", true, 0, 0), entry("A.TXT", false, 1, 101), entry("b.txt", false, 6, 100), entry("file10", false, 1, 100)];
        let (aligned, shown) = align(&old, &new, false);
        let names = |side: usize| -> Vec<String> {
            aligned.pairs.iter().map(|p| p[side].map_or("".into(), |i| shown[side][i].name.clone())).collect()
        };
        assert_eq!(names(0), ["docs", "", "a.txt", "b.txt", "file2", ""]);
        assert_eq!(names(1), ["", "lib", "A.TXT", "b.txt", "", "file10"]);
        let kinds: Vec<i32> = aligned.diffs.iter().map(|d| d.kind).collect();
        assert_eq!(kinds, [2, 1, 0, 3, 2, 1], "a.txt: 1 s apart and a different case are the same");
        assert_eq!(aligned.diffs[3], Diff { kind: 3, size: true, time: false, attr: false });
        assert_eq!(aligned.counts, [2, 2, 1, 1]);
        assert_eq!(aligned.row_of(1, 1), Some(2), "A.TXT is the second entry shown on the right, row 2");
        let (only, shown) = align(&old, &new, true);
        assert_eq!(only.pairs.len(), 5);
        assert!(shown[0].iter().all(|e| e.name != "a.txt"));
    }
}
