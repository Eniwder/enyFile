//! Alt+Enter: enyFile's own properties of one file or folder: times and attributes, where a
//! download came from (the Zone.Identifier stream browsers attach), alternate data streams
//! and the SHA-256. What comes from the file is only shown as text; no URL is opened.
//! A folder's size is added up on a worker without entering junctions and symbolic links.
//! SHA-256 is Windows' own (CNG, bcrypt.dll), so no hashing crate to keep up to date.
use super::*;
use std::{
    io::Read,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    sync::atomic::{AtomicBool, Ordering},
    time::SystemTime,
};
use windows::{
    Win32::{
        Security::Cryptography::{BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash, BCryptHashData},
        Storage::FileSystem::{
            FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard, GetFileVersionInfoSizeW, GetFileVersionInfoW,
            VS_FIXEDFILEINFO, VerQueryValueW, WIN32_FIND_STREAM_DATA,
        },
    },
    core::PCWSTR,
};

/// Read on a worker thread: the item and its streams may sit on a slow drive.
pub struct Info {
    pub directory: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// Of the item itself, not a link's target.
    pub attributes: u32,
    pub link: Option<PathBuf>,
    /// File version from the version resource (exe, dll, ...), as Explorer's 詳細 tab shows it.
    pub version: Option<String>,
    pub zone: Option<Zone>,
    /// Alternate data streams: name (without ":$DATA") and size.
    pub streams: Vec<(String, u64)>,
}
/// The [ZoneTransfer] section of a Zone.Identifier stream.
#[derive(Debug, Default, PartialEq)]
pub struct Zone {
    pub id: Option<u32>,
    pub host: Option<String>,
    pub referrer: Option<String>,
}
/// A folder's tree, added up by `usage`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub size: u64,
    pub files: u64,
    pub folders: u64,
    /// Junctions, symbolic links and mount points: counted, not entered.
    pub links: u64,
    /// Folders that could not be listed (access denied, gone; never the top one, which fails
    /// the whole count) or only partly (an error midway, an entry without metadata).
    pub unreadable: u64,
}
pub enum Measuring {
    Progress(Usage),
    Done(Usage),
    Failed(String),
}
pub enum Hashing {
    Progress(u64, u64),
    Done(String),
    Failed(String),
}

const ZONE_STREAM: &str = "Zone.Identifier";
/// Attribute bits that mean the data is not on this machine: reading it downloads it.
const REMOTE: u32 = 0x1000 | 0x40000 | 0x400000;

pub fn read(path: &Path) -> Result<Info, String> {
    let own = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    // Size and times of what a link points to, as opening it would see; the link's own when
    // the target is gone.
    let meta = std::fs::metadata(path).unwrap_or_else(|_| own.clone());
    let link = own.file_type().is_symlink().then(|| std::fs::read_link(path).ok()).flatten();
    let streams = streams(path);
    let zone = streams
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(ZONE_STREAM))
        .then(|| zone(path))
        .flatten();
    Ok(Info {
        directory: meta.is_dir(),
        size: meta.len(),
        modified: meta.modified().ok(),
        created: meta.created().ok(),
        attributes: own.file_attributes(),
        link,
        // A cloud file would be downloaded just to look.
        version: (!meta.is_dir() && own.file_attributes() & REMOTE == 0).then(|| version(path)).flatten(),
        zone,
        streams,
    })
}

/// Adds up the files below `root`; Ok(None) when cancelled. Folders that stand for another
/// name are counted in `links` and not entered: Explorer goes through them, which counts the
/// same data twice (or another drive, or loops). The directory listing carries each entry's
/// size and reparse tag (std reads both from the find data), so no file is opened.
/// `progress` gets the totals after each batch of entries and stops the work by returning false.
pub fn usage(root: &Path, cancel: &AtomicBool, mut progress: impl FnMut(&Usage) -> bool) -> Result<Option<Usage>, String> {
    const BATCH: usize = 512;
    let mut total = Usage::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if dir == root => return Err(e.to_string()),
            Err(_) => {
                total.unreadable += 1;
                continue;
            }
        };
        // An error midway keeps what was read, and the folder counts as unreadable. The listing
        // stops there: std's next call would only retry FindNextFileW, which may fail for good.
        let mut partial = false;
        for (n, entry) in entries.enumerate() {
            let Ok(entry) = entry else {
                partial = true;
                break;
            };
            let Ok(meta) = entry.metadata() else {
                partial = true;
                continue;
            };
            // is_symlink: a reparse point with a name-surrogate tag (junction, symbolic link,
            // mount point), not a cloud placeholder, which holds real data.
            if meta.file_type().is_symlink() {
                total.links += 1;
            } else if meta.is_dir() {
                total.folders += 1;
                pending.push(entry.path());
            } else {
                total.files += 1;
                total.size += meta.len();
            }
            if (n + 1) % BATCH == 0 && (cancel.load(Ordering::Relaxed) || !progress(&total)) {
                return Ok(None);
            }
        }
        total.unreadable += u64::from(partial);
        if cancel.load(Ordering::Relaxed) || !progress(&total) {
            return Ok(None);
        }
    }
    Ok(Some(total))
}

