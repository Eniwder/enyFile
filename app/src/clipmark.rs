//! Clipboard membership for list decorations. No file handles or directory reads.
use std::{collections::HashSet, os::windows::ffi::OsStrExt, path::{Path, PathBuf}, sync::{Arc, mpsc}};

#[derive(Default)]
pub struct Files {
    paths: HashSet<Vec<u16>>,
    cut: bool,
}
impl Files {
    pub fn new(paths: &[PathBuf], cut: bool) -> Self {
        Self { paths: paths.iter().map(|path| key(path)).collect(), cut }
    }
    /// 0 = not on clipboard, 1 = copy, 2 = cut.
    pub fn state(&self, path: &Path) -> i32 {
        if self.paths.is_empty() || !self.paths.contains(&key(path)) { 0 } else if self.cut { 2 } else { 1 }
    }
}
fn key(path: &Path) -> Vec<u16> {
    let path = crate::shell::shell_path(path);
    let wide: Vec<_> = path.as_os_str().encode_wide().collect();
    let mut key = Vec::with_capacity(wide.len());
    for unit in char::decode_utf16(wide) {
        match unit {
            Ok('/') => key.push(b'\\' as u16),
            Ok(ch) => for lower in ch.to_lowercase() { key.extend(lower.encode_utf16(&mut [0; 2]).iter().copied()); },
            Err(error) => key.push(error.unpaired_surrogate()),
        }
    }
    while key.last() == Some(&(b'\\' as u16)) { key.pop(); }
    key
}

/// OLE may ask another application to render delayed data. Keep it off the UI
/// thread, coalesce requests, and discard results if the clipboard changed.
pub fn reader() -> mpsc::SyncSender<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let Ok(_ole) = crate::shell::Apartment::new() else { return };
        while rx.recv().is_ok() {
            let sequence = crate::shell::clipboard_sequence();
            let Ok((paths, cut)) = crate::shell::clipboard_file_marks() else { continue };
            if sequence == crate::shell::clipboard_sequence()
                && !crate::post(crate::Reply::ClipboardFiles(sequence, Arc::new(Files::new(&paths, cut)))) { return; }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn marks_exact_paths_across_windows_prefixes_and_case() {
        let paths = [PathBuf::from(r"C:\Folder\File.txt"), PathBuf::from(r"\\server\share\folder")];
        let cut = Files::new(&paths, true);
        assert_eq!(cut.state(Path::new(r"\\?\c:\folder\FILE.TXT")), 2);
        assert_eq!(cut.state(Path::new(r"\\?\UNC\SERVER\SHARE\FOLDER\")), 2);
        assert_eq!(cut.state(Path::new(r"C:\other\File.txt")), 0);
        assert_eq!(cut.state(Path::new(r"\\server\share\folder\child.txt")), 0);
        assert_eq!(Files::new(&paths, false).state(&paths[0]), 1);
        assert_eq!(Files::default().state(&paths[0]), 0);
    }
}
