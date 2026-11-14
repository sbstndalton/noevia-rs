//! The few JavaScript and Node semantics the detector depends on: the `\s` character class, `String.prototype.trim`,
//! and `path.posix.{normalize,join,basename}`. Kept exact so verdicts match the Node oracle on odd input.

/// JavaScript's `\s` (and the set `trim()` removes): ASCII tab..CR, space, and the Unicode space separators, line and
/// paragraph separators and the byte-order mark. Unlike `char::is_whitespace` it has U+FEFF and lacks U+0085.
pub(crate) fn is_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// Line terminators for JavaScript's `^`/`$` in multiline mode and the characters `.` refuses.
pub(crate) fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

pub(crate) fn trim_end(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `path.posix.normalize`.
pub(crate) fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() {
        if absolute {
            return "/".to_string();
        }
        return if trailing { "./".to_string() } else { ".".to_string() };
    }
    if trailing {
        out.push('/');
    }
    if absolute {
        format!("/{out}")
    } else {
        out
    }
}

/// `path.posix.join` of two parts.
pub(crate) fn join(a: &str, b: &str) -> String {
    let parts: Vec<&str> = [a, b].into_iter().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return ".".to_string();
    }
    normalize(&parts.join("/"))
}

/// `path.posix.basename` without a suffix argument.
pub(crate) fn basename(path: &str) -> &str {
    let stripped = path.trim_end_matches('/');
    if stripped.is_empty() {
        return "";
    }
    match stripped.rfind('/') {
        Some(at) => stripped.get(at + 1..).unwrap_or(""),
        None => stripped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_paths_match_node() {
        assert_eq!(normalize("/usr/local/bin/"), "/usr/local/bin/");
        assert_eq!(normalize("/app/./x/../y"), "/app/y");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a/../.."), "..");
        assert_eq!(normalize("./"), "./");
        assert_eq!(join("/app", "./dist"), "/app/dist");
        assert_eq!(join("/", "x"), "/x");
        assert_eq!(join("/usr/local/bin", ""), "/usr/local/bin");
        assert_eq!(basename("/out/"), "out");
        assert_eq!(basename("/"), "");
        assert_eq!(basename("."), ".");
        assert_eq!(basename("a/b"), "b");
    }

    #[test]
    fn whitespace_is_the_javascript_class() {
        assert!(is_space('\u{feff}'));
        assert!(!is_space('\u{85}'));
        assert_eq!(trim("\u{a0} x \u{2028}"), "x");
    }
}
