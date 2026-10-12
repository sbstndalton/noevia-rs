//! JavaScript strings are UTF-16 and may hold lone surrogates; Node keeps them and
//! `JSON.stringify` writes them as `\udxxx` escapes. They arise in ordinary data: the web client
//! cuts a chat preview with `text.slice(0, 200)`, which halves an emoji that straddles the cut, and
//! core stores what it was sent. [`crate::parse`] refuses such strings (a Rust `String` cannot hold
//! them); this module is the lossless alternative for data Node wrote or will read back.
//!
//! A lone surrogate unit `u` (U+D800..=U+DFFF) is held in a `String` as the character
//! `LONE_BASE + (u - 0xD800)`, U+10F800..=U+10FFFF, the last 2,048 code points of Supplementary
//! Private Use Area-B. The mapping is injective because [`parse`] refuses (as
//! [`Refusal::Unsupported`]) text that already holds a character of that range, so equality of
//! mapped strings is equality of the JS strings. [`stringify`] and [`stringify_pretty`] write a
//! mapped character as `JSON.stringify` writes the lone surrogate (`\ud83d`, lowercase hex);
//! [`len`] and [`slice`] count it as the one UTF-16 unit it is, and [`slice`] cuts a surrogate
//! pair in half the way `String.prototype.slice` does.
//!
//! [`parse`] also puts an object's array-index keys (`"0"`, `"42"`) first, in ascending order, as
//! JS property order does; the remaining keys keep their insertion order.

use crate::{JValue, ParseError};

/// The character a lone surrogate U+D800 is held as.
pub const LONE_BASE: u32 = 0x10_F800;

/// Why [`parse`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `JSON.parse` throws on this text as well.
    Invalid,
    /// `JSON.parse` would accept it, this port does not: a character of U+10F800..=U+10FFFF, a
    /// number outside the `f64` range (JS: `Infinity`), or nesting serde_json does not follow.
    Unsupported,
}

impl From<ParseError> for Refusal {
    fn from(_: ParseError) -> Self {
        Refusal::Invalid
    }
}

/// The surrogate unit `c` stands for, if it is a mapped lone surrogate.
pub fn lone_unit(c: char) -> Option<u16> {
    let v = u32::from(c);
    if v >= LONE_BASE {
        u16::try_from(v - LONE_BASE + 0xD800).ok()
    } else {
        None
    }
}

/// The character a lone surrogate unit is held as (`None` for a unit that is not a surrogate).
pub fn lone_char(unit: u16) -> Option<char> {
    if (0xD800..=0xDFFF).contains(&unit) {
        char::from_u32(LONE_BASE + u32::from(unit) - 0xD800)
    } else {
        None
    }
}

fn reserved(c: char) -> bool {
    u32::from(c) >= LONE_BASE
}

/// Deeper than this the text is handed to serde_json as it is; if serde_json then refuses, that
/// is [`Refusal::Unsupported`] (its recursion limit is 128), never `Invalid`.
const DEPTH_FLAG: usize = 100;

fn hex4(s: &str) -> Option<u16> {
    if s.len() != 4 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(s, 16).ok()
}

