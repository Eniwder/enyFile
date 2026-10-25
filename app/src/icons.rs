//! File icons as Windows shows them (usually the associated application's icon).
//! Pixels are produced on worker threads and shared by key; the UI thread turns each key
//! into a Slint image once.
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::{
    collections::HashMap,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Graphics::Gdi::*,
        Storage::FileSystem::*,
        System::Com::*,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{Interface, PCWSTR},
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
    let pixels = if stays_local(path) {
        shell_icon(&path.to_string_lossy(), FILE_FLAGS_AND_ATTRIBUTES(0), SHGFI_FLAGS(0))
    } else {
        let ext = path.extension().unwrap_or_default().to_string_lossy();
        shell_icon(&format!("x.{ext}"), FILE_ATTRIBUTE_NORMAL, SHGFI_USEFILEATTRIBUTES)
    };
    record(key, pixels);
}

/// A shortcut's icon comes from the place it names (icon file or target). Only local places
/// are followed: a remote one would make the Shell connect to a server (with the user's
/// credentials) just because a folder or search result was listed, or stall this worker until
/// a share times out. Anything unclear gets the generic icon of its kind.
fn stays_local(path: &Path) -> bool {
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
    let named = match ext.as_str() {
        "lnk" => link_places(path).filter(|places| !places.is_empty()),
        "url" => std::fs::read(path).ok().map(|bytes| url_places(&bytes)),
        _ => return true,
    };
    named.is_some_and(|places| places.iter().all(|p| local(p)))
}
/// The shortcut's icon file and target, as stored (not resolved: resolving searches).
fn link_places(path: &Path) -> Option<Vec<String>> {
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        link.cast::<IPersistFile>().ok()?.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;
        let text = |buffer: &[u16]| String::from_utf16_lossy(&buffer[..buffer.iter().position(|&c| c == 0).unwrap_or(0)]);
        let mut icon = [0u16; 1024];
        let mut index = 0;
        link.GetIconLocation(&mut icon, &mut index).ok()?;
        let mut target = [0u16; 1024];
        link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32).ok()?;
        Some([text(&icon), text(&target)].into_iter().filter(|p| !p.is_empty()).collect())
    }
}
/// An Internet shortcut's icon file and, for a file: URL, its target.
fn url_places(bytes: &[u8]) -> Vec<String> {
    let text = match bytes {
        [0xFF, 0xFE, rest @ ..] => String::from_utf16_lossy(&rest.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>()),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    };
    let mut places = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        if key == "iconfile" && !value.trim().is_empty() {
            places.push(value.trim().to_string());
        } else if key == "url" && value.trim().get(..5).is_some_and(|s| s.eq_ignore_ascii_case("file:")) {
            // file:///C:/x is local; file://server/share and file:////server are not.
            let rest = value.trim()[5..].trim_start_matches('/');
            let slashes = value.trim()[5..].len() - rest.len();
            places.push(if slashes == 3 { rest.replace('/', "\\") } else { format!(r"\\{rest}") });
        }
    }
    places
}
/// On a local drive (or relative to the shortcut, which is being listed anyway).
fn local(place: &str) -> bool {
    let place = expand(place);
    let place = place.strip_prefix(r"\\?\").unwrap_or(&place);
    if place.starts_with(r"\\") || place.starts_with("//") || place.get(..4).is_some_and(|s| s.eq_ignore_ascii_case(r"UNC\")) {
        return false;
    }
    let bytes = place.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return !place.contains('%');
    }
    let root: Vec<u16> = format!("{}:\\", bytes[0] as char).encode_utf16().chain(Some(0)).collect();
    // DRIVE_REMOVABLE, DRIVE_FIXED, DRIVE_CDROM, DRIVE_RAMDISK (not DRIVE_REMOTE).
    matches!(unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }, 2 | 3 | 5 | 6)
}
/// `%NAME%` as Windows expands it in icon locations; unknown names stay as they are.
fn expand(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        let name = &rest[start + 1..start + 1 + len];
        out.push_str(&rest[..start]);
        match std::env::var(name) {
            Ok(value) if !name.is_empty() => out.push_str(&value),
            _ => out.push_str(&rest[start..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    out + rest
}

fn shell_icon(path: &str, attributes: FILE_FLAGS_AND_ATTRIBUTES, extra: SHGFI_FLAGS) -> Option<Pixels> {
    let wide: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let mut info = SHFILEINFOW::default();
    // Until the process's system image list exists, lookups made at the same time from
    // several threads (panes listing at startup) succeed without an icon: one at a time
    // until the first icon comes, then in parallel.
    static READY: AtomicBool = AtomicBool::new(false);
    static FIRST: Mutex<()> = Mutex::new(());
    let _first = (!READY.load(Ordering::Acquire)).then(|| FIRST.lock().unwrap_or_else(|e| e.into_inner()));
    // Large icons (32 px, more with a larger Shell icon size), cut to 32 px, shown at 16
    // logical px: sharp at 100-200 % scaling.
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attributes,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | extra,
        )
    };
    if !info.hIcon.is_invalid() {
        READY.store(true, Ordering::Release);
    }
    drop(_first);
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let pixels = icon_pixels(info.hIcon);
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    pixels.map(|p| shrink(p, SIZE))
}

/// Icon pixels are kept at this size at most.
const SIZE: u32 = 32;
/// Area-averaged downscale to `to` px. The renderer's bilinear filter reads only 2x2 source
/// pixels, so drawing a 48 px icon at 16 px skips pixels and breaks thin lines into jaggies.
fn shrink(pixels: Pixels, to: u32) -> Pixels {
    let (w, h) = (pixels.width(), pixels.height());
    if w <= to && h <= to {
        return pixels;
    }
    let (tw, th) = ((w * to / w.max(h)).max(1), (h * to / w.max(h)).max(1));
    // Premultiplied, so transparent pixels do not darken the edges.
    let src: Vec<[f32; 4]> = pixels
        .as_slice()
        .iter()
        .map(|p| {
            let a = p.a as f32 / 255.0;
            [p.r as f32 * a, p.g as f32 * a, p.b as f32 * a, p.a as f32]
        })
        .collect();
    // Each output pixel covers `scale` source pixels per axis, partly at the ends.
    let weights = |from: u32, into: u32| -> Vec<Vec<(usize, f32)>> {
        let scale = from as f32 / into as f32;
        (0..into)
            .map(|d| {
                let (start, end) = (d as f32 * scale, (d + 1) as f32 * scale);
                (start.floor() as u32..(end.ceil() as u32).min(from))
                    .map(|s| (s as usize, ((s + 1) as f32).min(end) - (s as f32).max(start)))
                    .filter(|&(_, cover)| cover > 0.0)
                    .map(|(s, cover)| (s, cover / scale))
                    .collect()
            })
            .collect()
    };
    let (across, down) = (weights(w, tw), weights(h, th));
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for rows in &down {
        for cols in &across {
            let mut sum = [0f32; 4];
            for &(y, wy) in rows {
                for &(x, wx) in cols {
                    let p = src[y * w as usize + x];
                    for c in 0..4 {
                        sum[c] += p[c] * wx * wy;
                    }
                }
            }
            let a = sum[3];
            let color = |v: f32| if a > 0.0 { (v * 255.0 / a).round().clamp(0.0, 255.0) as u8 } else { 0 };
            out.extend([color(sum[0]), color(sum[1]), color(sum[2]), a.round().clamp(0.0, 255.0) as u8]);
        }
    }
    SharedPixelBuffer::clone_from_slice(&out, tw, th)
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
            // Icons are at most 256 px (a monochrome mask is twice as tall); anything else is
            // refused, which also keeps w * h * 4 far from overflowing.
            let size = 1..=1024;
            if !size.contains(&bm.bmWidth) || !size.contains(&bm.bmHeight.saturating_abs()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_places() {
        let windows = std::env::var("SystemRoot").unwrap();
        for (place, ok) in [
            (r"\\server\share\icon.ico", false),
            (r"\\?\UNC\server\share\x.exe", false),
            ("//server/share/x.exe", false),
            (r"%SystemRoot%\system32\shell32.dll", true),
            (r"%NO_SUCH_VARIABLE_ENYFILE%\x.ico", false),
            (&format!(r"{windows}\notepad.exe"), true),
            (&format!(r"\\?\{windows}\notepad.exe"), true),
            ("icon.ico", true),
            // Byte 4 falls inside a character: must not be sliced there.
            ("あいう.ico", true),
            (r"\\?\あいう.ico", true),
        ] {
            assert_eq!(local(place), ok, "{place}");
        }
        assert_eq!(expand("%SystemRoot%-%%-%x"), format!("{windows}-%%-%x"));
    }
    /// Audit #9: the size check still lets real icons through.
    #[test]
    fn type_icons_still_load() {
        let _com = crate::shell::Apartment::new().unwrap();
        for (probe, attributes) in [("x.txt", FILE_ATTRIBUTE_NORMAL), ("folder", FILE_ATTRIBUTE_DIRECTORY)] {
            let pixels = shell_icon(probe, attributes, SHGFI_USEFILEATTRIBUTES).unwrap();
            assert!((16..=256).contains(&pixels.width()) && pixels.width() == pixels.height(), "{probe}");
        }
    }
    #[test]
    fn large_icons_are_area_averaged() {
        let px = |r: u8, a: u8| [r, 0, 0, a];
        let buffer = |pixels: &[[u8; 4]], w, h| SharedPixelBuffer::clone_from_slice(pixels.as_flattened(), w, h);
        // One-pixel stripes of white and black: every output pixel is their mix, none skipped.
        let stripes: Vec<_> = (0..48 * 48).map(|i| if i % 2 == 0 { px(255, 255) } else { px(0, 255) }).collect();
        let out = shrink(buffer(&stripes, 48, 48), 32);
        assert_eq!((out.width(), out.height()), (32, 32));
        // Each output pixel covers 1.5 stripes: 1/3 or 2/3 white.
        assert!(out.as_slice().iter().all(|p| [85, 170].contains(&p.r) && p.a == 255));
        // A transparent neighbor makes the edge fainter, not darker.
        let edge: Vec<_> = (0..64 * 64).map(|i| if i % 2 == 0 { px(255, 255) } else { px(0, 0) }).collect();
        let out = shrink(buffer(&edge, 64, 64), 32);
        assert!(out.as_slice().iter().all(|p| p.r == 255 && p.a == 128));
        let small = buffer(&stripes[..16 * 16], 16, 16);
        assert_eq!(shrink(small, 32).width(), 16);
    }
    #[test]
    fn url_shortcut_places() {
        let url = b"[InternetShortcut]\r\nURL=https://example.com/\r\nIconIndex=0\r\nIconFile=\\\\server\\share\\i.ico\r\n";
        assert_eq!(url_places(url), [r"\\server\share\i.ico"]);
        assert_eq!(url_places(b"[InternetShortcut]\nURL=file:///C:/Users/x.txt\n"), [r"C:\Users\x.txt"]);
        assert_eq!(url_places(b"[InternetShortcut]\nURL=file://server/share/x\n"), [r"\\server/share/x"]);
        assert!(url_places(b"[InternetShortcut]\nURL=https://example.com/\n").is_empty());
    }
    /// Audit #7: listing a shortcut to a share must not make the Shell connect to it.
    #[test]
    fn shortcuts_to_shares_get_the_generic_icon() {
        let _com = crate::shell::Apartment::new().unwrap();
        let dir = std::env::temp_dir().join(format!("enyfile-icons-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let windows = std::env::var("SystemRoot").unwrap();
        let shortcut = |name: &str, target: &str, icon: &str| {
            let path = dir.join(name);
            unsafe {
                let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
                let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
                let target = wide(target);
                link.SetPath(PCWSTR(target.as_ptr())).unwrap();
                if !icon.is_empty() {
                    let icon = wide(icon);
                    link.SetIconLocation(PCWSTR(icon.as_ptr()), 0).unwrap();
                }
                let file = path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
                link.cast::<IPersistFile>().unwrap().Save(PCWSTR(file.as_ptr()), true).unwrap();
            }
            path
        };
        let notepad = format!(r"{windows}\notepad.exe");
        assert!(stays_local(&shortcut("local.lnk", &notepad, "")));
        assert!(!stays_local(&shortcut("share.lnk", r"\\server\share\x.exe", "")));
        assert!(!stays_local(&shortcut("icon.lnk", &notepad, r"\\server\share\i.ico")));
        std::fs::write(dir.join("share.url"), "[InternetShortcut]\r\nURL=https://example.com/\r\nIconFile=\\\\server\\share\\i.ico\r\n").unwrap();
        assert!(!stays_local(&dir.join("share.url")));
        std::fs::write(dir.join("web.url"), "[InternetShortcut]\r\nURL=https://example.com/\r\n").unwrap();
        assert!(stays_local(&dir.join("web.url")));
        assert!(stays_local(&dir.join("other.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
