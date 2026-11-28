//! Whether two paths are the same file system object, or one lies inside the other.
//! Path text cannot tell: case, 8.3 short names, `\\?\` prefixes, hard links and
//! junctions all give one object several names. The object behind a path is identified by
//! its volume serial number and file ID (links followed). Missing evidence is reported
//! separately from a negative result. Destructive steps check again right before they
//! run; a change in the moment between that check and the Shell acting remains possible.
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::ErrorKind,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
};
use windows::Win32::{Foundation::HANDLE, Storage::FileSystem::*};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ident {
    volume: u64,
    file: u128,
}

/// The object `path` names now (junctions and symbolic links followed), if it exists and
/// can be opened. Opens with no access rights, so locked and read-only items work too.
pub fn ident(path: &Path) -> Option<Ident> {
    // std applies Windows long-path handling here. Zero access still permits file ID
    // queries, and backup semantics allows the same open for directories.
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .ok()?;
    let handle = HANDLE(file.as_raw_handle());
    // SAFETY: file owns the handle for both calls and each buffer has the exact API layout.
    unsafe {
        let mut info = FILE_ID_INFO::default();
        let by_id = GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        );
        let ident = if by_id.is_ok() {
            Some(Ident {
                volume: info.VolumeSerialNumber,
                file: u128::from_le_bytes(info.FileId.Identifier),
            })
        } else {
            // File systems without 128-bit IDs (FAT): the 64-bit index.
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            GetFileInformationByHandle(handle, &mut info)
                .ok()
                .map(|_| Ident {
                    volume: info.dwVolumeSerialNumber as u64,
                    file: ((info.nFileIndexHigh as u128) << 32) | info.nFileIndexLow as u128,
                })
        };
        // Some network file systems report no ID at all: not usable for comparing.
        ident.filter(|i| i.file != 0)
    }
}

/// The volume `path` lies on (links followed), if it can be opened.
pub fn volume(path: &Path) -> Option<u64> {
    ident(path).map(|i| i.volume)
}

/// Identities looked up once per check (a merge asks about many paths in one folder).
#[derive(Default)]
pub struct Idents(HashMap<PathBuf, Option<Ident>>);
impl Idents {
    pub fn get(&mut self, path: &Path) -> Option<Ident> {
        *self
            .0
            .entry(path.to_path_buf())
            .or_insert_with(|| ident(path))
    }
    /// None means one or both objects could not be identified.
    pub fn same(&mut self, a: &Path, b: &Path) -> Option<bool> {
        Some(self.get(a)? == self.get(b)?)
    }

    /// Check both named and resolved ancestors. A missing suffix can be checked by
    /// its nearest existing ancestor, but an unreadable ancestor makes a negative
    /// result inconclusive.
    pub fn within(&mut self, inner: &Path, outer: &Path) -> Option<bool> {
        let target = self.get(outer)?;
        let mut resolved = None;
        let mut complete = true;
        for path in inner.ancestors().filter(|p| p.has_root()) {
            if resolved.is_none() {
                match fs::canonicalize(path) {
                    Ok(path) => resolved = Some(path),
                    Err(e)
                        if e.kind() == ErrorKind::NotFound
                            && matches!(fs::symlink_metadata(path), Err(e) if e.kind() == ErrorKind::NotFound) =>
                    {
                        continue;
                    }
                    Err(_) => complete = false,
                }
            }
            match self.get(path) {
                Some(id) if id == target => return Some(true),
                Some(_) => {}
                None => complete = false,
            }
        }
        for path in resolved?.ancestors().filter(|p| p.has_root()) {
            match self.get(path) {
                Some(id) if id == target => return Some(true),
                Some(_) => {}
                None => complete = false,
            }
        }
        complete.then_some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn identity_beyond_path_text() {
        let root = std::env::temp_dir().join(format!("enyfile-pathid-{}", std::process::id()));
        let f = Fixture(root.clone());
        std::fs::create_dir_all(root.join(r"Real\Sub")).unwrap();
        std::fs::write(root.join(r"Real\a.txt"), "a").unwrap();
        std::fs::hard_link(root.join(r"Real\a.txt"), root.join("link.txt")).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.join("J"))
            .arg(root.join("Real"))
            .output()
            .unwrap();
        assert!(made.status.success());
        let canonical = std::fs::canonicalize(&root).unwrap();
        let mut ids = Idents::default();
        // Case and the \\?\ prefix.
        assert_eq!(
            ids.same(&root.join(r"REAL\A.TXT"), &canonical.join(r"Real\a.txt")),
            Some(true)
        );
        // A hard link is the same file under another name.
        assert_eq!(
            ids.same(&root.join("link.txt"), &root.join(r"Real\a.txt")),
            Some(true)
        );
        // Through a junction: the same folder, and inside it.
        assert_eq!(ids.same(&root.join("J"), &root.join("Real")), Some(true));
        assert_eq!(
            ids.within(&root.join(r"J\Sub"), &root.join("Real")),
            Some(true)
        );
        // A junction to a folder inside: JS is Real\Sub, so it lies within Real.
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.join("JS"))
            .arg(root.join(r"Real\Sub"))
            .output()
            .unwrap();
        assert!(made.status.success());
        assert_eq!(ids.within(&root.join("JS"), &root.join("Real")), Some(true));
        assert_eq!(
            ids.within(&root.join(r"JS\new\deeper"), &root.join("Real")),
            Some(true)
        );
        assert_eq!(
            ids.within(&root.join("Real"), &root.join("JS")),
            Some(false)
        );
        // Not yet existing paths count by their existing ancestors.
        assert_eq!(
            ids.within(&root.join(r"Real\Sub\new\deeper"), &root.join("Real")),
            Some(true)
        );
        assert_eq!(
            ids.within(&root.join("Realm"), &root.join("Real")),
            Some(false)
        );
        assert_eq!(
            ids.within(&root.join("Real"), &root.join(r"Real\Sub")),
            Some(false)
        );
        assert_eq!(ids.same(&root, &root.join("missing")), None);
        assert_eq!(ids.within(&root, &root.join("missing")), None);
        let long = root.join(["long-path-segment-1234567890"; 10].join("\\"));
        fs::create_dir_all(&long).unwrap();
        let extended = fs::canonicalize(&long).unwrap();
        assert_eq!(ids.same(&long, &extended), Some(true));
        assert_eq!(ids.within(&long, &canonical), Some(true));
        drop(f);
    }
}
