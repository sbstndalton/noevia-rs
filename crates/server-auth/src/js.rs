//! The JavaScript semantics the Node gate relies on, ported exactly: `\s` (and `trim`), ASCII-only
//! case-insensitive regex prefixes, `decodeURIComponent`, truthiness and number comparisons of
//! better-sqlite3 values.

use server_store::rusqlite::types::Value;
use std::fmt::Write as _;

/// ECMAScript `\s` (WhiteSpace + LineTerminator), the set `trim()` removes.
pub fn is_space(c: char) -> bool {
    u16::try_from(u32::from(c)).is_ok_and(policy_leaves::is_js_space)
}

/// `s` without its leading `\s*`.
pub fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// `/^<word>/i` without the `u` flag: ASCII case-insensitive (non-`u` canonicalisation never maps
/// a non-ASCII character onto ASCII). Returns the rest after the word.
pub fn strip_prefix_ci<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let head = s.get(..word.len())?;
    head.eq_ignore_ascii_case(word)
        .then(|| s.get(word.len()..))
        .flatten()
}

/// `decodeURIComponent`, `None` where it throws (a `%` not followed by two hex digits, or escapes
/// that are not well-formed UTF-8, overlong forms and surrogates included).
///
/// Equivalence with the JS algorithm: escaped bytes and literal characters are concatenated as
/// UTF-8 and the result must be valid UTF-8. JS also requires a multi-byte sequence to come only
/// from escapes; a literal character can never complete or continue an escaped lead byte in valid
/// UTF-8 (a literal is either ASCII or starts with a lead byte), so both rules reject the same
/// inputs.
pub fn decode_uri_component(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `!!value` for a better-sqlite3 column value (BLOBs arrive as Buffers: truthy).
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Integer(i) => *i != 0,
        Value::Real(f) => *f != 0.0 && !f.is_nan(),
        Value::Text(t) => !t.is_empty(),
        Value::Blob(_) => true,
    }
}

/// The value as a JS number for `>`/`<=` against a number, where that is unambiguous. TEXT and
/// BLOB columns (never written by Node in the columns compared) give `None`, and every comparison
/// with `None` is treated as failing, so an odd row is refused, never accepted.
pub fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Real(f) => Some(*f),
        Value::Null => Some(0.0),
        Value::Text(_) | Value::Blob(_) => None,
    }
}

/// `String(v)` for a TEXT column; anything else is `None` (refused by callers).
pub fn text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

/// Lowercase hex SHA-256 of the UTF-8 of `s`: core auth.cjs `digest`.
pub fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in d {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Constant-time equality of two strings' SHA-256 digests: the time does not depend on where
/// they differ or on either length.
pub fn ct_eq(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    let (da, db) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    bool::from(da.as_slice().ct_eq(db.as_slice()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decode_matches_js() {
        assert_eq!(decode_uri_component("a%20b").as_deref(), Some("a b"));
        assert_eq!(decode_uri_component("%C3%A9").as_deref(), Some("é"));
        assert_eq!(decode_uri_component("%c3%a9").as_deref(), Some("é"));
        assert_eq!(decode_uri_component("%3B%3D").as_deref(), Some(";="));
        for bad in [
            "%",
            "%2",
            "%zz",
            "%C3",
            "%C3x",
            "%C0%AF",
            "%ED%A0%80",
            "%FF",
            "%E2%82",
            "%C3\u{e9}",
        ] {
            assert_eq!(decode_uri_component(bad), None, "{bad}");
        }
        assert_eq!(decode_uri_component("é").as_deref(), Some("é"));
    }

    #[test]
    fn spaces_are_js_spaces() {
        for c in [
            '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '\u{a0}', '\u{2028}', '\u{3000}', '\u{feff}',
        ] {
            assert!(is_space(c), "{c:?}");
        }
        for c in ['\u{85}', 'a', '\u{200b}', '\u{1f600}'] {
            assert!(!is_space(c), "{c:?}");
        }
    }

    #[test]
    fn prefix_is_ascii_case_insensitive() {
        assert_eq!(strip_prefix_ci("bEaReR x", "Bearer"), Some(" x"));
        assert_eq!(strip_prefix_ci("Bear", "Bearer"), None);
        assert_eq!(strip_prefix_ci("Béarer", "Bearer"), None);
    }

    #[test]
    fn ct_eq_is_equality() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(ct_eq("", ""));
        assert_eq!(
            digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