fn version(path: &Path) -> Option<String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        // u32s: VS_FIXEDFILEINFO inside the block is read in place.
        let mut block = vec![0u32; (size as usize).div_ceil(4)];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, block.as_mut_ptr().cast()).ok()?;
        let mut fixed = std::ptr::null_mut();
        let mut len = 0;
        let root: Vec<u16> = "\\".encode_utf16().chain(Some(0)).collect();
        if !VerQueryValueW(block.as_ptr().cast(), PCWSTR(root.as_ptr()), &mut fixed, &mut len).as_bool()
            || fixed.is_null()
            || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let f = std::ptr::read_unaligned(fixed as *const VS_FIXEDFILEINFO);
        let (ms, ls) = (f.dwFileVersionMS, f.dwFileVersionLS);
        Some(format!("{}.{}.{}.{}", ms >> 16, ms & 0xFFFF, ls >> 16, ls & 0xFFFF))
    }
}

fn streams(path: &Path) -> Vec<(String, u64)> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data = WIN32_FIND_STREAM_DATA::default();
    let mut found = Vec::new();
    // Fails with ERROR_HANDLE_EOF when there is no stream at all (most folders).
    let Ok(handle) = (unsafe { FindFirstStreamW(PCWSTR(wide.as_ptr()), FindStreamInfoStandard, &mut data as *mut _ as *mut _, None) }) else {
        return found;
    };
    loop {
        let len = data.cStreamName.iter().position(|&c| c == 0).unwrap_or(data.cStreamName.len());
        let full = String::from_utf16_lossy(&data.cStreamName[..len]);
        // ":name:$DATA"; the unnamed main stream is "::$DATA".
        let name = full.strip_prefix(':').unwrap_or(&full);
        let name = name.strip_suffix(":$DATA").unwrap_or(name);
        if !name.is_empty() {
            found.push((name.to_string(), data.StreamSize.max(0) as u64));
        }
        if unsafe { FindNextStreamW(handle, &mut data as *mut _ as *mut _) }.is_err() {
            break;
        }
    }
    let _ = unsafe { FindClose(handle) };
    found
}

fn zone(path: &Path) -> Option<Zone> {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":");
    stream.push(ZONE_STREAM);
    // Anyone can write this stream: read a bounded amount.
    let mut bytes = Vec::new();
    std::fs::File::open(PathBuf::from(stream)).ok()?.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    Some(parse_zone(&bytes))
}

pub fn parse_zone(bytes: &[u8]) -> Zone {
    let text = match bytes {
        [0xFF, 0xFE, rest @ ..] => String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    };
    let mut zone = Zone::default();
    let mut section = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.eq_ignore_ascii_case("[ZoneTransfer]");
            continue;
        }
        let Some((key, value)) = line.split_once('=').filter(|_| section) else { continue };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "zoneid" => zone.id = value.parse().ok(),
            "hosturl" if !value.is_empty() => zone.host = Some(value.to_string()),
            "referrerurl" if !value.is_empty() => zone.referrer = Some(value.to_string()),
            _ => {}
        }
    }
    zone
}

fn zone_name(id: u32) -> String {
    let name = match id {
        0 => "ローカル コンピューター",
        1 => "ローカル イントラネット",
        2 => "信頼済みサイト",
        3 => "インターネット",
        4 => "制限付きサイト",
        _ => "不明",
    };
    format!("{name} ({id})")
}

