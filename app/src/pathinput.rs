//! Path editor formatting and parsing for Windows.
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};

pub const NUL_ERROR: &str = "パスに使用できない文字（NUL）が含まれています";

pub fn display(path: &Path) -> String {
    let plain = match path.components().next() {
        Some(Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..)
            ) =>
        {
            crate::shell::os_path(path).ok()
        }
        _ => None,
    };
    plain
        .as_deref()
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn trim(text: &str) -> &str {
    text.trim().trim_matches('"')
}

fn native(path: &OsStr) -> Result<PathBuf, &'static str> {
    let mut wide: Vec<_> = path.encode_wide().collect();
    for c in &mut wide {
        match *c {
            // Win32 uses NUL-terminated strings.
            0 => return Err(NUL_ERROR),
            47 => *c = 92,
            _ => {}
        }
    }
    Ok(OsString::from_wide(&wide).into())
}

pub fn literal(text: &str) -> Result<Option<PathBuf>, &'static str> {
    let text = trim(text);
    (!text.is_empty())
        .then(|| native(OsStr::new(text)))
        .transpose()
}

pub enum Draft {
    Saved(Option<PathBuf>),
    Edited(String),
}

impl Default for Draft {
    fn default() -> Self {
        Self::Saved(None)
    }
}

impl Draft {
    pub fn text(&self) -> String {
        match self {
            Self::Saved(path) => path.as_deref().map(display).unwrap_or_default(),
            Self::Edited(text) => text.clone(),
        }
    }

    pub fn path(&self) -> Result<Option<PathBuf>, &'static str> {
        match self {
            Self::Saved(Some(path)) if path.as_os_str().encode_wide().any(|c| c == 0) => {
                Err(NUL_ERROR)
            }
            Self::Saved(path) => Ok(path.clone()),
            Self::Edited(text) => literal(text),
        }
    }
}

pub fn expand(text: &str) -> Result<PathBuf, &'static str> {
    expand_with(text, |name| std::env::var_os(name))
}

fn expand_with(
    text: &str,
    mut lookup: impl FnMut(&str) -> Option<OsString>,
) -> Result<PathBuf, &'static str> {
    let mut out = OsString::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else {
            break;
        };
        let end = start + len + 2;
        let name = &rest[start + 1..end - 1];
        out.push(&rest[..start]);
        // Invalid/unknown names and %% stay literal. Never re-expand values:
        // percent signs and shell metacharacters in a value are path text.
        let value = if name.is_empty() || name.contains(['=', '\0']) {
            None
        } else {
            lookup(name)
        };
        match value {
            Some(value) => out.push(value),
            None => out.push(&rest[start..end]),
        }
        rest = &rest[end..];
    }
    out.push(rest);
    native(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_and_suffixes_are_paths() {
        let lookup = |name: &str| match name.to_ascii_uppercase().as_str() {
            "USERPROFILE" => Some(OsString::from(r"C:\Users\日本語 user")),
            "CHILD" => Some(OsString::from("Documents")),
            _ => None,
        };
        assert_eq!(
            expand_with(r"%userprofile%\%CHILD%", lookup).unwrap(),
            PathBuf::from(r"C:\Users\日本語 user\Documents")
        );
        for text in [r"C:\plain", "%UNKNOWN%", "%%", "%unfinished", "%bad=name%"] {
            assert_eq!(expand_with(text, lookup).unwrap().as_os_str(), text);
        }
    }

    #[test]
    fn values_are_not_commands_or_recursive_templates() {
        let value = r"C:\%TEMP%\a & whoami; $(calc.exe)";
        let result = expand_with("%VALUE%", |_| Some(OsString::from(value))).unwrap();
        assert_eq!(result.as_os_str(), value);
        assert!(expand_with("bad\0path", |_| None).is_err());
        assert!(expand_with("%VALUE%", |_| Some(OsString::from("bad\0path"))).is_err());
    }

    #[test]
    fn preserves_non_unicode_environment_values() {
        let value = OsString::from_wide(&[67, 58, 92, 0xD800]);
        assert_eq!(
            expand_with("%VALUE%", |_| Some(value.clone())).unwrap(),
            PathBuf::from(value)
        );
    }

    #[test]
    fn editor_paths_keep_windows_prefixes_and_meaning() {
        for (text, expected) in [
            ("C:/Users\\日本語/", r"C:\Users\日本語\"),
            ("//server/share\\folder", r"\\server\share\folder"),
            (
                "//?/UNC/server/share/folder.",
                r"\\?\UNC\server\share\folder.",
            ),
            ("//?/Volume{guid}/folder", r"\\?\Volume{guid}\folder"),
            ("../folder", r"..\folder"),
        ] {
            let path = literal(text).unwrap().unwrap();
            assert_eq!(path.as_os_str(), expected);
            assert!(display(&path).contains('/'));
        }
        assert_eq!(display(Path::new(r"\\?\C:\folder.")), "//?/C:/folder.");
        assert_eq!(
            display(Path::new(r"\\?\UNC\server\share")),
            "//server/share"
        );
        assert_eq!(
            literal(" \"C:/My Folder\" ").unwrap(),
            Some(PathBuf::from(r"C:\My Folder"))
        );
        assert!(literal("C:/bad\0path").is_err());
        assert!(literal(" ").unwrap().is_none());
        assert_eq!(
            expand_with("%ROOT%/child", |_| Some("//server/share".into())).unwrap(),
            PathBuf::from(r"\\server\share\child")
        );
    }
}
