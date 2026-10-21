//! Paste planning. Conflicts are asked one at a time (NexusFile style); "merge" walks into
//! folders and asks about their contents the same way. Nothing is changed until every
//! question is answered, so Cancel at any point leaves the file system untouched.
//! Runs on a worker thread: it stats and lists folders, and blocks while the user decides.
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug)]
pub struct Meta {
    pub directory: bool,
    pub size: u64,
    pub modified: SystemTime,
}
fn meta(path: &Path) -> Option<Meta> {
    let m = std::fs::metadata(path).ok()?;
    Some(Meta {
        directory: m.is_dir(),
        size: m.len(),
        modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Folder,
    /// A file meets a folder of the same name (or the reverse): only skip or rename.
    Mismatch,
}
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Overwrite,
    /// Overwrite when the incoming file is newer, otherwise skip.
    Newer,
    Skip,
    Rename(String),
    Merge,
    /// Set the existing folder aside, paste, then delete it (not via the Recycle Bin).
    /// Folders only; the incoming folder may lie inside the existing one.
    Replace,
    Cancel,
}
pub struct Question {
    pub kind: Kind,
    pub moved: bool,
    pub source: PathBuf,
    pub target: PathBuf,
    pub from: Meta,
    pub to: Meta,
    pub auto_name: String,
    /// Lower-cased names a rename may not use in the target folder.
    pub taken: HashSet<String>,
}

/// One copy/move into an existing folder; `name` is Some for a rename.
pub struct Op {
    pub source: PathBuf,
    pub dir: PathBuf,
    pub name: Option<String>,
    /// The target did not exist when planned; executing must not overwrite one.
    pub fresh: bool,
}
impl Op {
    pub fn target(&self) -> PathBuf {
        self.dir.join(match &self.name {
            Some(n) => std::ffi::OsStr::new(n.as_str()),
            None => self.source.file_name().unwrap(),
        })
    }
}
#[derive(Default)]
pub struct Plan {
    pub moved: bool,
    /// Existing items replaced (deleted, not recycled, after pasting), with the object each
    /// one was when the user chose to replace it.
    pub remove: Vec<PathBuf>,
    pub remove_ids: Vec<Option<crate::pathid::Ident>>,
    /// Items left out because the destination is the item itself or lies inside it.
    pub inside: usize,
    pub ops: Vec<Op>,
    /// Source folders merged during a move, innermost first: removed if left empty.
    pub merged: Vec<PathBuf>,
    /// Top-level results in the destination, to select after pasting.
    pub targets: Vec<PathBuf>,
    pub skipped: bool,
}

fn fold(name: &str) -> String {
    name.to_lowercase()
}
const INVALID: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];
/// Why `name` cannot be used as a new name, if it cannot.
pub fn name_error(name: &str, taken: &HashSet<String>) -> Option<&'static str> {
    let base = fold(name.split('.').next().unwrap_or(""));
    if name.trim().is_empty() {
        Some("名前を入力してください")
    } else if name.contains(INVALID) || name.chars().any(char::is_control) {
        Some("使えない文字が含まれています")
    } else if name.ends_with(['.', ' ']) {
        Some("末尾に . や空白は使えません")
    } else if RESERVED.contains(&base.trim_end()) {
        Some("Windowsの予約名です")
    } else if taken.contains(&fold(name)) {
        Some("同じ名前が既にあります")
    } else {
        None
    }
}
/// Byte length of the part before the extension (folders: the whole name).
pub fn stem_len(name: &str, directory: bool) -> usize {
    match name.rfind('.') {
        Some(dot) if dot > 0 && !directory => dot,
        _ => name.len(),
    }
}
/// "name (2).ext", like Explorer's "keep both".
pub fn auto_name(name: &str, directory: bool, taken: &HashSet<String>) -> String {
    let (stem, ext) = name.split_at(stem_len(name, directory));
    (2..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|candidate| !taken.contains(&fold(candidate)))
        .unwrap()
}