struct Hash(BCRYPT_HASH_HANDLE);
impl Drop for Hash {
    fn drop(&mut self) {
        let _ = unsafe { BCryptDestroyHash(self.0) };
    }
}
fn check(status: windows::Win32::Foundation::NTSTATUS) -> Result<(), String> {
    if status.0 < 0 { Err(format!("CNG エラー 0x{:08X}", status.0 as u32)) } else { Ok(()) }
}
/// SHA-256 of the file as lowercase hex; Ok(None) when cancelled. `progress` gets the bytes
/// read so far after each chunk and stops the work by returning false.
pub fn sha256(path: &Path, cancel: &AtomicBool, mut progress: impl FnMut(u64) -> bool) -> Result<Option<String>, String> {
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    // Shares read, write and delete (Rust's default): hashing never locks the file.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut handle = BCRYPT_HASH_HANDLE::default();
    check(unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut handle, None, None, 0) })?;
    let hash = Hash(handle);
    let mut buffer = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        check(unsafe { BCryptHashData(hash.0, &buffer[..n], 0) })?;
        done += n as u64;
        if !progress(done) {
            return Ok(None);
        }
    }
    let mut digest = [0u8; 32];
    check(unsafe { BCryptFinishHash(hash.0, &mut digest, 0) })?;
    Ok(Some(digest.iter().map(|b| format!("{b:02x}")).collect()))
}

/// The open properties dialog.
pub(super) struct Props {
    id: u64,
    pane: usize,
    path: PathBuf,
    kind: String,
    info: Option<Result<Info, String>>,
    hash: HashState,
    usage: UsageState,
    cancel: Arc<AtomicBool>,
}
enum HashState {
    /// Folders, or not read yet.
    None,
    /// The data is in the cloud: hashing would download it, so it waits for H.
    Waiting,
    Running(u64, u64),
    Done(String),
    Failed(String),
}

enum UsageState {
    /// Files, or not read yet.
    None,
    Running(Usage),
    Done(Usage),
    Failed(String),
}

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
/// `format_size` leaves small sizes without a unit (the list's size column).
fn bytes(n: u64) -> String {
    if n < 1000 { format!("{n} Byte") } else { listing::format_size(n) }
}
fn time(t: Option<SystemTime>) -> String {
    t.map_or_else(|| "—".into(), |t| chrono::DateTime::<chrono::Local>::from(t).format("%Y/%m/%d %H:%M:%S").to_string())
}
fn attribute_names(attributes: u32) -> String {
    let names: Vec<&str> = [
        (0x1, "読み取り専用"),
        (0x2, "隠し"),
        (0x4, "システム"),
        (0x20, "アーカイブ"),
        (0x100, "一時"),
        (0x400, "再解析ポイント"),
        (0x800, "圧縮"),
        (0x4000, "暗号化"),
        (REMOTE, "クラウド上"),
    ]
    .iter()
    .filter(|&&(bit, _)| attributes & bit != 0)
    .map(|&(_, name)| name)
    .collect();
    if names.is_empty() { "なし".into() } else { names.join("、") }
}
/// URLs and stream names come from whoever wrote the file: cut long ones short.
fn text(value: &str) -> String {
    const MAX: usize = 600;
    let value = listing::visible(value);
    match value.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &value[..at]),
        None => value.into_owned(),
    }
}
fn row(label: &str, value: impl Into<String>) -> PropRow {
    PropRow { label: label.into(), value: value.into().into(), ..PropRow::default() }
}
fn heading(label: &str) -> PropRow {
    PropRow { label: label.into(), heading: true, ..PropRow::default() }
}

