//! Colors and font: defaults, settings keys and parsing. The UI's `Theme` global holds the
//! live values; `main.rs` copies these into it.
use std::collections::HashMap;

/// Base colors: (config key, label in the settings, default).
pub const BASE: &[(&str, &str, &str)] = &[
    ("pane", "背景", "#2d2d2d"),
    ("text", "前景", "#dcdcdc"),
    ("folder-text", "フォルダ", "#e8e8e8"),
    ("drive-text", "ドライブ", "#64a8ee"),
    ("readonly-text", "読み取り専用属性", "#d8c060"),
    ("hidden-text", "隠し属性", "#8a8a8a"),
    ("size-text", "サイズ", "#c8c8c8"),
    ("time-text", "日付・時刻", "#c8c8c8"),
    ("attr-text", "属性", "#a0a0a0"),
    ("kind", "種類", "#5ea0e6"),
    ("selected-bg", "選択", "#2f4c6e"),
    ("cursor-bg", "カーソル", "#d6d6d6"),
    ("accent", "強調（件数・空き容量）", "#64a8ee"),
    ("chrome", "ウィンドウ背景", "#383838"),
    ("bar", "パス欄・バー", "#353535"),
    ("diff-add-bg", "比較: 右（新）にだけある", "#1e4a2b"),
    ("diff-del-bg", "比較: 左（旧）にだけある", "#5a2329"),
    ("diff-change-bg", "比較: 違う項目（サイズ・日時・属性）", "#5c5320"),
    // A light palette needs these too (the defaults are those of the dark theme).
    ("selected-text", "選択の文字", "#ffffff"),
    ("cursor-text", "カーソルの文字", "#1c1c1c"),
    ("cursor-selected-bg", "カーソル（選択中の項目）", "#9cc2ec"),
    ("cursor-inactive-bg", "カーソル（非アクティブなペイン）", "#3a3a3a"),
    ("cursor-inactive-line", "カーソルの枠線（非アクティブなペイン）", "#5c5c5c"),
    ("dim", "薄い文字", "#9a9a9a"),
    ("border", "枠線", "#4a4a4a"),
    ("dialog", "ダイアログ・メニュー", "#3a3a3a"),
];

/// Default extension colors, after NexusFile's groups (a few newer formats added).
pub const EXTENSIONS: &[(&str, &str)] = &[
    ("7Z;ACE;ALZ;ARJ;CAB;GZ;LZH;RAR;TAR;XZ;ZIP", "#a0a020"),
    ("ASF;AVI;MKV;MOV;MP4;MPEG;MPG;RAM;RM;WEBM;WMV", "#ff8040"),
    ("BAK;TMP", "#e04040"),
    ("BAT;CMD;LNK;PIF;PS1", "#f0f040"),
    ("BMP;GIF;ICO;JPEG;JPG;PNG;PSD;PSP;SVG;TIF;TIFF;WEBP;WMF", "#d060f0"),
    ("CAP;DOC;DOCX;GAL;GUL;HWP;MD;PDF;RTF;TXT", "#40b0b0"),
    ("CFG;INF;INI;REG;TOML;YAML;YML", "#909090"),
    ("CHM;HLP", "#b06020"),
    ("DLL;SYS", "#ff40ff"),
    ("EXE;MSI", "#30e050"),
    ("HTM;HTML;URL", "#8888ff"),
    ("FLAC;M4A;MID;MP2;MP3;OGG;WAV", "#e06040"),
    ("PPT;PPTX;XLS;XLSX", "#2090c0"),
];

pub const DEFAULT_FONT: &str = "Meiryo UI";
pub const DEFAULT_SIZE: f32 = 13.0;

pub fn parse(hex: &str) -> Option<slint::Color> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(slint::Color::from_rgb_u8((value >> 16) as u8, (value >> 8) as u8, value as u8))
}
pub fn hex(color: slint::Color) -> String {
    format!("#{:02x}{:02x}{:02x}", color.red(), color.green(), color.blue())
}
/// "zip; RAR" -> ["zip", "rar"]
pub fn split_extensions(text: &str) -> Vec<String> {
    text.split([';', ',', ' '])
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}
/// Lower-case extension -> color; earlier rules win when an extension repeats.
pub fn extension_map(rules: &[(String, slint::Color)]) -> HashMap<String, slint::Color> {
    let mut map = HashMap::new();
    for (extensions, color) in rules {
        for ext in split_extensions(extensions) {
            map.entry(ext).or_insert(*color);
        }
    }
    map
}

/// Installed font families (no vertical "@" variants), sorted.
pub fn font_families() -> Vec<String> {
    static FONTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FONTS.get_or_init(enumerate_font_families).clone()
}

