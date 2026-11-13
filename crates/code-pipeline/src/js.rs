//! The few JavaScript semantics the pieces lean on, over `js_json`'s WTF-16 strings (a lone
//! surrogate held as its mapped character): `trim`, `Array.from` code points, `String(v)` in a
//! template literal and in `Array.prototype.join`, `Number.isInteger`, and `{ ...a, k: v }`.

use crate::TypeError;
use js_json::{number_to_string, to_js_string, wtf16, JValue};

/// ECMAScript WhiteSpace and LineTerminator, what `trim` strips.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `s.trim()`.
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// Joins a mapped lone high surrogate followed by a mapped lone low one into the character they
/// make, as JS sees two such units once whatever stood between them is removed.
pub(crate) fn rejoin(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending: Option<u16> = None;
    for c in s.chars() {
        let unit = wtf16::lone_unit(c);
        if let (Some(hi), Some(lo)) = (pending, unit) {
            if (0xDC00..=0xDFFF).contains(&lo) {
                let cp = 0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
                if let Some(joined) = char::from_u32(cp) {
                    out.push(joined);
                    pending = None;
                    continue;
                }
            }
        }
        if let Some(hi) = pending.take() {
            if let Some(m) = wtf16::lone_char(hi) {
                out.push(m);
            }
        }
        match unit {
            Some(u) if (0xD800..=0xDBFF).contains(&u) => pending = Some(u),
            _ => out.push(c),
        }
    }
    if let Some(m) = pending.and_then(wtf16::lone_char) {
        out.push(m);
    }
    out
}

/// A string from UTF-16 units, a lone surrogate kept as its mapped character.
pub(crate) fn from_units(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|r| match r {
            Ok(c) => c,
            Err(e) => wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{FFFD}'),
        })
        .collect()
}

/// The UTF-16 units of a WTF-16 string (a mapped lone surrogate is its one unit).
pub(crate) fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => {
                let mut buf = [0u16; 2];
                out.extend_from_slice(c.encode_utf16(&mut buf));
            }
        }
    }
    out
}

/// `${v}` in a template literal: `String(v)`.
pub(crate) fn tpl(v: &JValue) -> Result<String, TypeError> {
    to_js_string(v).map_err(|_| TypeError)
}

/// How `Array.prototype.join` writes one element: `null`/`undefined` as nothing.
pub(crate) fn join_part(v: &JValue) -> Result<String, TypeError> {
    if v.is_nullish() {
        Ok(String::new())
    } else {
        tpl(v)
    }
}

/// `Number.isInteger(v)`.
pub(crate) fn is_integer(v: &JValue) -> bool {
    v.as_num().is_some_and(|n| n.is_finite() && n.trunc() == n)
}

/// `Number.isFinite(v)`.
pub(crate) fn is_finite(v: &JValue) -> bool {
    v.as_num().is_some_and(f64::is_finite)
}

/// A number as JS writes it.
pub(crate) fn num(n: f64) -> String {
    number_to_string(n)
}

/// `v ?? null`.
pub(crate) fn or_null(v: &JValue) -> JValue {
    if v.is_nullish() {
        JValue::Null
    } else {
        v.clone()
    }
}

/// `a === s` for a string `s`.
pub(crate) fn is_str(v: &JValue, s: &str) -> bool {
    v.as_str() == Some(s)
}

/// `{ ...base, key: value }`: an existing key keeps its place.
pub(crate) fn with(base: &JValue, pairs: Vec<(&str, JValue)>) -> JValue {
    let mut items = match base {
        JValue::Obj(items) => items.clone(),
        _ => Vec::new(),
    };
    for (k, v) in pairs {
        wtf16::set(&mut items, k, v);
    }
    JValue::Obj(items)
}

/// An object literal in source order.
pub(crate) fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::obj(pairs)
}

/// A string value.
pub(crate) fn s(t: &str) -> JValue {
    JValue::Str(t.to_string())
}

/// An optional string: `None` is `null`.
pub(crate) fn opt_s(t: Option<&str>) -> JValue {
    t.map_or(JValue::Null, s)
}
