//! User settings in a JSON file. Unknown keys are kept when saving, so the file can grow
//! (themes, transparency, ...) without losing anything.
//!
//! ```json
//! {
//!   "places": { "desktop": "D:\\Desk", "pictures": "...", "videos": "...", "music": "..." },
//!   "bookmarks": [ { "name": "Work", "path": "D:\\Work" } ]
//! }
//! ```
//! A missing place falls back to the Windows known folder (including OneDrive redirection).
use crate::theme;
use serde_json::{Value, json};
use std::path::PathBuf;

pub const PLACES: [&str; 5] = ["desktop", "documents", "pictures", "videos", "music"];
/// Optional list columns, in `Config::columns` order.
pub const COLUMNS: [&str; 5] = ["size", "date", "time", "attr", "kind"];

/// Last window placement in physical pixels: outer position, inner size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub name: String,
    pub path: PathBuf,
}
pub struct Config {
    file: PathBuf,
    raw: Value,
    /// Places set by the user; None means the Windows known folder.
    pub places: [Option<PathBuf>; 5],
    pub bookmarks: Vec<Bookmark>,
    /// Left pane share of the width (0.15..=0.85).
    pub split: f32,
    /// Splitter positions as fractions: 3 side by side (2 boundaries), 3 stacked (2), 4 panes (x, y).
    pub split3_columns: [f32; 2],
    pub split3_rows: [f32; 2],
    pub split4: [f32; 2],
    /// Alt+Z: list hidden files too, per pane.
    pub show_hidden: [bool; 4],
    /// Panes shown: 2 (side by side), 3 (stacked) or 4 (2x2).
    pub panes: usize,
    /// 3-pane layout: three tall panes side by side instead of stacked wide ones.
    pub three_columns: bool,
    /// View > 属性の表示: the A R H S column.
    /// View > 列: size, date, time, attributes, type.
    pub columns: [bool; 5],
    /// Folders the panes open at startup; None = the sandbox (development) or Desktop / Documents.
    pub start: [Option<PathBuf>; 2],
    /// Window opacity, 0.3..=1.0.
    pub opacity: f32,
    /// Base colors in `theme::BASE` order.
    pub colors: Vec<slint::Color>,
    /// Extension rules: ("ZIP;RAR", color), first match wins.
    pub extension_colors: Vec<(String, slint::Color)>,
    pub font_family: String,
    pub font_size: f32,
    /// Recursive search enters junctions and symbolic links to folders (off: cycles and
    /// unexpectedly huge trees such as a link to a drive root are left alone).
    pub search_links: bool,
    /// Where the window was when it was last closed; None = Slint's default placement.
    pub window: Option<Placement>,
}
fn default_extensions() -> Vec<(String, slint::Color)> {
    theme::EXTENSIONS
        .iter()
        .map(|(e, c)| (e.to_string(), theme::parse(c).unwrap()))
        .collect()
}

/// The Windows default for place `index` (desktop, documents, pictures, videos, music).
pub fn known_folder(index: usize) -> Option<PathBuf> {
    use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::*};
    let id = [FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Pictures, FOLDERID_Videos, FOLDERID_Music][index];
    unsafe {
        let path = SHGetKnownFolderPath(&id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let result = path.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(path.0 as *const _));
        result
    }
}

