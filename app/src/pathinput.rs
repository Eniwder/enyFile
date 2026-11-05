//! Expand path-bar variables once, without invoking a command interpreter.
use std::ffi::OsString;

pub fn expand(text: &str) -> Result<OsString, &'static str> {
    expand_with(text, |name| std::env::var_os(name))
}

fn expand_with(text: &str, mut lookup: impl FnMut(&str) -> Option<OsString>) -> Result<OsString, &'static str> {
    use std::os::windows::ffi::OsStrExt;

    let mut out = OsString::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else { break };
        let end = start + len + 2;
        let name = &rest[start + 1..end - 1];
        out.push(&rest[..start]);
        // Invalid/unknown names and %% stay literal. Never re-expand values:
        // percent signs and shell metacharacters in a value are path text.
        let value = if name.is_empty() || name.contains(['=', '\0']) { None } else { lookup(name) };
        match value {
            Some(value) => out.push(value),
            None => out.push(&rest[start..end]),
        }
        rest = &rest[end..];
    }
    out.push(rest);
    // Win32 takes NUL-terminated strings: reject rather than silently truncate.
    if out.encode_wide().any(|c| c == 0) {
        return Err("パスに使用できない文字（NUL）が含まれています");
    }
    Ok(out)
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
        assert_eq!(expand_with(r"%userprofile%\%CHILD%", lookup).unwrap(), OsString::from(r"C:\Users\日本語 user\Documents"));
        for text in [r"C:\plain", "%UNKNOWN%", "%%", "%unfinished", "%bad=name%"] {
            assert_eq!(expand_with(text, lookup).unwrap(), OsString::from(text));
        }
    }

    #[test]
    fn values_are_not_commands_or_recursive_templates() {
        let value = r"C:\%TEMP%\a & whoami; $(calc.exe)";
        let result = expand_with("%VALUE%", |_| Some(OsString::from(value))).unwrap();
        assert_eq!(result, OsString::from(value));
        assert!(expand_with("bad\0path", |_| None).is_err());
        assert!(expand_with("%VALUE%", |_| Some(OsString::from("bad\0path"))).is_err());
    }

    #[test]
    fn preserves_non_unicode_environment_values() {
        use std::os::windows::ffi::OsStringExt;
        let value = OsString::from_wide(&[67, 58, 92, 0xD800]);
        assert_eq!(expand_with("%VALUE%", |_| Some(value.clone())).unwrap(), value);
    }
}
