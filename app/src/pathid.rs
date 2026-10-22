//! Whether two paths are the same file system object, or one lies inside the other.
//! Path text cannot tell: case, 8.3 short names, `\\?\` prefixes, hard links and
//! junctions all give one object several names. The object behind a path is identified by
//! its volume serial number and file ID (links followed), with the normalized text only
//! as a fallback where no ID can be read. Destructive steps check again right before they
//! run; a change in the moment between that check and the Shell acting remains possible.
use std::{
    collections::HashMap,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{Foundation::CloseHandle, Storage::FileSystem::*},
    core::PCWSTR,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ident {
    volume: u64,
    file: u128,
}

/// The object `path` names now (junctions and symbolic links followed), if it exists and
/// can be opened. Opens with no access rights, so locked and read-only items work too.
pub fn ident(path: &Path) -> Option<Ident> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let handle = CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            // Needed to open folders.
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .ok()?;
        let mut info = FILE_ID_INFO::default();
        let by_id = GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        );
        let ident = if by_id.is_ok() {
            Some(Ident { volume: info.VolumeSerialNumber, file: u128::from_le_bytes(info.FileId.Identifier) })
        } else {
            // File systems without 128-bit IDs (FAT): the 64-bit index.
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            GetFileInformationByHandle(handle, &mut info).ok().map(|_| Ident {
                volume: info.dwVolumeSerialNumber as u64,
                file: ((info.nFileIndexHigh as u128) << 32) | info.nFileIndexLow as u128,
            })
        };
        let _ = CloseHandle(handle);
        // Some network file systems report no ID at all: not usable for comparing.
        ident.filter(|i| i.file != 0)
    }
}

/// Text form for the fallback: no `\\?\` prefix, no trailing separator, lower case.
fn text(path: &Path) -> String {
    let plain = crate::shell::shell_path(path);
    plain.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

/// Identities looked up once per check (a merge asks about many paths in one folder).
#[derive(Default)]
pub struct Idents(HashMap<PathBuf, Option<Ident>>);
impl Idents {
    fn get(&mut self, path: &Path) -> Option<Ident> {
        *self.0.entry(path.to_path_buf()).or_insert_with(|| ident(path))
    }
    /// `a` and `b` are the same object (hard links included).
    pub fn same(&mut self, a: &Path, b: &Path) -> bool {
        match (self.get(a), self.get(b)) {
            (Some(x), Some(y)) => x == y,
            _ => text(a) == text(b),
        }
    }
    /// `inner` is `outer` or somewhere below it. `inner` need not exist yet: its nearest
    /// existing ancestors are compared. Each ancestor is opened as named, so a junction on
    /// the way counts as the folder it points to. A junction pointing further down (J to
    /// `outer\sub`) is not `outer` itself, so the ancestors of the resolved path count too.
    pub fn within(&mut self, inner: &Path, outer: &Path) -> bool {
        let Some(target) = self.get(outer) else {
            let (inner, outer) = (text(inner), text(outer));
            return inner == outer || inner.starts_with(&format!("{outer}\\"));
        };
        let mut resolved = None;
        let mut path = Some(inner);
        while let Some(p) = path {
            match self.get(p) {
                Some(id) if id == target => return true,
                Some(_) if resolved.is_none() => resolved = std::fs::canonicalize(p).ok(),
                _ => {}
            }
            path = p.parent();
        }
        let mut path = resolved.as_deref();
        while let Some(p) = path {
            if self.get(p) == Some(target) {
                return true;
            }
            path = p.parent();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::process::Command::new("cmd").args(["/c", "rmdir", "/s", "/q"]).arg(&self.0).output();
        }
    }
    #[test]
    fn identity_beyond_path_text() {
        let root = std::env::temp_dir().join(format!("enyfile-pathid-{}", std::process::id()));
        let f = Fixture(root.clone());
        std::fs::create_dir_all(root.join(r"Real\Sub")).unwrap();
        std::fs::write(root.join(r"Real\a.txt"), "a").unwrap();
        std::fs::hard_link(root.join(r"Real\a.txt"), root.join("link.txt")).unwrap();
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(root.join("J")).arg(root.join("Real")).output().unwrap();
        assert!(made.status.success());
        let canonical = std::fs::canonicalize(&root).unwrap();
        let mut ids = Idents::default();
        // Case and the \\?\ prefix.
        assert!(ids.same(&root.join(r"REAL\A.TXT"), &canonical.join(r"Real\a.txt")));
        // A hard link is the same file under another name.
        assert!(ids.same(&root.join("link.txt"), &root.join(r"Real\a.txt")));
        // Through a junction: the same folder, and inside it.
        assert!(ids.same(&root.join("J"), &root.join("Real")));
        assert!(ids.within(&root.join(r"J\Sub"), &root.join("Real")));
        // A junction to a folder inside: JS is Real\Sub, so it lies within Real.
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(root.join("JS")).arg(root.join(r"Real\Sub")).output().unwrap();
        assert!(made.status.success());
        assert!(ids.within(&root.join("JS"), &root.join("Real")));
        assert!(ids.within(&root.join(r"JS\new\deeper"), &root.join("Real")));
        assert!(!ids.within(&root.join("Real"), &root.join("JS")));
        // Not yet existing paths count by their existing ancestors.
        assert!(ids.within(&root.join(r"Real\Sub\new\deeper"), &root.join("Real")));
        assert!(!ids.within(&root.join("Realm"), &root.join("Real")));
        assert!(!ids.within(&root.join("Real"), &root.join(r"Real\Sub")));
        drop(f);
    }
}