impl Config {
    /// A missing or unreadable file gives the defaults; it is created on the first save.
    pub fn load(file: PathBuf) -> Self {
        let raw = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        Self::from_value(file, raw)
    }
    fn from_value(file: PathBuf, raw: Value) -> Self {
        let places = std::array::from_fn(|i| {
            raw["places"][PLACES[i]]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
        });
        let split = raw["split"].as_f64().map_or(0.5, |v| v as f32).clamp(0.15, 0.85);
        let pair = |key: &str, default: [f32; 2]| -> [f32; 2] {
            let value = |i: usize| raw[key].get(i).and_then(Value::as_f64).map(|v| v as f32);
            match (value(0), value(1)) {
                (Some(a), Some(b)) if (0.05..=0.95).contains(&a) && (0.05..=0.95).contains(&b) => [a, b],
                _ => default,
            }
        };
        let thirds = pair("split3_columns", [1.0 / 3.0, 2.0 / 3.0]);
        let split3_columns = if thirds[0] < thirds[1] { thirds } else { [1.0 / 3.0, 2.0 / 3.0] };
        let thirds = pair("split3_rows", [1.0 / 3.0, 2.0 / 3.0]);
        let split3_rows = if thirds[0] < thirds[1] { thirds } else { [1.0 / 3.0, 2.0 / 3.0] };
        let split4 = pair("split4", [0.5, 0.5]);
        // Older files stored one flag for both panes.
        let show_hidden = match &raw["show_hidden"] {
            Value::Bool(b) => [*b; 4],
            Value::Array(a) => [0, 1, 2, 3].map(|i| a.get(i).and_then(Value::as_bool).unwrap_or(false)),
            _ => [false; 4],
        };
        let panes = raw["panes"].as_u64().map_or(2, |n| n as usize).clamp(2, 4);
        // "show_attributes" is the older single switch for the attribute column.
        let attributes = raw["show_attributes"].as_bool().unwrap_or(true);
        let columns = COLUMNS.map(|key| raw["columns"][key].as_bool().unwrap_or(if key == "attr" { attributes } else { true }));
        let three_columns = raw["three_columns"].as_bool().unwrap_or(false);
        let start = ["left", "right"].map(|side| {
            raw["start"][side].as_str().filter(|s| !s.trim().is_empty()).map(PathBuf::from)
        });
        let opacity = raw["opacity"].as_f64().map_or(1.0, |o| o as f32).clamp(0.3, 1.0);
        let bookmarks = raw["bookmarks"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|b| {
                        Some(Bookmark {
                            name: b["name"].as_str()?.to_string(),
                            path: PathBuf::from(b["path"].as_str()?),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let colors = theme::BASE
            .iter()
            .map(|(key, _, default)| {
                raw["colors"][*key]
                    .as_str()
                    .and_then(theme::parse)
                    .unwrap_or_else(|| theme::parse(default).unwrap())
            })
            .collect();
        let extension_colors = raw["ext_colors"]
            .as_array()
            .map(|rules| {
                rules
                    .iter()
                    .filter_map(|r| Some((r["exts"].as_str()?.to_string(), theme::parse(r["color"].as_str()?)?)))
                    .collect()
            })
            .unwrap_or_else(default_extensions);
        let font_family = raw["font"]["family"]
            .as_str()
            .filter(|f| !f.trim().is_empty())
            .unwrap_or(theme::DEFAULT_FONT)
            .to_string();
        let font_size = raw["font"]["size"].as_f64().map_or(theme::DEFAULT_SIZE, |s| s as f32).clamp(8.0, 32.0);
        let search_links = raw["search_links"].as_bool().unwrap_or(false);
        let w = &raw["window"];
        let int = |key: &str| w[key].as_i64().and_then(|v| i32::try_from(v).ok());
        let size = |key: &str| w[key].as_u64().and_then(|v| u32::try_from(v).ok()).filter(|&v| v >= 100);
        let window = match (int("x"), int("y"), size("width"), size("height")) {
            (Some(x), Some(y), Some(width), Some(height)) => {
                Some(Placement { x, y, width, height, maximized: w["maximized"].as_bool().unwrap_or(false) })
            }
            _ => None,
        };
        Self {
            file,
            raw,
            places,
            bookmarks,
            split,
            split3_columns,
            split3_rows,
            split4,
            show_hidden,
            panes,
            three_columns,
            columns,
            start,
            opacity,
            colors,
            extension_colors,
            font_family,
            font_size,
            search_links,
            window,
        }
    }
    pub fn default_colors() -> Vec<slint::Color> {
        theme::BASE.iter().map(|(_, _, d)| theme::parse(d).unwrap()).collect()
    }
    pub fn default_extension_colors() -> Vec<(String, slint::Color)> {
        default_extensions()
    }
    /// The folder place `index` opens: the user's setting, else the Windows default.
    pub fn place(&self, index: usize) -> Option<PathBuf> {
        self.places[index].clone().or_else(|| known_folder(index))
    }
    /// Unset places are not written, so they keep following the Windows default.
    pub fn save(&mut self) -> Result<(), String> {
        let mut places = serde_json::Map::new();
        for (key, place) in PLACES.iter().zip(&self.places) {
            if let Some(path) = place {
                places.insert(key.to_string(), json!(path.to_string_lossy()));
            }
        }
        self.raw["places"] = Value::Object(places);
        let round = |v: f32| (v as f64 * 1000.0).round() / 1000.0;
        self.raw["split"] = json!(round(self.split));
        self.raw["split3_columns"] = json!(self.split3_columns.map(round));
        self.raw["split3_rows"] = json!(self.split3_rows.map(round));
        self.raw["split4"] = json!(self.split4.map(round));
        self.raw["show_hidden"] = json!(self.show_hidden);
        self.raw["panes"] = json!(self.panes);
        self.raw["three_columns"] = json!(self.three_columns);
        self.raw.as_object_mut().map(|o| o.remove("show_attributes"));
        self.raw["columns"] = Value::Object(COLUMNS.iter().zip(self.columns).map(|(k, v)| (k.to_string(), json!(v))).collect());
        let mut start = serde_json::Map::new();
        for (side, path) in ["left", "right"].iter().zip(&self.start) {
            if let Some(path) = path {
                start.insert(side.to_string(), json!(path.to_string_lossy()));
            }
        }
        self.raw["start"] = Value::Object(start);
        self.raw["opacity"] = json!((self.opacity as f64 * 100.0).round() / 100.0);
        // Only what differs from the defaults, so improved defaults still reach old configs.
        let mut colors = serde_json::Map::new();
        for ((key, _, default), color) in theme::BASE.iter().zip(&self.colors) {
            if theme::parse(default) != Some(*color) {
                colors.insert(key.to_string(), json!(theme::hex(*color)));
            }
        }
        self.raw["colors"] = Value::Object(colors);
        if self.extension_colors == default_extensions() {
            self.raw.as_object_mut().map(|o| o.remove("ext_colors"));
        } else {
            self.raw["ext_colors"] = Value::Array(
                self.extension_colors
                    .iter()
                    .map(|(e, c)| json!({ "exts": e, "color": theme::hex(*c) }))
                    .collect(),
            );
        }
        self.raw["font"] = json!({ "family": self.font_family, "size": self.font_size });
        self.raw["search_links"] = json!(self.search_links);
        if let Some(w) = self.window {
            self.raw["window"] = json!({ "x": w.x, "y": w.y, "width": w.width, "height": w.height, "maximized": w.maximized });
        }
        self.raw["bookmarks"] = Value::Array(
            self.bookmarks
                .iter()
                .map(|b| json!({ "name": b.name, "path": b.path.to_string_lossy() }))
                .collect(),
        );
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(&self.raw).map_err(|e| e.to_string())?;
        // Write then rename, so a crash never leaves a half-written settings file.
        let temp = self.file.with_extension("json.tmp");
        std::fs::write(&temp, text).map_err(|e| e.to_string())?;
        // A scanner or indexer may hold the file for a moment: retry briefly.
        let mut attempt = 0;
        loop {
            match std::fs::rename(&temp, &self.file) {
                Ok(()) => return Ok(()),
                Err(e) if attempt >= 20 => return Err(e.to_string()),
                Err(_) => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_keeps_unknown_keys_and_defaults() {
        let dir = std::env::temp_dir().join(format!("enyfile-config-{}", std::process::id()));
        let file = dir.join("config.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &file,
            r#"{ "theme": "dark", "places": { "music": "D:\\曲" }, "bookmarks": [ { "name": "作業", "path": "D:\\Work" } ] }"#,
        )
        .unwrap();
        let mut config = Config::load(file.clone());
        assert_eq!(config.place(4), Some(PathBuf::from(r"D:\曲")));
        assert!(config.places[0].is_none() && config.place(0).is_some(), "desktop falls back to the known folder");
        assert_eq!(config.bookmarks, [Bookmark { name: "作業".into(), path: r"D:\Work".into() }]);
        assert_eq!(config.split, 0.5);
        config.bookmarks.push(Bookmark { name: "x".into(), path: r"C:\x".into() });
        config.places[2] = Some(r"E:\写真".into());
        config.split = 0.3;
        config.save().unwrap();
        let again = Config::load(file.clone());
        assert_eq!(again.bookmarks.len(), 2);
        assert_eq!(again.raw["theme"], "dark");
        assert!(again.raw["places"].get("desktop").is_none(), "defaults are not written");
        assert_eq!(again.place(2), Some(PathBuf::from(r"E:\写真")));
        assert_eq!(again.place(4), Some(PathBuf::from(r"D:\曲")));
        assert!((again.split - 0.3).abs() < 1e-6);
        assert_eq!(again.window, None);
        let mut config = again;
        let placement = Placement { x: -1900, y: 40, width: 1200, height: 700, maximized: true };
        config.window = Some(placement);
        config.save().unwrap();
        assert_eq!(Config::load(file.clone()).window, Some(placement));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
