//! Best-effort snapshots, never evidence that a listed process caused the failure.
use super::*;
use std::{sync::Mutex, thread};
use windows::{Win32::System::RestartManager::*, core::PWSTR};

pub(super) struct Watch {
    current: Arc<Mutex<Option<Arc<str>>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::Thread>,
}

impl Watch {
    pub(super) fn start() -> Self {
        let current = Arc::new(Mutex::new(None::<Arc<str>>));
        let stopped = Arc::new(AtomicBool::new(false));
        let operation = thread::current().id();
        let worker = thread::Builder::new()
            .name("delete-diagnostics".into())
            .spawn({
                let current = current.clone();
                let stopped = stopped.clone();
                move || {
                    let mut reported: Option<Arc<str>> = None;
                    while !stopped.load(Ordering::Acquire) {
                        let path = current.lock().unwrap().clone();
                        if let Some(path) = path
                            && !reported
                                .as_ref()
                                .is_some_and(|last| Arc::ptr_eq(last, &path))
                        {
                            let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                            if let Err(error) = probe(&wide)
                                && matches!(error.code().0 as u32, 0x80070020 | 0x80070021)
                            {
                                report(
                                    &path,
                                    &wide,
                                    "in-progress",
                                    operation,
                                    &format!("HRESULT=0x{:08X} {error}", error.code().0 as u32),
                                );
                                reported = Some(path);
                            }
                        }
                        thread::park_timeout(Duration::from_millis(50));
                    }
                }
            })
            .map(|handle| handle.thread().clone())
            .map_err(|error| {
                crate::recovery::record(format!(
                    "delete: live diagnostics unavailable; thread={operation:?} error={error}"
                ))
            })
            .ok();
        Self {
            current,
            stopped,
            worker,
        }
    }

    pub(super) fn track(&self, path: &str) {
        *self.current.lock().unwrap() = Some(Arc::from(path));
    }

    pub(super) fn clear(&self, path: &str) {
        let mut current = self.current.lock().unwrap();
        if current.as_deref() == Some(path) {
            *current = None;
        }
    }

    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.unpark();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}

fn probe(wide: &[u16]) -> windows::core::Result<()> {
    // OPEN_EXISTING without DELETE_ON_CLOSE: request access only, never delete data.
    // Share everything and close immediately so this probe cannot retain a lock.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }?;
    let _ = unsafe { CloseHandle(handle) };
    Ok(())
}

pub(super) fn inspect(path: &str, phase: &str, always: bool) {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let access = match probe(&wide) {
        Ok(()) if !always => return,
        Ok(()) => "allowed-now (does not guarantee deletion)".to_owned(),
        Err(error) => format!("HRESULT=0x{:08X} {error}", error.code().0 as u32),
    };
    report(path, &wide, phase, thread::current().id(), &access);
}

fn report(path: &str, wide: &[u16], phase: &str, operation: thread::ThreadId, access: &str) {
    crate::recovery::record(format!(
        "delete: access snapshot; thread={operation:?} phase={phase} path={path:?} delete_access={access}"
    ));
    // Restart Manager does not accept directories. Never follow a directory or link.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        other => {
            crate::recovery::record(format!(
                "delete: users snapshot unavailable; phase={phase} path={path:?} metadata={other:?}"
            ));
            return;
        }
    }
    let result = users(wide);
    crate::recovery::checkpoint(format!(
        "delete: users snapshot (not necessarily blockers); thread={operation:?} phase={phase} path={path:?} result={result:?}"
    ));
}

struct Session(u32);
impl Drop for Session {
    fn drop(&mut self) {
        let _ = unsafe { RmEndSession(self.0) };
    }
}

fn users(path: &[u16]) -> Result<Vec<String>, String> {
    let mut handle = 0;
    let mut key = [0u16; 33];
    unsafe { RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) }
        .ok()
        .map_err(|e| format!("RmStartSession: {e}"))?;
    let session = Session(handle);
    unsafe { RmRegisterResources(session.0, Some(&[PCWSTR(path.as_ptr())]), None, None) }
        .ok()
        .map_err(|e| format!("RmRegisterResources: {e}"))?;
    let mut entries = Vec::<RM_PROCESS_INFO>::new();
    for _ in 0..3 {
        let mut needed = 0;
        let mut count = entries.len() as u32;
        let mut reasons = 0;
        let status = unsafe {
            RmGetList(
                session.0,
                &mut needed,
                &mut count,
                if entries.is_empty() {
                    None
                } else {
                    Some(entries.as_mut_ptr())
                },
                &mut reasons,
            )
        };
        if status == ERROR_MORE_DATA {
            if needed > 4096 {
                return Err("too many resource users".into());
            }
            entries.resize(needed as usize, RM_PROCESS_INFO::default());
            continue;
        }
        status.ok().map_err(|e| format!("RmGetList: {e}"))?;
        return Ok(entries
            .iter()
            .take(count as usize)
            .map(|entry| {
                let text = |s: &[u16]| {
                    String::from_utf16_lossy(
                        &s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())],
                    )
                };
                format!(
                    "pid={} self={} app={:?} service={:?}",
                    entry.Process.dwProcessId,
                    entry.Process.dwProcessId == std::process::id(),
                    text(&entry.strAppName),
                    text(&entry.strServiceShortName)
                )
            })
            .collect());
    }
    Err("resource users changed during query".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn identifies_our_own_lock_and_releases_diagnostic_handles() {
        let path = std::env::temp_dir().join(format!(
            "enyfile-delete-diagnostic-{}.txt",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(&path)
            .unwrap();
        let path_text = path.to_str().unwrap();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let found = users(&wide).unwrap();
        assert!(
            found
                .iter()
                .any(|s| s.contains(&format!("pid={} self=true", std::process::id()))),
            "{found:?}"
        );
        inspect(path_text, "test-held", false);
        assert!(
            std::fs::remove_file(&path).is_err(),
            "diagnostics must not release the original lock"
        );
        drop(file);
        inspect(path_text, "test-released", true);
        std::fs::remove_file(&path).expect("diagnostic handles/session must be released");
    }
}