impl Controller {
    /// Alt+Enter on one file or folder. Several items, a drive or "..": Windows' properties.
    pub(super) fn open_props(&mut self, p: usize) -> bool {
        let items = self.targets(p);
        let [path] = items.as_slice() else { return false };
        let data = self.data(p);
        let Some(entry) = data.rows.iter().find(|e| e.path.as_ref() == path) else { return false };
        self.close_props();
        self.props_opened += 1;
        let id = self.props_opened;
        let cancel = Arc::new(AtomicBool::new(false));
        self.props = Some(Props {
            id,
            pane: p,
            path: path.clone(),
            kind: entry.type_name.to_string(),
            info: None,
            hash: HashState::None,
            usage: UsageState::None,
            cancel: cancel.clone(),
        });
        let path = path.clone();
        std::thread::spawn(move || {
            let info = read(&path);
            let directory = matches!(&info, Ok(info) if info.directory);
            let start = match &info {
                Ok(info) if !info.directory && info.attributes & REMOTE == 0 => Some(info.size),
                _ => None,
            };
            if !post(Reply::Props(id, info)) {
                return;
            }
            if directory {
                usage_job(id, &path, &cancel);
            } else if let Some(total) = start {
                hash_job(id, &path, total, &cancel);
            }
        });
        if let Some(ui) = self.ui.upgrade() {
            let name = entry.path.file_name().map_or_else(|| shown(&entry.path), |n| listing::visible(&n.to_string_lossy()).into_owned());
            ui.set_props_name(name.into());
            ui.set_props_folder(entry.path.parent().map(shown).unwrap_or_default().into());
            ui.set_props_open(true);
            ui.invoke_focus_props();
        }
        self.props_rows();
        self.apply_opacity();
        true
    }
    pub(super) fn close_props(&mut self) {
        let Some(props) = self.props.take() else { return };
        props.cancel.store(true, Ordering::Relaxed);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_props_open(false);
            ui.invoke_focus_pane(props.pane as i32);
        }
        self.apply_opacity();
    }
    pub(super) fn props_info(&mut self, id: u64, info: Result<Info, String>) {
        let Some(props) = self.props.as_mut().filter(|p| p.id == id) else { return };
        props.hash = match &info {
            Ok(info) if info.directory => HashState::None,
            Ok(info) if info.attributes & REMOTE != 0 => HashState::Waiting,
            Ok(info) => HashState::Running(0, info.size),
            Err(_) => HashState::None,
        };
        if matches!(&info, Ok(info) if info.directory) {
            props.usage = UsageState::Running(Usage::default());
        }
        props.info = Some(info);
        self.props_rows();
    }
    pub(super) fn props_hash(&mut self, id: u64, hashing: Hashing) {
        let Some(props) = self.props.as_mut().filter(|p| p.id == id) else { return };
        props.hash = match hashing {
            Hashing::Progress(done, total) => HashState::Running(done, total),
            Hashing::Done(hex) => HashState::Done(hex),
            Hashing::Failed(e) => HashState::Failed(e),
        };
        self.props_rows();
    }
    pub(super) fn props_usage(&mut self, id: u64, measuring: Measuring) {
        let Some(props) = self.props.as_mut().filter(|p| p.id == id) else { return };
        props.usage = match measuring {
            Measuring::Progress(usage) => UsageState::Running(usage),
            Measuring::Done(usage) => UsageState::Done(usage),
            Measuring::Failed(e) => UsageState::Failed(e),
        };
        self.props_rows();
    }
    /// H: start the hash of a cloud file, or copy a finished one.
    pub(super) fn props_hash_key(&mut self) {
        let Some(props) = self.props.as_mut() else { return };
        match &props.hash {
            HashState::Waiting => {
                let size = props.info.as_ref().and_then(|i| i.as_ref().ok()).map_or(0, |i| i.size);
                props.hash = HashState::Running(0, size);
                let (id, path, cancel) = (props.id, props.path.clone(), props.cancel.clone());
                std::thread::spawn(move || hash_job(id, &path, size, &cancel));
                self.props_rows();
            }
            HashState::Done(hex) => {
                let hex = hex.clone();
                self.clipboard_text(&hex, "SHA-256 をコピーしました");
            }
            _ => {}
        }
    }
    /// C: everything shown, as "label<TAB>value" lines.
    pub(super) fn props_copy(&mut self) {
        let Some(props) = &self.props else { return };
        let mut lines = vec![format!("パス\t{}", display(&props.path))];
        for row in self.props_list() {
            if row.heading {
                lines.push(String::new());
                lines.push(row.label.to_string());
            } else {
                lines.push(format!("{}\t{}", row.label, row.value));
            }
        }
        self.clipboard_text(&lines.join("\r\n"), "プロパティをコピーしました");
    }
    /// W: Windows' own properties of the same item.
    pub(super) fn props_windows(&mut self) {
        let Some(props) = &self.props else { return };
        let (p, path) = (props.pane, props.path.clone());
        self.close_props();
        self.show_menu(p, ctxmenu::Target::Items(vec![path]), false, Some("properties"));
    }
    fn props_list(&self) -> Vec<PropRow> {
        let Some(props) = &self.props else { return Vec::new() };
        let mut rows = vec![row("種類", props.kind.clone())];
        let info = match &props.info {
            None => {
                rows.push(row("", "読み込み中…"));
                return rows;
            }
            Some(Err(e)) => {
                rows.push(row("エラー", e.clone()));
                return rows;
            }
            Some(Ok(info)) => info,
        };
        if !info.directory {
            rows.push(row("サイズ", bytes(info.size)));
        }
        match &props.usage {
            UsageState::None => {}
            UsageState::Running(u) => {
                let dim = |label: &str, value: String| PropRow { dim: true, ..row(label, value) };
                rows.push(dim("サイズ", format!("計算中… {}", bytes(u.size))));
                rows.push(dim("内容", format!("ファイル{}、フォルダー{}", grouped(u.files), grouped(u.folders))));
            }
            UsageState::Done(u) => {
                rows.push(row("サイズ", bytes(u.size)));
                rows.push(row("内容", format!("ファイル{}、フォルダー{}", grouped(u.files), grouped(u.folders))));
                let mut skipped = Vec::new();
                if u.links > 0 {
                    skipped.push(format!("リンク{}件", grouped(u.links)));
                }
                if u.unreadable > 0 {
                    skipped.push(format!("読めないフォルダー{}件", grouped(u.unreadable)));
                }
                if !skipped.is_empty() {
                    rows.push(row("集計外", skipped.join("、")));
                }
            }
            UsageState::Failed(e) => rows.push(PropRow { dim: true, ..row("サイズ", format!("計算できません: {e}")) }),
        }
        rows.push(row("更新日時", time(info.modified)));
        rows.push(row("作成日時", time(info.created)));
        rows.push(row("属性", attribute_names(info.attributes)));
        if let Some(link) = &info.link {
            rows.push(row("リンク先", shown(link)));
        }
        if let Some(version) = &info.version {
            rows.push(row("バージョン", version.clone()));
        }
        if let Some(zone) = &info.zone {
            rows.push(heading("ダウンロード情報"));
            rows.push(row("取得元", zone.host.as_deref().map_or_else(|| "記録なし".into(), text)));
            if let Some(referrer) = &zone.referrer {
                rows.push(row("参照元", text(referrer)));
            }
            rows.push(row("ゾーン", zone.id.map_or_else(|| "記録なし".into(), zone_name)));
        }
        rows.push(heading("詳細情報"));
        let sha = match &props.hash {
            HashState::None => None,
            HashState::Waiting => Some(("クラウド上のファイルです。H で計算（ダウンロードされます）".to_string(), false)),
            HashState::Running(done, total) => {
                let percent = if *total == 0 { 0 } else { done * 100 / total };
                Some((format!("計算中… {percent}%（{} / {}）", bytes(*done), bytes(*total)), false))
            }
            HashState::Done(hex) => Some((hex.clone(), true)),
            HashState::Failed(e) => Some((format!("計算できません: {e}"), false)),
        };
        if let Some((value, finished)) = sha {
            rows.push(PropRow { mono: finished, dim: !finished, ..row("SHA-256", value) });
        }
        let streams: Vec<String> = info.streams.iter().map(|(name, size)| format!("{}（{}）", text(name), bytes(*size))).collect();
        rows.push(row("ADS", if streams.is_empty() { "なし".into() } else { streams.join("、") }));
        rows
    }
    fn props_rows(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let hash = match self.props.as_ref().map(|p| &p.hash) {
            Some(HashState::Waiting) => 1,
            Some(HashState::Done(_)) => 2,
            _ => 0,
        };
        ui.set_props_hash(hash);
        ui.set_props_rows(ModelRc::new(slint::VecModel::from(self.props_list())));
    }
}

