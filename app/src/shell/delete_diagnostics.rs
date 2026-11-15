//! Best-effort snapshots, never evidence that a listed process caused the failure.
use super::*;
use windows::{core::PWSTR, Win32::System::RestartManager::*};

pub(super) fn inspect(path: &str, phase: &str, always: bool) {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // OPEN_EXISTING without DELETE_ON_CLOSE: request access only, never delete data.
    // Share everything and close immediately so this probe cannot retain a lock.
    let probe = unsafe {
        CreateFileW(PCWSTR(wide.as_ptr()), DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None,
            OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, None)
    };
    let access = match probe {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            if !always { return; }
            "allowed-now (does not guarantee deletion)".to_owned()
        }
        Err(error) => format!("HRESULT=0x{:08X} {error}", error.code().0 as u32),
    };
    crate::recovery::checkpoint(format!("delete: access snapshot; phase={phase} path={path:?} delete_access={access}"));
    // Restart Manager does not accept directories. Never follow a directory or link.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        other => {
            crate::recovery::record(format!("delete: users snapshot unavailable; phase={phase} path={path:?} metadata={other:?}"));
            return;
        }
    }
    let result = users(&wide);
    crate::recovery::checkpoint(format!("delete: users snapshot (not necessarily blockers); phase={phase} path={path:?} result={result:?}"));
}

struct Session(u32);
impl Drop for Session {
    fn drop(&mut self) { let _ = unsafe { RmEndSession(self.0) }; }
}

fn users(path: &[u16]) -> Result<Vec<String>, String> {
    let mut handle = 0;
    let mut key = [0u16; 33];
    unsafe { RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) }
        .ok().map_err(|e| format!("RmStartSession: {e}"))?;
    let session = Session(handle);
    unsafe { RmRegisterResources(session.0, Some(&[PCWSTR(path.as_ptr())]), None, None) }
        .ok().map_err(|e| format!("RmRegisterResources: {e}"))?;
    let mut entries = Vec::<RM_PROCESS_INFO>::new();
    for _ in 0..3 {
        let mut needed = 0;
        let mut count = entries.len() as u32;
        let mut reasons = 0;
        let status = unsafe { RmGetList(session.0, &mut needed, &mut count,
            if entries.is_empty() { None } else { Some(entries.as_mut_ptr()) }, &mut reasons) };
        if status == ERROR_MORE_DATA {
            if needed > 4096 { return Err("too many resource users".into()); }
            entries.resize(needed as usize, RM_PROCESS_INFO::default());
            continue;
        }
        status.ok().map_err(|e| format!("RmGetList: {e}"))?;
        return Ok(entries.iter().take(count as usize).map(|entry| {
            let text = |s: &[u16]| String::from_utf16_lossy(&s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())]);
            format!("pid={} self={} app={:?} service={:?}", entry.Process.dwProcessId,
                entry.Process.dwProcessId == std::process::id(), text(&entry.strAppName), text(&entry.strServiceShortName))
        }).collect());
    }
    Err("resource users changed during query".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn identifies_our_own_lock_and_releases_diagnostic_handles() {
        let path = std::env::temp_dir().join(format!("enyfile-delete-diagnostic-{}.txt", std::process::id()));
        let file = std::fs::OpenOptions::new().write(true).create_new(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0).open(&path).unwrap();
        let path_text = path.to_str().unwrap();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let found = users(&wide).unwrap();
        assert!(found.iter().any(|s| s.contains(&format!("pid={} self=true", std::process::id()))), "{found:?}");
        inspect(path_text, "test-held", false);
        assert!(std::fs::remove_file(&path).is_err(), "diagnostics must not release the original lock");
        drop(file);
        inspect(path_text, "test-released", true);
        std::fs::remove_file(&path).expect("diagnostic handles/session must be released");
    }
}