struct Planner<'a> {
    plan: Plan,
    /// Names present or planned per destination folder.
    names: HashMap<PathBuf, HashSet<String>>,
    rules: HashMap<Kind, Answer>,
    ask: &'a mut dyn FnMut(Question) -> (Answer, bool),
    ids: crate::pathid::Idents,
}
struct Cancelled;
impl Planner<'_> {
    fn names(&mut self, dir: &Path) -> Result<&mut HashSet<String>, String> {
        if !self.names.contains_key(dir) {
            let set = std::fs::read_dir(dir)
                .map_err(|e| format!("{}: {e}", dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| fold(&e.file_name().to_string_lossy()))
                .collect();
            self.names.insert(dir.to_path_buf(), set);
        }
        Ok(self.names.get_mut(dir).unwrap())
    }
    fn push(&mut self, source: &Path, dir: &Path, name: Option<String>, fresh: bool, top: bool) {
        let op = Op {
            source: source.to_path_buf(),
            dir: dir.to_path_buf(),
            name,
            fresh,
        };
        if top {
            self.plan.targets.push(op.target());
        }
        self.plan.ops.push(op);
    }
    fn place(&mut self, source: &Path, dir: &Path, top: bool) -> Result<Result<(), Cancelled>, String> {
        let name = source
            .file_name()
            .ok_or_else(|| format!("{} は貼り付けできません", source.display()))?
            .to_string_lossy()
            .into_owned();
        // A folder cannot go into itself or below itself (Windows refuses, and a Replace
        // first would already have deleted something): left out before any question.
        if self.ids.within(dir, source) {
            self.plan.inside += 1;
            self.plan.skipped = true;
            return Ok(Ok(()));
        }
        let names = self.names(dir)?;
        if !names.contains(&fold(&name)) {
            names.insert(fold(&name));
            self.push(source, dir, None, true, top);
            return Ok(Ok(()));
        }
        let target = dir.join(&name);
        let from = meta(source).ok_or_else(|| format!("{name} が見つかりません"))?;
        // The item is already here (copied from this folder along with items from others):
        // a copy becomes "name (2)" as when everything comes from this folder; a move has
        // nothing to do. Asking would offer Replace / Overwrite of the item with itself.
        if self.ids.same(source, &target) {
            if !self.plan.moved {
                let auto = auto_name(&name, from.directory, self.names(dir)?);
                self.names(dir)?.insert(fold(&auto));
                self.push(source, dir, Some(auto), true, top);
            } else if top {
                self.plan.targets.push(target);
            }
            return Ok(Ok(()));
        }
        // Two clipboard items with the same name both land here: the second one conflicts.
        let to = meta(&target).unwrap_or_else(|| from.clone());
        let kind = match (from.directory, to.directory) {
            (true, true) => Kind::Folder,
            (false, false) => Kind::File,
            _ => Kind::Mismatch,
        };
        let names = self.names(dir)?;
        let taken = names.clone();
        let auto = auto_name(&name, from.directory, &taken);
        let rule = self.rules.get(&kind);
        let answer = match rule {
            Some(Answer::Rename(_)) => Answer::Rename(auto),
            Some(rule) => rule.clone(),
            None => {
                let (answer, all) = (self.ask)(Question {
                    kind,
                    moved: self.plan.moved,
                    source: source.to_path_buf(),
                    target: target.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    auto_name: auto,
                    taken,
                });
                if all && kind != Kind::Mismatch && answer != Answer::Cancel {
                    self.rules.insert(kind, answer.clone());
                }
                answer
            }
        };
        match answer {
            Answer::Cancel => return Ok(Err(Cancelled)),
            Answer::Skip => self.plan.skipped = true,
            Answer::Newer if from.modified <= to.modified => self.plan.skipped = true,
            Answer::Overwrite | Answer::Newer if kind == Kind::File => {
                self.push(source, dir, None, false, top)
            }
            Answer::Rename(new) => {
                self.names(dir)?.insert(fold(&new));
                self.push(source, dir, Some(new), true, top);
            }
            Answer::Merge if kind == Kind::Folder => {
                let mut children: Vec<PathBuf> = std::fs::read_dir(source)
                    .map_err(|e| format!("{}: {e}", source.display()))?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .collect();
                children.sort();
                for child in children {
                    if let Err(c) = self.place(&child, &target, false)? {
                        return Ok(Err(c));
                    }
                }
                if self.plan.moved {
                    self.plan.merged.push(source.to_path_buf());
                }
                if top {
                    self.plan.targets.push(target);
                }
            }
            Answer::Replace if kind == Kind::Folder => {
                self.plan.remove_ids.push(crate::pathid::ident(&target));
                self.plan.remove.push(target);
                self.push(source, dir, None, false, top);
            }
            // Overwrite/merge across a file/folder mismatch is not offered; treat as skip.
            _ => self.plan.skipped = true,
        }
        Ok(Ok(()))
    }
}

