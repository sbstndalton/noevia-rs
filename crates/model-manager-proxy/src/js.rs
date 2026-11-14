//! The JavaScript semantics the proxy's lines lean on: `String.prototype.trim`, `Number(value)`,
//! `===`, `String(value)`, `Array.prototype.join`, `Math.round`, `decodeURIComponent` and
//! `for (... of value)` over JSON values. Strings are `js_json::wtf16` text (a lone surrogate is a
//! mapped character).

use crate::Thrown;
use js_json::{to_js_string, wtf16, JValue};

/// JS `WhiteSpace` and `LineTerminator` (what `trim` and `\s` match).
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `s.trim()`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `a === b` (two objects or arrays are never the same value here: they come from different
/// parses).
pub fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `String(v)`; a TypeError for an object with an own `toString`.
pub fn string(v: &JValue) -> Result<String, Thrown> {
    to_js_string(v).map_err(|_| Thrown::type_error())
}

/// `String(v || '')`.
pub fn string_or_empty(v: &JValue) -> Result<String, Thrown> {
    if v.truthy() {
        string(v)
    } else {
        Ok(String::new())
    }
}

/// `list.join(', ')`: `null` and `undefined` are empty.
pub fn join(list: &[JValue], sep: &str) -> Result<String, Thrown> {
    let mut parts = Vec::with_capacity(list.len());
    for v in list {
        parts.push(if v.is_nullish() {
            String::new()
        } else {
            string(v)?
        });
    }
    Ok(parts.join(sep))
}

/// `value.prop` for a JSON value: a TypeError on `null`/`undefined`, `undefined` for a primitive
/// or array without it.
pub fn prop<'a>(value: &'a JValue, key: &str) -> Result<&'a JValue, Thrown> {
    if value.is_nullish() {
        return Err(Thrown::type_error());
    }
    Ok(value.get(key))
}

/// `for (const x of value)`: an array's items, a string's code points (a lone surrogate is one),
/// anything else a TypeError.
pub fn iterate(value: &JValue) -> Result<Vec<JValue>, Thrown> {
    match value {
        JValue::Arr(items) => Ok(items.clone()),
        JValue::Str(s) => Ok(s.chars().map(|c| JValue::Str(c.to_string())).collect()),
        _ => Err(Thrown::type_error()),
    }
}

/// `Object.keys(value).length` for a truthy value.
pub fn key_count(value: &JValue) -> usize {
    match value {
        JValue::Obj(items) => items.len(),
        JValue::Arr(items) => items.len(),
        JValue::Str(s) => wtf16::len(s),
        _ => 0,
    }
}

/// `Math.round(x)`: the nearest integer, a tie towards +∞.
pub fn round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let floor = x.floor();
    let r = if x - floor >= 0.5 { floor + 1.0 } else { floor };
    if r == 0.0 && x < 0.0 {
        -0.0
    } else {
        r
    }
}

/// JS `ToNumber(v)` for a JSON value (an array through its string, an object NaN, or a TypeError
/// when it has an own `toString`).
pub fn to_number(v: &JValue) -> Result<f64, Thrown> {
    Ok(match v {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Num(n) => *n,
        JValue::Str(s) => string_to_number(s),
        JValue::Arr(_) => string_to_number(&string(v)?),
        JValue::Obj(items) => {
            if items.iter().any(|(k, _)| k == "toString") {
                return Err(Thrown::type_error());
            }
            f64::NAN
        }
    })
}