pub fn filter_fonts(fonts: &[String], query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    fonts.iter().filter(|name| name.to_lowercase().contains(&query)).cloned().collect()
}

fn enumerate_font_families() -> Vec<String> {
    use windows::Win32::{Foundation::LPARAM, Graphics::Gdi::*};
    unsafe extern "system" fn collect(font: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, list: LPARAM) -> i32 {
        let list = unsafe { &mut *(list.0 as *mut Vec<String>) };
        let face = unsafe { &(*font).lfFaceName };
        let len = face.iter().position(|&c| c == 0).unwrap_or(face.len());
        let name = String::from_utf16_lossy(&face[..len]);
        if !name.starts_with('@') {
            list.push(name);
        }
        1
    }
    let mut list: Vec<String> = Vec::new();
    unsafe {
        let dc = GetDC(None);
        let mut query = LOGFONTW { lfCharSet: DEFAULT_CHARSET, ..Default::default() };
        EnumFontFamiliesExW(dc, &mut query, Some(collect), LPARAM(&mut list as *mut _ as isize), 0);
        ReleaseDC(None, dc);
    }
    list.sort();
    list.dedup();
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Ask the OS through the character fallback path first, then require those
    /// supported characters to survive Parley's normal selected-font path.
    /// This does not require a particular CJK font on CI or bundle test fonts.
    #[test]
    fn os_supported_characters_survive_normal_font_selection() {
        use parley::{FontContext, LayoutContext, StyleProperty, fontique};
        use parley::style::{FontFamily, FontFamilyName};
        let mut collection = fontique::Collection::default();
        let mut cache = fontique::SourceCache::default();
        let mut supported = String::new();
        for ch in "A019あア漢难汉繁體中文龍門한국어𠀀⟰⨌😀".chars() {
            let mut query = collection.query(&mut cache);
            query.set_families(std::iter::empty::<fontique::QueryFamily<'_>>());
            // Unknown script has no representative sample; use actual-text OS fallback.
            query.set_fallbacks(fontique::FallbackKey::new(fontique::Script::from_bytes(*b"Zzzz"), None));
            query.set_fallback_chars([ch]);
            let mut found = false;
            query.matches_with(|font| {
                if font.charmap().and_then(|map| map.map(ch)).is_some_and(|glyph| glyph != 0) {
                    found = true;
                    fontique::QueryStatus::Stop
                } else {
                    fontique::QueryStatus::Continue
                }
            });
            if found { supported.push(ch); }
        }
        assert!(!supported.is_empty(), "the OS should provide at least a basic text font");
        let mut fonts = FontContext::default();
        let mut layouts = LayoutContext::<[u8; 4]>::new();
        let mut builder = layouts.ranged_builder(&mut fonts, &supported, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::Single(FontFamilyName::named(DEFAULT_FONT))));
        builder.push_default(StyleProperty::FontSize(DEFAULT_SIZE));
        let mut layout = builder.build(&supported);
        layout.break_all_lines(None);
        let missing = layout.lines().flat_map(|line| line.items()).map(|item| match item {
            parley::layout::PositionedLayoutItem::GlyphRun(run) => run.glyphs().filter(|glyph| glyph.id == 0).count(),
            _ => 0,
        }).sum::<usize>();
        assert_eq!(missing, 0, "OS-supported text lost glyphs: {supported}");
    }
    #[test]
    fn font_filter_matches_substrings_without_case() {
        let fonts = vec!["Meiryo UI".into(), "Noto Sans JP".into(), "Noto Serif JP".into(), "游ゴシック".into()];
        assert_eq!(filter_fonts(&fonts, " NOTO "), fonts[1..3]);
        assert_eq!(filter_fonts(&fonts, "ゴシック"), fonts[3..]);
        assert!(filter_fonts(&fonts, "not installed").is_empty());
        assert_eq!(filter_fonts(&fonts, ""), fonts);
    }
    #[test]
    fn colors_and_extensions() {
        let c = parse("#40b0b0").unwrap();
        assert_eq!(hex(c), "#40b0b0");
        assert!(parse("40B0B0").is_some() && parse("#12345").is_none() && parse("#zzzzzz").is_none());
        assert_eq!(split_extensions(" zip; .RAR ,7z"), ["zip", "rar", "7z"]);
        let red = parse("#ff0000").unwrap();
        let map = extension_map(&[("TXT;MD".into(), c), ("txt".into(), red)]);
        assert_eq!(map["txt"], c, "first rule wins");
        assert_eq!(map["md"], c);
        assert!(BASE.iter().all(|(_, _, d)| parse(d).is_some()));
        assert!(EXTENSIONS.iter().all(|(_, d)| parse(d).is_some()));
    }
}