/// `JSON.parse(text)`, keeping lone surrogates.
pub fn parse(text: &str) -> Result<JValue, Refusal> {
    // One pass over the text: lone surrogate escapes become their mapped characters (serde_json
    // refuses them), and what JS would accept but this port refuses is noted. Only escapes inside
    // strings are rewritten, so the structure serde_json then validates is the text's own.
    let mut out = String::with_capacity(text.len());
    let mut unsupported = false;
    let mut deep = false;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let w = c.len_utf8();
        if in_string {
            match c {
                '"' => {
                    in_string = false;
                    out.push(c);
                    rest = rest.get(w..).unwrap_or("");
                }
                '\\' => {
                    let after = rest.get(1..).unwrap_or("");
                    if after.starts_with('u') {
                        let unit = after.get(1..5).and_then(hex4);
                        match unit {
                            Some(hi @ 0xD800..=0xDBFF) => {
                                let low = after
                                    .get(5..7)
                                    .filter(|s| *s == "\\u")
                                    .and_then(|_| after.get(7..11))
                                    .and_then(hex4)
                                    .filter(|lo| (0xDC00..=0xDFFF).contains(lo));
                                if let Some(lo) = low {
                                    let cp = 0x10000
                                        + ((u32::from(hi) - 0xD800) << 10)
                                        + (u32::from(lo) - 0xDC00);
                                    if cp >= LONE_BASE {
                                        unsupported = true;
                                    }
                                    out.push_str(rest.get(..12).unwrap_or(""));
                                    rest = rest.get(12..).unwrap_or("");
                                } else {
                                    if let Some(m) = lone_char(hi) {
                                        out.push(m);
                                    }
                                    rest = rest.get(6..).unwrap_or("");
                                }
                            }
                            Some(lo @ 0xDC00..=0xDFFF) => {
                                if let Some(m) = lone_char(lo) {
                                    out.push(m);
                                }
                                rest = rest.get(6..).unwrap_or("");
                            }
                            Some(_) => {
                                out.push_str(rest.get(..6).unwrap_or(""));
                                rest = rest.get(6..).unwrap_or("");
                            }
                            None => {
                                // Not four hex digits: left for serde_json to refuse.
                                out.push_str("\\u");
                                rest = rest.get(2..).unwrap_or("");
                            }
                        }
                    } else if let Some(e) = after.chars().next() {
                        out.push('\\');
                        out.push(e);
                        rest = after.get(e.len_utf8()..).unwrap_or("");
                    } else {
                        out.push('\\');
                        rest = "";
                    }
                }
                _ => {
                    if reserved(c) {
                        unsupported = true;
                    }
                    out.push(c);
                    rest = rest.get(w..).unwrap_or("");
                }
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' | '{' => {
                depth += 1;
                if depth > DEPTH_FLAG {
                    deep = true;
                }
            }
            ']' | '}' => depth = depth.saturating_sub(1),
            '-' | '0'..='9' => {
                let end = rest
                    .find(|x: char| {
                        !(x.is_ascii_digit() || matches!(x, '-' | '+' | '.' | 'e' | 'E'))
                    })
                    .unwrap_or(rest.len());
                let lexeme = rest.get(..end).unwrap_or("");
                if lexeme.parse::<f64>().is_ok_and(f64::is_infinite) {
                    unsupported = true;
                }
                out.push_str(lexeme);
                rest = rest.get(end..).unwrap_or("");
                continue;
            }
            _ => {}
        }
        out.push(c);
        rest = rest.get(w..).unwrap_or("");
    }
    match crate::parse(&out) {
        Ok(_) if unsupported => Err(Refusal::Unsupported),
        Ok(mut v) => {
            js_key_order(&mut v);
            Ok(v)
        }
        Err(_) if unsupported || deep => Err(Refusal::Unsupported),
        Err(_) => Err(Refusal::Invalid),
    }
}

/// An array index as a property key: the canonical decimal form of 0..=2^32-2.
pub fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if key.len() > 1 && key.starts_with('0') {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|n| *n < u64::from(u32::MAX))
        .and_then(|n| u32::try_from(n).ok())
}

/// JS property order for one object: array-index keys first, ascending, then the rest as they
/// came. (The values are left alone; [`parse`] applies it at every level.)
pub fn order_keys(items: &mut [(String, JValue)]) {
    if items.iter().any(|(k, _)| array_index(k).is_some()) {
        // Stable: non-index keys keep their order.
        items.sort_by_key(|(k, _)| match array_index(k) {
            Some(n) => (0u8, n),
            None => (1u8, 0),
        });
    }
}

fn js_key_order(v: &mut JValue) {
    // Iterative, so a deep value cannot exhaust the stack.
    let mut stack: Vec<&mut JValue> = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            JValue::Obj(items) => {
                order_keys(items);
                stack.extend(items.iter_mut().map(|(_, x)| x));
            }
            JValue::Arr(items) => stack.extend(items.iter_mut()),
            _ => {}
        }
    }
}

/// `obj[key] = value` with JS property order: an existing key keeps its place; a new array-index
/// key goes before the first larger index key (or the first non-index key), any other new key at
/// the end.
pub fn set(items: &mut Vec<(String, JValue)>, key: &str, value: JValue) {
    if let Some(slot) = items.iter_mut().find(|(k, _)| k == key) {
        slot.1 = value;
        return;
    }
    match array_index(key) {
        Some(n) => {
            let at = items
                .iter()
                .position(|(k, _)| array_index(k).is_none_or(|m| m > n))
                .unwrap_or(items.len());
            items.insert(at, (key.to_string(), value));
        }
        None => items.push((key.to_string(), value)),
    }
}

/// `JSON.stringify(value)` with lone surrogates written as JS writes them.
pub fn stringify(v: &JValue) -> Option<String> {
    if matches!(v, JValue::Undefined) {
        return None;
    }
    let mut out = String::new();
    crate::emit(v, &mut out, true);
    Some(out)
}

/// `JSON.stringify(value, null, 2)` (core `atomicJson`'s bytes) with lone surrogates.
pub fn stringify_pretty(v: &JValue) -> Option<String> {
    if matches!(v, JValue::Undefined) {
        return None;
    }
    let mut out = String::new();
    crate::emit_pretty(v, &mut out, 0, true);
    Some(out)
}

/// `s.length`: UTF-16 code units, a mapped lone surrogate being one.
pub fn len(s: &str) -> usize {
    s.chars()
        .map(|c| if reserved(c) { 1 } else { c.len_utf16() })
        .sum()
}