/// JS `StringToNumber`.
pub fn string_to_number(raw: &str) -> f64 {
    let s = trim(raw);
    if s.is_empty() {
        return 0.0;
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = s.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return radix_value(digits, radix);
        }
    }
    // StrDecimalLiteral: sign? (digits [. digits?] | . digits) ([eE] sign? digits)?
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let count = |from: usize| {
        b.get(from..)
            .map_or(0, |r| r.iter().take_while(|c| c.is_ascii_digit()).count())
    };
    let int = count(i);
    i += int;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac = count(i);
        i += frac;
    }
    if int + frac == 0 {
        return f64::NAN;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = count(i);
        if exp == 0 {
            return f64::NAN;
        }
        i += exp;
    }
    if i != b.len() {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// The value of `digits` (hex, octal or binary, already checked) rounded once to the nearest `f64`
/// (ties to even), as `Number("0x…")` rounds the exact integer; past 2^1024 it is infinity.
fn radix_value(digits: &str, radix: u32) -> f64 {
    let width = match radix {
        16 => 4,
        8 => 3,
        _ => 1,
    };
    let mut bits: Vec<bool> = Vec::with_capacity(digits.len() * width);
    for c in digits.chars() {
        let d = c.to_digit(radix).unwrap_or(0);
        for i in (0..width).rev() {
            bits.push(d >> i & 1 == 1);
        }
    }
    let first = bits.iter().position(|b| *b).unwrap_or(bits.len());
    let bits = bits.get(first..).unwrap_or(&[]);
    let value = |b: &[bool]| b.iter().fold(0u64, |acc, x| acc << 1 | u64::from(*x));
    if bits.len() <= 53 {
        return value(bits) as f64;
    }
    let (head, rest) = bits.split_at(53);
    let mut mantissa = value(head);
    let guard = rest.first().copied().unwrap_or(false);
    let sticky = rest.iter().skip(1).any(|b| *b);
    if guard && (sticky || mantissa & 1 == 1) {
        mantissa += 1;
    }
    let exp = i32::try_from(rest.len()).unwrap_or(i32::MAX);
    // mantissa <= 2^53 is exact in an f64; scaling by a power of two is exact until it overflows.
    let mut out = mantissa as f64;
    let mut left = exp;
    while left > 0 && out.is_finite() {
        let step = left.min(1000);
        out *= 2f64.powi(step);
        left -= step;
    }
    out
}

fn hex(b: Option<&u8>) -> Option<u8> {
    let b = *b?;
    char::from(b)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

fn hex_byte(b: &[u8], at: usize) -> Option<u8> {
    Some(hex(b.get(at + 1))? << 4 | hex(b.get(at + 2))?)
}

/// `decodeURIComponent(s)`; `None` where it throws a URIError (a `%` without two hex digits, a
/// lead byte that is a continuation or longer than four bytes, a missing or wrong continuation, or
/// octets that are not one UTF-8 code point: overlong forms, surrogates, past U+10FFFF).
pub fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c != b'%' {
            out.push(c);
            i += 1;
            continue;
        }
        let lead = hex_byte(b, i)?;
        i += 3;
        if lead < 0x80 {
            out.push(lead);
            continue;
        }
        let n = lead.leading_ones();
        if n == 1 || n > 4 {
            return None;
        }
        let mut octets = vec![lead];
        for _ in 1..n {
            if b.get(i) != Some(&b'%') {
                return None;
            }
            let next = hex_byte(b, i)?;
            if next & 0xC0 != 0x80 {
                return None;
            }
            octets.push(next);
            i += 3;
        }
        std::str::from_utf8(&octets).ok()?;
        out.extend_from_slice(&octets);
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn decodes_as_decode_uri_component() {
        assert_eq!(decode_uri_component("a%2Fb").as_deref(), Some("a/b"));
        assert_eq!(decode_uri_component("%E2%9C%93").as_deref(), Some("✓"));
        assert_eq!(decode_uri_component("%f0%9f%98%80").as_deref(), Some("😀"));
        for bad in [
            "%", "%2", "%zz", "%C3", "%80", "%C0%AF", "%ED%A0%80", "%F4%90%80%80", "%F8%80", "%C3%28",
        ] {
            assert_eq!(decode_uri_component(bad), None, "{bad}");
        }
    }

    #[test]
    fn numbers_and_rounding() {
        assert_eq!(string_to_number(" 0x10 "), 16.0);
        assert!(string_to_number("-0x10").is_nan());
        assert_eq!(string_to_number("1."), 1.0);
        assert_eq!(string_to_number(".5"), 0.5);
        assert!(string_to_number("1_0").is_nan());
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(0.49999999999999994), 0.0);
    }
}
