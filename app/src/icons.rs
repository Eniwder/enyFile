//! File icons as Windows shows them (usually the associated application's icon).
//! Pixels are produced on worker threads and shared by key; the UI thread turns each key
//! into a Slint image once.
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::{
    collections::HashMap,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Graphics::Gdi::*,
        Storage::FileSystem::*,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::PCWSTR,
};

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

/// Icon pixels by key, with the failed lookups so far and when the last one was made.
/// A lookup can fail while the Shell is busy; a cached failure would leave a generic icon
/// for good, so failures are retried, spaced out so that one folder full of the same kind
/// (hundreds of .mp4) does not use up every attempt at once.
type Lookup = (Option<Pixels>, u8, Instant);
fn store() -> &'static Mutex<HashMap<Arc<str>, Lookup>> {
    static STORE: OnceLock<Mutex<HashMap<Arc<str>, Lookup>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}
const ATTEMPTS: u8 = 8;
pub const RETRY: Duration = Duration::from_millis(700);
pub fn get(key: &str) -> Option<Pixels> {
    store().lock().unwrap().get(key).and_then(|(pixels, _, _)| pixels.clone())
}
/// True when `key` has pixels, failed too often, or failed a moment ago.
fn settled(key: &str) -> bool {
    store()
        .lock()
        .unwrap()
        .get(key)
        .is_some_and(|(pixels, failures, last)| pixels.is_some() || *failures >= ATTEMPTS || last.elapsed() < RETRY)
}
fn record(key: &Arc<str>, pixels: Option<Pixels>) {
    let mut store = store().lock().unwrap();
    let failures = store.get(key).map_or(0, |(_, f, _)| *f);
    let failures = if pixels.is_some() { failures } else { failures + 1 };
    store.insert(key.clone(), (pixels, failures, Instant::now()));
}

/// Files whose icon is stored in the file itself: these need a per-file lookup.
pub fn per_file(ext: &str) -> bool {
    matches!(ext.to_lowercase().as_str(), "exe" | "lnk" | "ico" | "url" | "cur" | "ani" | "scr")
}
/// Icon shared by every item of a kind: one per extension, one for folders.
pub fn type_key(ext: &str, directory: bool) -> Arc<str> {
    if directory {
        "<folder>".into()
    } else {
        format!(".{}", ext.to_lowercase()).into()
    }
}
/// Looks up and caches the type icon (by extension only, no disk access). Worker threads only.
pub fn ensure_type(key: &Arc<str>, directory: bool) {
    if settled(key) {
        return;
    }
    let probe = if directory { "folder".to_string() } else { format!("x{key}") };
    let attributes = if directory { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    record(key, shell_icon(&probe, attributes, SHGFI_USEFILEATTRIBUTES));
}
/// Looks up and caches the icon of one file or drive (reads the file). Worker threads only.
pub fn ensure_path(key: &Arc<str>, path: &Path) {
    if settled(key) {
        return;
    }
    record(key, shell_icon(&path.to_string_lossy(), FILE_FLAGS_AND_ATTRIBUTES(0), SHGFI_FLAGS(0)));
}

fn shell_icon(path: &str, attributes: FILE_FLAGS_AND_ATTRIBUTES, extra: SHGFI_FLAGS) -> Option<Pixels> {
    let wide: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let mut info = SHFILEINFOW::default();
    // 32 px icons, shown at 16 logical px: sharp at 100-200 % scaling.
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | extra,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let pixels = icon_pixels(info.hIcon);
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    pixels
}

/// HICON -> RGBA. Uses the alpha channel when present, otherwise the AND mask.
fn icon_pixels(icon: HICON) -> Option<Pixels> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        let read = |bitmap: HBITMAP| -> Option<(u32, u32, Vec<u8>)> {
            if bitmap.is_invalid() {
                return None;
            }
            let mut bm = BITMAP::default();
            if GetObjectW(HGDIOBJ(bitmap.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
                return None;
            }
            let (w, h) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32); // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut buffer = vec![0u8; (w * h * 4) as usize];
            let dc = CreateCompatibleDC(None);
            let lines = GetDIBits(dc, bitmap, 0, h, Some(buffer.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);
            let _ = DeleteDC(dc);
            (lines != 0).then_some((w, h, buffer))
        };
        let color = read(info.hbmColor);
        let mask = read(info.hbmMask);
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        let (w, h, mut bgra) = color?;
        let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
        for (i, p) in bgra.chunks_exact_mut(4).enumerate() {
            p.swap(0, 2); // BGRA -> RGBA
            if !has_alpha {
                // Old-style icon: a white AND-mask pixel means transparent.
                let transparent = mask.as_ref().is_some_and(|(_, _, m)| m.get(i * 4).is_some_and(|&v| v != 0));
                p[3] = if transparent { 0 } else { 255 };
            }
        }
        Some(SharedPixelBuffer::clone_from_slice(&bgra, w, h))
    }
}