/// Reports progress at most every 100 ms; stops when cancelled or the app is gone.
fn hash_job(id: u64, path: &Path, total: u64, cancel: &AtomicBool) {
    let mut last = Instant::now();
    let result = sha256(path, cancel, |done| {
        if last.elapsed() < Duration::from_millis(100) {
            return true;
        }
        last = Instant::now();
        // The dialog is closed or replaced: the UI drops it, but there is no use going on.
        !cancel.load(Ordering::Relaxed) && post(Reply::Hash(id, Hashing::Progress(done, total)))
    });
    let hashing = match result {
        Ok(Some(hex)) => Hashing::Done(hex),
        Ok(None) => return,
        Err(e) => Hashing::Failed(e),
    };
    post(Reply::Hash(id, hashing));
}

/// Like `hash_job`, for a folder's size.
fn usage_job(id: u64, path: &Path, cancel: &AtomicBool) {
    let mut last = Instant::now();
    let result = usage(path, cancel, |usage| {
        if last.elapsed() < Duration::from_millis(100) {
            return true;
        }
        last = Instant::now();
        post(Reply::Usage(id, Measuring::Progress(*usage)))
    });
    let measuring = match result {
        Ok(Some(usage)) => Measuring::Done(usage),
        Ok(None) => return,
        Err(e) => Measuring::Failed(e),
    };
    post(Reply::Usage(id, measuring));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_version() {
        let notepad = PathBuf::from(std::env::var("SystemRoot").unwrap()).join(r"System32\notepad.exe");
        let found = read(&notepad).unwrap().version.unwrap();
        assert_eq!(found.split('.').filter(|n| n.parse::<u16>().is_ok()).count(), 4, "{found}");
        let text = std::env::temp_dir().join(format!("enyfile-version-{}.txt", std::process::id()));
        std::fs::write(&text, "no version here").unwrap();
        assert_eq!(read(&text).unwrap().version, None);
        let _ = std::fs::remove_file(&text);
    }
    #[test]
    fn zone_identifier() {
        let text = b"[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://example.com/download\r\nHostUrl=https://example.com/files/sample.zip\r\n";
        assert_eq!(
            parse_zone(text),
            Zone { id: Some(3), host: Some("https://example.com/files/sample.zip".into()), referrer: Some("https://example.com/download".into()) }
        );
        // UTF-16 with a BOM, keys in any case, other sections ignored.
        let wide: Vec<u8> = "\u{FEFF}[Other]\nHostUrl=x\n[zonetransfer]\nzoneid=4\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(parse_zone(&wide), Zone { id: Some(4), host: None, referrer: None });
        assert_eq!(parse_zone(b"garbage\0\xff"), Zone::default());
    }
    #[test]
    fn sha256_streams_and_cancel() {
        let dir = std::env::temp_dir().join(format!("enyfile-props-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.txt");
        std::fs::write(&file, "abc").unwrap();
        let never = AtomicBool::new(false);
        assert_eq!(
            sha256(&file, &never, |_| true).unwrap().as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        let empty = dir.join("empty");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(
            sha256(&empty, &never, |_| true).unwrap().as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(sha256(&file, &AtomicBool::new(true), |_| true).unwrap(), None);
        assert_eq!(sha256(&file, &never, |_| false).unwrap(), None);
        // A downloaded file: the browser's stream shows up in the streams and as the zone.
        let mut stream = file.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let zone = "[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.com/a.zip\r\n";
        std::fs::write(PathBuf::from(stream), zone).unwrap();
        let info = read(&file).unwrap();
        assert_eq!(info.size, 3);
        assert_eq!(info.streams, vec![("Zone.Identifier".to_string(), zone.len() as u64)]);
        assert_eq!(info.zone.unwrap().host.as_deref(), Some("https://example.com/a.zip"));
        assert!(read(&dir).unwrap().streams.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn folder_usage_skips_links() {
        let dir = std::env::temp_dir().join(format!("enyfile-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"a\b")).unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::write(dir.join("one.txt"), "12345").unwrap();
        std::fs::write(dir.join(r"a\b\two.bin"), vec![7u8; 100_000]).unwrap();
        // A junction back to the top folder: entering it would loop.
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(dir.join(r"a\loop")).arg(&dir).output().unwrap();
        assert!(made.status.success());
        let never = AtomicBool::new(false);
        let found = usage(&dir, &never, |_| true).unwrap().unwrap();
        assert_eq!((found.size, found.files, found.folders, found.links, found.unreadable), (100_005, 2, 3, 1, 0));
        assert_eq!(usage(&dir, &AtomicBool::new(true), |_| true).unwrap(), None);
        assert_eq!(usage(&dir, &never, |_| false).unwrap(), None);
        assert!(usage(&dir.join("missing"), &never, |_| true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn formatting() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(26004123), "26,004,123");
        assert_eq!(attribute_names(0x21), "読み取り専用、アーカイブ");
        assert_eq!(attribute_names(0), "なし");
        assert_eq!(text("a\u{202E}b"), "a[RLO]b");
        assert_eq!(text(&"x".repeat(700)).chars().count(), 601);
    }
}
