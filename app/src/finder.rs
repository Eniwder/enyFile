//! Ctrl+Alt+Shift+F: one walk of a folder tree kept in memory (names and attributes only,
//! about 100-150 bytes an item), so each change of the search text is matched again
//! without reading the disk. The walk starts when the search bar opens.
use crate::listing::{self, Entry, Pattern, Raw};
use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT};

/// Matches of one query at most; a drive root can match millions of files.
pub const RESULT_LIMIT: usize = 100_000;
/// Items kept in memory at most. A bigger tree is not kept: each query walks the disk again.
pub const INDEX_LIMIT: usize = 1_000_000;
/// Results go to the UI a few times a second, the first ones at once.
const BATCH: Duration = Duration::from_millis(150);

pub enum Event {
    /// Matches of query `id`; Some(hit the result limit) once the query is complete.
    Found(u64, Vec<Entry>, Option<bool>),
    /// The tree is bigger than the index limit: queries now walk the disk.
    Overflow,
    /// Per-file icons (exe, lnk, ...) of the results are ready.
    Icons,
}

/// The walk runs until the finder is dropped (another folder, search closed).
pub struct Finder {
    control: Arc<(Mutex<Option<(u64, Pattern)>>, Condvar)>,
    stop: Arc<AtomicBool>,
}
impl Finder {
    /// Reparse-point folders (junctions, symbolic links) are listed but entered only with
    /// `follow`, and then only when their target is outside everything walked so far:
    /// that rules out cycles and walking the same tree twice.
    pub fn start(root: PathBuf, show_hidden: bool, follow: bool, limit: usize, send: impl Fn(Event) -> bool + Send + 'static) -> Self {
        let control = Arc::new((Mutex::new(None), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let walk = Walk {
            queue: VecDeque::from([root.clone()]),
            walked: vec![root.clone()],
            root,
            show_hidden,
            follow,
            limit,
            control: control.clone(),
            stop: stop.clone(),
            send: Box::new(send),
            dirs: Vec::new(),
            items: Vec::new(),
            overflow: false,
            query: None,
        };
        std::thread::spawn(move || walk.run());
        Self { control, stop }
    }
    /// Replaces the query being answered; its results come as `Event::Found(id, ..)`.
    pub fn query(&self, id: u64, pattern: Pattern) {
        let (lock, cv) = &*self.control;
        *lock.lock().unwrap() = Some((id, pattern));
        cv.notify_one();
    }
    /// Set once the finder is dropped.
    pub fn stopped(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
}
impl Drop for Finder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.control.1.notify_one();
    }
}

struct Dir {
    path: PathBuf,
    /// Relative to the root, e.g. "src\ui\", and lower-cased for matching.
    location: String,
    fold: String,
}
struct Item {
    dir: u32,
    directory: bool,
    attributes: u32,
    size: u64,
    modified: SystemTime,
    name: Box<OsStr>,
    fold: Box<str>,
}
struct Query {
    id: u64,
    pattern: Pattern,
    /// Items matched so far.
    pos: usize,
    count: usize,
    batch: Vec<Entry>,
    sent: Option<Instant>,
    done: bool,
    icons: Vec<(Arc<str>, Arc<PathBuf>)>,
}
impl Query {
    fn push(&mut self, entry: Entry) {
        if entry.icon != entry.type_icon {
            self.icons.push((entry.icon.clone(), entry.path.clone()));
        }
        self.batch.push(entry);
        self.count += 1;
    }
}
struct Walk {
    root: PathBuf,
    show_hidden: bool,
    follow: bool,
    limit: usize,
    control: Arc<(Mutex<Option<(u64, Pattern)>>, Condvar)>,
    stop: Arc<AtomicBool>,
    send: Box<dyn Fn(Event) -> bool + Send>,
    /// Breadth first, so shallow matches come first.
    queue: VecDeque<PathBuf>,
    walked: Vec<PathBuf>,
    dirs: Vec<Dir>,
    items: Vec<Item>,
    /// The tree outgrew `limit`: nothing is kept and each query walks again.
    overflow: bool,
    query: Option<Query>,
}
impl Walk {
    fn run(mut self) {
        // File type names and icons come from the Shell, which needs COM on this thread.
        let _com = crate::shell::Apartment::new();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            if let Some((id, pattern)) = self.control.0.lock().unwrap().take() {
                self.query = Some(Query { id, pattern, pos: 0, count: 0, batch: Vec::new(), sent: None, done: false, icons: Vec::new() });
                if self.overflow {
                    self.queue = VecDeque::from([self.root.clone()]);
                    self.walked = vec![self.root.clone()];
                }
            }
            let open = self.query.as_ref().is_some_and(|q| !q.done);
            let alive = if open && self.query.as_ref().is_some_and(|q| q.pos < self.items.len()) {
                self.scan()
            } else if !self.queue.is_empty() && (open || !self.overflow) {
                let dir = self.queue.pop_front().unwrap();
                self.read(dir)
            } else if open {
                self.finish(false)
            } else {
                // Everything walked and answered: wait for the next query.
                let (lock, cv) = &*self.control;
                let waiting = lock.lock().unwrap();
                drop(cv.wait_while(waiting, |q| q.is_none() && !self.stop.load(Ordering::Relaxed)).unwrap());
                true
            };
            if !alive {
                return;
            }
        }
    }
    /// Matches the next part of the kept items against the query.
    fn scan(&mut self) -> bool {
        let Some(q) = self.query.as_mut() else { return true };
        let end = (q.pos + 8192).min(self.items.len());
        for item in &self.items[q.pos..end] {
            let dir = &self.dirs[item.dir as usize];
            if q.pattern.matches_in(&dir.fold, &item.fold) {
                let raw = Raw {
                    name: item.name.to_os_string(),
                    size: item.size,
                    modified: item.modified,
                    attributes: item.attributes,
                    directory: item.directory,
                };
                q.push(located(listing::entry(&dir.path, raw), &dir.location));
                if q.count >= RESULT_LIMIT {
                    return self.finish(true);
                }
            }
        }
        q.pos = end;
        self.flush()
    }
    /// Reads one folder: kept for later queries, or matched right away past the limit.
    fn read(&mut self, dir: PathBuf) -> bool {
        // Unreadable folders (access denied, vanished) are skipped, as Explorer's search does.
        let Ok(rows) = listing::scan(&dir, self.show_hidden) else { return true };
        for raw in &rows {
            if raw.attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0 {
                continue;
            }
            let path = dir.join(&raw.name);
            if raw.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
                self.queue.push_back(path);
            } else if self.follow {
                if let Ok(target) = std::fs::canonicalize(&path) {
                    if !self.walked.iter().any(|w| target.starts_with(w)) {
                        self.walked.push(target);
                        self.queue.push_back(path);
                    }
                }
            }
        }
        let location = match dir.strip_prefix(&self.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => format!("{}\\", rel.display()),
            _ => String::new(),
        };
        let dir = Dir { fold: location.to_lowercase(), path: dir, location };
        if self.overflow {
            let Some(q) = self.query.as_mut() else { return true };
            for raw in rows {
                if q.pattern.matches_in(&dir.fold, &raw.name.to_string_lossy().to_lowercase()) {
                    q.push(located(listing::entry(&dir.path, raw), &dir.location));
                    if q.count >= RESULT_LIMIT {
                        return self.finish(true);
                    }
                }
            }
            return self.flush();
        }
        let index = self.dirs.len() as u32;
        self.dirs.push(dir);
        self.items.extend(rows.into_iter().map(|raw| Item {
            dir: index,
            directory: raw.directory,
            attributes: raw.attributes,
            size: raw.size,
            modified: raw.modified,
            fold: raw.name.to_string_lossy().to_lowercase().into_boxed_str(),
            name: raw.name.into_boxed_os_str(),
        }));
        if self.items.len() > self.limit {
            return self.give_up();
        }
        true
    }
    /// Too many items to keep: the query being answered gets the kept ones first, then
    /// they are dropped and the walk goes on matching as it reads.
    fn give_up(&mut self) -> bool {
        while self.query.as_ref().is_some_and(|q| !q.done && q.pos < self.items.len()) {
            if !self.scan() {
                return false;
            }
        }
        self.overflow = true;
        self.items = Vec::new();
        self.dirs = Vec::new();
        (self.send)(Event::Overflow)
    }
    fn flush(&mut self) -> bool {
        let Some(q) = self.query.as_mut() else { return true };
        if q.batch.is_empty() || q.sent.is_some_and(|t| t.elapsed() < BATCH) {
            return true;
        }
        q.sent = Some(Instant::now());
        let batch = std::mem::take(&mut q.batch);
        (self.send)(Event::Found(q.id, batch, None))
    }
    fn finish(&mut self, limited: bool) -> bool {
        let Some(q) = self.query.as_mut() else { return true };
        q.done = true;
        let batch = std::mem::take(&mut q.batch);
        let icons = std::mem::take(&mut q.icons);
        if !(self.send)(Event::Found(q.id, batch, Some(limited))) {
            return false;
        }
        // Files with their own icon after the list is complete; a new query goes first.
        let mut loaded = false;
        for (key, path) in &icons {
            if self.stop.load(Ordering::Relaxed) || self.control.0.lock().unwrap().is_some() {
                break;
            }
            if crate::icons::get(key).is_none() {
                crate::icons::ensure_path(key, &crate::shell::shell_path(path));
                loaded = true;
            }
        }
        !loaded || (self.send)(Event::Icons)
    }
}
/// Sorting by name then orders the results by relative path.
fn located(mut entry: Entry, location: &str) -> Entry {
    entry.wide_name = format!("{location}{}", entry.name).encode_utf16().chain(Some(0)).collect();
    entry.location = location.to_string();
    entry
}
