//! File-name rules: the quoting of a name for system text (`quoted-name.cjs`) and the
//! "does the question name this file" token rule (`rag.cjs` `questionNames`, #1457).

use prompt_framing::js::{to_lower, units};

/// `JSON.stringify(name)`, then the code points `quoted-name.cjs` also writes as `\uXXXX`.
pub fn quote_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || is_raw_escaped(c) => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The code points `quoted-name.cjs` (RAW) escapes after JSON quoting.
fn is_raw_escaped(c: char) -> bool {
    matches!(
        c,
        '\u{85}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// `quotedNames(names)`: each name quoted, joined with ", ".
pub fn quoted_names(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| quote_name(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `questionNames(name)` from `filesContext`: the lower-cased question contains the
/// lower-cased name, or its last `/` segment, as a whole token. Look-behind and look-ahead
/// are written out by hand (the `regex` crate has neither).
pub fn question_names(query: &str, name: &str) -> bool {
    let q = to_lower(&units(query));
    let full = to_lower(&units(name));
    let last = full.rsplit(|&c| c == u16::from(b'/')).next().unwrap_or(&[]);
    whole_token(&q, &full) || whole_token(&q, last)
}

/// `[a-z0-9_.\-]`: may not come right before a match.
fn is_before_class(c: u16) -> bool {
    is_ascii_alnum(c) || matches!(c, 0x5f | 0x2e | 0x2d)
}

/// `[a-z0-9_\-]`: may not come right after a match.
fn is_word_class(c: u16) -> bool {
    is_ascii_alnum(c) || matches!(c, 0x5f | 0x2d)
}

fn is_ascii_alnum(c: u16) -> bool {
    (0x61..=0x7a).contains(&c) || (0x30..=0x39).contains(&c)
}

/// `(?<![a-z0-9_.\-])needle(?![a-z0-9_\-]|\.[a-z0-9])` over UTF-16 code units, as in the JS regex.
fn whole_token(hay: &[u16], needle: &[u16]) -> bool {
    (0..=hay.len()).any(|pos| {
        let before_ok = pos == 0 || hay.get(pos - 1).is_some_and(|&c| !is_before_class(c));
        let end = pos + needle.len();
        before_ok && hay.get(pos..end) == Some(needle) && after_ok(hay, end)
    })
}

fn after_ok(hay: &[u16], end: usize) -> bool {
    match hay.get(end) {
        None => true,
        Some(&c) => {
            !is_word_class(c)
                && !(c == u16::from(b'.') && hay.get(end + 1).is_some_and(|&d| is_ascii_alnum(d)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{question_names, quote_name};

    #[test]
    fn quoting_matches_quoted_name_js() {
        assert_eq!(quote_name("plain.md"), "\"plain.md\"");
        assert_eq!(quote_name("quote \"x\".md"), "\"quote \\\"x\\\".md\"");
        assert_eq!(quote_name("a\u{1}\n\\"), "\"a\\u0001\\n\\\\\"");
        assert_eq!(quote_name("line\u{2028}break"), "\"line\\u2028break\"");
        assert_eq!(quote_name("\u{7f}é"), "\"\u{7f}é\"");
    }

    #[test]
    fn whole_token_rule() {
        assert!(question_names("what is in data.md?", "data.md"));
        assert!(!question_names("what is in mydata.md?", "data.md"));
        assert!(!question_names("see data.md.bak", "data.md"));
        assert!(question_names("DATA.MD please", "data.md"));
        assert!(question_names("open data.md now", "docs/data.md"));
        assert!(question_names("open docs/data.md now", "docs/data.md"));
    }
}