/// Plans pasting `sources` into `dir`, asking `ask` about each conflict. `ask` returns the
/// answer and whether to apply it to all later conflicts of the same kind.
/// Ok(None) means the user cancelled.
pub fn plan(
    sources: &[PathBuf],
    dir: &Path,
    moved: bool,
    ask: &mut dyn FnMut(Question) -> (Answer, bool),
) -> Result<Option<Plan>, String> {
    let mut planner = Planner {
        plan: Plan {
            moved,
            ..Plan::default()
        },
        names: HashMap::new(),
        rules: HashMap::new(),
        ask,
        ids: crate::pathid::Idents::default(),
    };
    for source in sources {
        if planner.place(source, dir, true)?.is_err() {
            return Ok(None);
        }
    }
    Ok(Some(planner.plan))
}
/// `inner` relative to `outer`, both resolved as the file system names them now.
fn below(inner: &Path, outer: &Path) -> Option<PathBuf> {
    let (inner, outer) = (std::fs::canonicalize(inner).ok()?, std::fs::canonicalize(outer).ok()?);
    let rest = inner.strip_prefix(&outer).ok()?;
    (!rest.as_os_str().is_empty()).then(|| rest.to_path_buf())
}
impl Plan {
    /// Something appeared where we planned to create a new item: executing would overwrite
    /// it without the user having chosen that.
    pub fn changed(&self) -> bool {
        self.ops.iter().any(|op| op.fresh && op.target().exists())
    }
    /// Checked right before anything is renamed or written, with the disk as it is now
    /// (the user may have taken a while to answer): why the paste must not run, or for each
    /// op whose source lies inside an item being replaced, that item's index in `remove` and
    /// the path below it, to find the source again once the item is set aside.
    pub fn verify(&self) -> Result<Vec<Option<(usize, PathBuf)>>, String> {
        if self.changed() {
            return Err("確認中に貼り付け先が変わったため中止しました".into());
        }
        // All up front: a refusal halfway through would leave items set aside.
        let paths = self.remove.iter().chain(self.ops.iter().flat_map(|op| [&op.source, &op.dir]));
        for path in paths {
            crate::shell::os_path(path)?;
        }
        let mut ids = crate::pathid::Idents::default();
        for (path, planned) in self.remove.iter().zip(&self.remove_ids) {
            if crate::pathid::ident(path) != *planned {
                return Err(format!("置換するフォルダーが確認後に変わったため中止しました: {}", path.display()));
            }
        }
        if let Some(op) = self.ops.iter().find(|op| ids.within(&op.dir, &op.source)) {
            return Err(format!("フォルダーをその中へは貼り付けられません: {}", op.source.display()));
        }
        let mut inside = Vec::new();
        for op in &self.ops {
            let Some(i) = self.remove.iter().position(|path| ids.within(&op.source, path)) else {
                inside.push(None);
                continue;
            };
            // Replacing an item with itself deletes it; a source we cannot find again below
            // the set-aside item would be lost the same way.
            let below = (!ids.same(&op.source, &self.remove[i])).then(|| below(&op.source, &self.remove[i])).flatten();
            match below {
                Some(rest) => inside.push(Some((i, rest))),
                None => return Err(format!("貼り付け元を削除することになるため中止しました: {}", op.source.display())),
            }
        }
        Ok(inside)
    }
    /// After a move: remove merged source folders that ended up empty (skipped items stay).
    pub fn remove_emptied(&self) {
        for dir in &self.merged {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn fixture(tag: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("enyfile-paste-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in [
            ("src/Folder/inner.txt", Some("new")),
            ("src/Folder/sub/deep.txt", Some("deep")),
            ("src/a.txt", Some("a")),
            ("dst/Folder/inner.txt", Some("old")),
            ("dst/Folder/keep.txt", Some("keep")),
            ("dst/a.txt", Some("a-old")),
            ("dst/a (2).txt", Some("taken")),
        ] {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if let Some(text) = text {
                std::fs::write(path, text).unwrap();
            }
        }
        Fixture(root)
    }
    fn names(plan: &Plan) -> Vec<String> {
        plan.ops
            .iter()
            .map(|op| {
                let t = op.target();
                format!("{}/{}", t.parent().unwrap().file_name().unwrap().to_string_lossy(), t.file_name().unwrap().to_string_lossy())
            })
            .collect()
    }
    #[test]
    fn merge_recurses_and_asks_each_conflict() {
        let f = fixture("merge");
        let sources = [f.0.join("src/Folder"), f.0.join("src/a.txt")];
        let mut asked = Vec::new();
        let plan = plan(&sources, &f.0.join("dst"), false, &mut |q| {
            asked.push((q.kind, q.target.file_name().unwrap().to_string_lossy().into_owned(), q.auto_name.clone()));
            match q.kind {
                Kind::Folder => (Answer::Merge, false),
                _ if q.target.ends_with("inner.txt") => (Answer::Overwrite, false),
                _ => (Answer::Rename(q.auto_name.clone()), false),
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            asked,
            [
                (Kind::Folder, "Folder".into(), "Folder (2)".into()),
                (Kind::File, "inner.txt".into(), "inner (2).txt".into()),
                (Kind::File, "a.txt".into(), "a (3).txt".into()),
            ]
        );
        assert_eq!(names(&plan), ["Folder/inner.txt", "Folder/sub", "dst/a (3).txt"]);
        assert!(plan.remove.is_empty() && !plan.skipped && !plan.changed());
        assert_eq!(plan.targets.len(), 2);
    }
    #[test]
    fn apply_to_all_cancel_and_replace() {
        let f = fixture("rules");
        let sources = [f.0.join("src/Folder"), f.0.join("src/a.txt")];
        // Replace for folders; cancel on the next question leaves no plan at all.
        let mut count = 0;
        let cancelled = plan(&sources, &f.0.join("dst"), false, &mut |q| {
            count += 1;
            if q.kind == Kind::Folder { (Answer::Replace, true) } else { (Answer::Cancel, false) }
        })
        .unwrap();
        assert!(cancelled.is_none() && count == 2);
        // "Rename, apply to all" for files does not ask again.
        let mut asked = 0;
        let plan = plan(&sources, &f.0.join("dst"), false, &mut |q| {
            asked += 1;
            match q.kind {
                Kind::Folder => (Answer::Merge, false),
                _ => (Answer::Rename(q.auto_name.clone()), true),
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(asked, 2);
        assert_eq!(names(&plan), ["Folder/inner (2).txt", "Folder/sub", "dst/a (3).txt"]);
        // Newer-only skips an older incoming file.
        let older = plan_newer(&f);
        assert!(older.skipped && older.ops.is_empty());
    }
    fn plan_newer(f: &Fixture) -> Plan {
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        std::fs::File::options().write(true).open(f.0.join("src/a.txt")).unwrap().set_modified(old).unwrap();
        plan(&[f.0.join("src/a.txt")], &f.0.join("dst"), false, &mut |_| (Answer::Newer, false))
            .unwrap()
            .unwrap()
    }
    /// Audit #1: Replace must never delete what is being pasted.
    #[test]
    fn never_replace_the_source() {
        let f = fixture("self");
        let root = std::fs::canonicalize(&f.0).unwrap();
        for dir in [r"w\Folder\Folder", r"w\B", r"x\B"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join(r"w\Folder\Folder\inner.txt"), "inner").unwrap();
        // (a) Folder\Folder pasted next to Folder, with "Replace, apply to all" from B: it
        // is replaced too, and verify says where to find the source once Folder is set
        // aside (shell::tests::replace_with_a_folder_inside_it runs it).
        let nested = crate::shell::shell_path(&root.join(r"w\Folder\Folder"));
        let mut asked = 0;
        let replaced = plan(&[root.join(r"x\B"), nested.clone()], &root.join("w"), false, &mut |_| {
            asked += 1;
            (Answer::Replace, true)
        })
        .unwrap()
        .unwrap();
        assert_eq!(asked, 1);
        assert_eq!(replaced.remove, [root.join(r"w\B"), root.join(r"w\Folder")]);
        assert_eq!(replaced.verify().unwrap(), [None, Some((1, PathBuf::from("Folder")))]);
        // (b) An item already in the destination: a copy is duplicated without a question,
        // a move does nothing.
        let here = root.join(r"dst\a.txt");
        let mut questions = 0;
        let copy = plan(&[crate::shell::shell_path(&here), f.0.join("src/a.txt")], &root.join("dst"), false, &mut |_| {
            questions += 1;
            (Answer::Skip, false)
        })
        .unwrap()
        .unwrap();
        assert_eq!(names(&copy)[0], "dst/a (3).txt", "\"a (2).txt\" is taken");
        assert_eq!(questions, 1, "only src/a.txt is asked about");
        let moved = plan(&[here.clone()], &root.join("dst"), true, &mut |_| unreachable!()).unwrap().unwrap();
        assert!(moved.ops.is_empty() && moved.remove.is_empty());
        // A hand-made plan that replaces the source with itself is refused before anything runs.
        let bad = Plan {
            remove: vec![root.join(r"w\Folder\Folder")],
            remove_ids: vec![crate::pathid::ident(&nested)],
            ops: vec![Op { source: nested, dir: root.join("x"), name: None, fresh: false }],
            ..Plan::default()
        };
        assert!(bad.verify().unwrap_err().contains("貼り付け元を削除"));
        assert!(root.join(r"w\Folder\Folder\inner.txt").exists());
    }
    /// A folder pasted into itself or below itself is left out before any question.
    #[test]
    fn not_into_itself() {
        let f = fixture("inside");
        let folder = f.0.join("src/Folder");
        let mut asked = 0;
        let plan = plan(&[folder.clone(), f.0.join("src/a.txt")], &std::fs::canonicalize(folder.join("sub")).unwrap(), false, &mut |_| {
            asked += 1;
            (Answer::Skip, false)
        })
        .unwrap()
        .unwrap();
        assert_eq!((plan.inside, asked), (1, 0));
        assert_eq!(names(&plan), ["sub/a.txt"]);
        let into_itself = Plan { ops: vec![Op { source: folder.clone(), dir: folder.clone(), name: None, fresh: true }], ..Plan::default() };
        assert!(into_itself.verify().is_err());
    }
    #[test]
    fn rename_validation() {
        let taken: HashSet<String> = ["a.txt", "a (2).txt"].map(String::from).into();
        for (name, ok) in [
            ("A.TXT", false),
            ("a (2).txt", false),
            ("x?.txt", false),
            ("x.", false),
            ("con.txt", false),
            ("", false),
            ("a new.txt", true),
        ] {
            assert_eq!(name_error(name, &taken).is_none(), ok, "{name}");
        }
        assert_eq!(auto_name("a.txt", false, &taken), "a (3).txt");
        assert_eq!(auto_name("v1.2", true, &taken), "v1.2 (2)");
        assert_eq!(stem_len("日本語.txt", false), "日本語".len());
    }
}