/// `s.slice(0, n)`: a surrogate pair cut in half keeps its high surrogate, as in JS.
pub fn slice(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for c in s.chars() {
        let w = if reserved(c) { 1 } else { c.len_utf16() };
        if units + w > n {
            if w == 2 && units < n {
                let mut buf = [0u16; 2];
                if let Some(m) = c
                    .encode_utf16(&mut buf)
                    .first()
                    .copied()
                    .and_then(lone_char)
                {
                    out.push(m);
                }
            }
            break;
        }
        units += w;
        out.push(c);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn lone_surrogates_round_trip_as_node_writes_them() {
        for text in [
            r#""\ud83d""#,
            r#""\udc00x""#,
            r#""a\ud83d\ud83d""#,
            r#"{"t":"\udfff\ud800"}"#,
            r#"["\\ud800","\\\ud800"]"#,
        ] {
            let v = parse(text).unwrap();
            assert_eq!(stringify(&v).unwrap(), text, "{text}");
        }
        // A valid pair stays one character.
        let v = parse(r#""\ud83d\ude00""#).unwrap();
        assert_eq!(v, JValue::Str("😀".into()));
        assert_eq!(stringify(&v).unwrap(), "\"😀\"");
        // Keys too.
        let v = parse(r#"{"\ud800":1}"#).unwrap();
        assert_eq!(stringify(&v).unwrap(), r#"{"\ud800":1}"#);
        assert_eq!(stringify_pretty(&v).unwrap(), "{\n  \"\\ud800\": 1\n}");
        // The strict parser still refuses them.
        assert!(crate::parse(r#""\ud800""#).is_err());
    }

    #[test]
    fn refusals_say_whether_js_would_throw() {
        for invalid in [
            "",
            "{",
            "[1,]",
            "\"\\u12\"",
            "\"\\x\"",
            "\u{feff}{}",
            "{} x",
            "\"\\ud800",
        ] {
            assert_eq!(parse(invalid), Err(Refusal::Invalid), "{invalid:?}");
        }
        for unsupported in [
            "1e400",
            "[-1e999]",
            "\"\u{10F800}\"",
            "\"\u{10FFFF}\"",
            r#""\udbfe\udc00""#,
            &format!("{}{}", "[".repeat(200), "]".repeat(200)),
        ] {
            assert_eq!(
                parse(unsupported),
                Err(Refusal::Unsupported),
                "{unsupported:?}"
            );
        }
        assert_eq!(parse("1e308").unwrap(), JValue::Num(1e308));
        assert_eq!(
            parse("\"\u{10F7FF}\"").unwrap(),
            JValue::Str("\u{10F7FF}".into())
        );
    }

    #[test]
    fn key_order_is_js_property_order() {
        let v = parse(
            r#"{"b":1,"10":2,"a":{"2":0,"x":0,"1":0},"1":3,"01":4,"4294967295":5,"4294967294":6}"#,
        )
        .unwrap();
        assert_eq!(
            stringify(&v).unwrap(),
            r#"{"1":3,"10":2,"4294967294":6,"b":1,"a":{"1":0,"2":0,"x":0},"01":4,"4294967295":5}"#
        );
        let mut items = vec![("z".to_string(), JValue::Null)];
        set(&mut items, "5", JValue::Num(5.0));
        set(&mut items, "a", JValue::Num(1.0));
        set(&mut items, "3", JValue::Num(3.0));
        set(&mut items, "z", JValue::Bool(true));
        let keys: Vec<_> = items.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["3", "5", "z", "a"]);
        assert_eq!(items[2].1, JValue::Bool(true));
    }

    #[test]
    fn lengths_and_slices_are_utf16() {
        let s = |t: &str| match parse(t).unwrap() {
            JValue::Str(s) => s,
            _ => unreachable!(),
        };
        assert_eq!(len("a😀"), 3);
        assert_eq!(len(&s(r#""a\ud83d""#)), 2);
        assert_eq!(slice("ab😀c", 3), s(r#""ab\ud83d""#));
        assert_eq!(slice("ab😀c", 4), "ab😀");
        assert_eq!(slice("ab😀c", 2), "ab");
        assert_eq!(slice(&s(r#""\ud83dxy""#), 2), s(r#""\ud83dx""#));
        assert_eq!(
            stringify(&JValue::Str(slice("ab😀c", 3))).unwrap(),
            r#""ab\ud83d""#
        );
    }

    #[test]
    fn many_keys_parse_in_linear_time() {
        let n = 200_000;
        let mut text = String::from("{");
        for i in 0..n {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&format!("\"k{i}\":{i}"));
        }
        text.push('}');
        let started = std::time::Instant::now();
        let JValue::Obj(items) = parse(&text).unwrap() else {
            unreachable!()
        };
        assert_eq!(items.len(), n);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
