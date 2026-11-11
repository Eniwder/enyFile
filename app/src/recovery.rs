//! A background observer: a blocked UI cannot write its own diagnostic log.
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, atomic::{AtomicBool, Ordering}, mpsc::{self, SyncSender}},
    time::{Duration, Instant},
};

enum Event {
    Heartbeat(String),
    Note(String),
}
static EVENTS: OnceLock<SyncSender<Event>> = OnceLock::new();

/// Observe native events independently of application callbacks. A queued Slint
/// redraw can suppress later requests until a frame is delivered.
#[derive(Default)]
pub struct Activity {
    waiting: Option<Instant>,
    inputs: u64,
    frames: u64,
}
impl Activity {
    pub fn input(&mut self, now: Instant) {
        self.inputs += 1;
        self.waiting.get_or_insert(now);
    }
    pub fn frame(&mut self) {
        self.frames += 1;
        self.waiting = None;
    }
    pub fn retry_redraw(&mut self, now: Instant) -> bool {
        if self.waiting.is_some_and(|since| now.duration_since(since) >= Duration::from_secs(2)) {
            self.waiting = Some(now);
            true
        } else {
            false
        }
    }
    pub fn status(&self) -> String {
        format!("native-inputs={} redraw-events={} awaiting-frame={}", self.inputs, self.frames, self.waiting.is_some())
    }
}

pub struct Monitor(Arc<AtomicBool>);
impl Drop for Monitor {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
pub fn record(note: String) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.try_send(Event::Note(note));
    }
}
pub fn heartbeat(context: String) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.try_send(Event::Heartbeat(context));
    }
}
pub fn start(path: PathBuf) -> Monitor {
    let (tx, rx) = mpsc::sync_channel(8);
    let _ = EVENTS.set(tx);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    std::thread::spawn(move || {
        let mut context = String::from("UI heartbeat not received yet");
        let mut stalled = false;
        let mut last_status = Instant::now();
        append(&path, "UI monitor started");
        while !stopped.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_secs(15)) {
                Ok(Event::Heartbeat(next)) => {
                    if stalled {
                        append(&path, "UI heartbeat resumed");
                        stalled = false;
                    }
                    context = next;
                    if last_status.elapsed() >= Duration::from_secs(60) {
                        append(&path, &format!("UI heartbeat OK; {context}"));
                        last_status = Instant::now();
                    }
                }
                Ok(Event::Note(note)) => append(&path, &note),
                Err(mpsc::RecvTimeoutError::Timeout) if !stalled => {
                    // Sleep also pauses the heartbeat. This is evidence of a gap, not
                    // proof that the UI deadlocked, and is labelled accordingly.
                    append(&path, &format!("UI heartbeat missing for 15s (UI stall or system sleep); last state: {context}"));
                    stalled = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    Monitor(stop)
}
fn append(path: &Path, note: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 256 * 1024) {
        // Bound disk usage; only this observer writes these files.
        let _ = std::fs::rename(path, path.with_extension("previous.log"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "[{}] enyFile {}: {note}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), env!("CARGO_PKG_VERSION"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_redraw_retries_without_input_extending_the_deadline() {
        let now = Instant::now();
        let mut activity = Activity::default();
        assert!(!activity.retry_redraw(now + Duration::from_secs(3)));
        activity.input(now);
        activity.input(now + Duration::from_secs(1));
        assert!(!activity.retry_redraw(now + Duration::from_secs(1)));
        assert!(activity.retry_redraw(now + Duration::from_secs(2)));
        assert!(!activity.retry_redraw(now + Duration::from_secs(3)));
        assert!(activity.retry_redraw(now + Duration::from_secs(4)));
        activity.frame();
        assert!(!activity.retry_redraw(now + Duration::from_secs(10)));
    }
}
