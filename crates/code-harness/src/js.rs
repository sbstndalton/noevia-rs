//! The few JavaScript semantics both modules lean on, over `js_json`'s WTF-16 strings (a lone
//! surrogate held as its mapped character): UTF-16 units, `trim`, `Number(string)`, `String(v)`,
//! property reads that throw on `null`/`undefined`, and the abstract `<` against a number.

use crate::Thrown;
use js_json::{wtf16, JValue};

/// The UTF-16 code units of a WTF-16 string (a mapped lone surrogate is its one unit).
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

/// ECMAScript WhiteSpace and LineTerminator, what `trim` and `Number()` strip.
fn is_js_space(c: char) -> bool {
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
pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// `Buffer.byteLength(s)`: UTF-8 bytes, a lone surrogate written as the three of U+FFFD.
pub(crate) fn byte_length(s: &str) -> u64 {
    s.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                3
            } else {
                c.len_utf8() as u64
            }
        })
        .sum()
}

/// `Number(s)` for a string: ECMAScript StringToNumber (whitespace trimmed, empty is 0,
/// `Infinity`, `0x`/`0o`/`0b` literals of any length correctly rounded, any other text that is not
/// a decimal literal NaN). After server-stats' `string_to_number`, over WTF-16 text.
pub(crate) fn string_to_number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    if !t.is_ascii() {
        return f64::NAN;
    }
    let t = t.as_bytes();
    if let (Some(b'0'), Some(p)) = (t.first(), t.get(1)) {
        let radix = match p {
            b'x' | b'X' => Some(16),
            b'o' | b'O' => Some(8),
            b'b' | b'B' => Some(2),
            _ => None,
        };
        if let Some(radix) = radix {
            return radix_value(t.get(2..).unwrap_or(&[]), radix);
        }
    }
    let (neg, body) = match t.first() {
        Some(b'+') => (false, t.get(1..).unwrap_or(&[])),
        Some(b'-') => (true, t.get(1..).unwrap_or(&[])),
        _ => (false, t),
    };
    if body == b"Infinity" {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if !decimal_literal(body) {
        return f64::NAN;
    }
    let text = std::str::from_utf8(body).unwrap_or("x");
    match text.parse::<f64>() {
        Ok(n) if neg => -n,
        Ok(n) => n,
        Err(_) => f64::NAN,
    }
}

/// StrUnsignedDecimalLiteral without `Infinity`: digits with an optional fraction (either side may
/// be empty, not both) and an optional exponent.
fn decimal_literal(b: &[u8]) -> bool {
    let digits = |s: &[u8], mut i: usize| {
        let start = i;
        while s.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        (i, i - start)
    };
    let (mut i, int) = digits(b, 0);
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        let (j, n) = digits(b, i + 1);
        i = j;
        frac = n;
    }
    if int == 0 && frac == 0 {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let (j, n) = digits(b, i);
        if n == 0 {
            return false;
        }
        i = j;
    }
    i == b.len()
}

/// The value of a `0x`/`0o`/`0b` literal's digits, rounded to nearest, ties to even.
fn radix_value(digits: &[u8], radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    let width = match radix {
        16 => 4,
        8 => 3,
        _ => 1,
    };
    let mut bits: Vec<bool> = Vec::with_capacity(digits.len() * width);
    for &d in digits {
        let Some(v) = char::from(d).to_digit(radix) else {
            return f64::NAN;
        };
        for k in (0..width).rev() {
            bits.push((v >> k) & 1 == 1);
        }
    }
    let first = bits.iter().position(|&b| b).unwrap_or(bits.len());
    let bits = bits.get(first..).unwrap_or(&[]);
    if bits.len() <= 64 {
        let n = bits.iter().fold(0u64, |n, &b| (n << 1) | u64::from(b));
        return n as f64;
    }
    // The top 64 bits, with every lower set bit folded into the last one (a sticky bit below the
    // rounding position), round to the same double as the whole number.
    let (top, rest) = bits.split_at(64);
    let mut n = top.iter().fold(0u64, |n, &b| (n << 1) | u64::from(b));
    if rest.iter().any(|&b| b) {
        n |= 1;
    }
    let shift = i32::try_from(rest.len()).unwrap_or(i32::MAX);
    (n as f64) * 2f64.powi(shift)
}

/// V8's TypeError for a value with no primitive form.
pub(crate) fn no_primitive() -> Thrown {
    Thrown::type_error("Cannot convert object to primitive value")
}

/// `String(v)` (or a template substitution): a TypeError for an object with its own `toString`.
pub(crate) fn to_string(v: &JValue) -> Result<String, Thrown> {
    js_json::to_js_string(v).map_err(|_| no_primitive())
}

/// `String(v)` as if every own `toString` key were absent (difference D2): objects are
/// `[object Object]`, arrays join their elements.
pub(crate) fn to_string_plain(v: &JValue) -> String {
    match v {
        JValue::Obj(_) => "[object Object]".into(),
        JValue::Arr(items) => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !item.is_nullish() {
                    out.push_str(&to_string_plain(item));
                }
            }
            out
        }
        other => js_json::to_js_string(other).unwrap_or_default(),
    }
}

/// `v.key`, which throws for `null` and `undefined` (any other primitive reads `undefined`).
pub(crate) fn get<'a>(v: &'a JValue, key: &str) -> Result<&'a JValue, Thrown> {
    match v {
        JValue::Undefined | JValue::Null => Err(Thrown::type_error(format!(
            "Cannot read properties of {} (reading '{key}')",
            if matches!(v, JValue::Null) {
                "null"
            } else {
                "undefined"
            }
        ))),
        other => Ok(other.get(key)),
    }
}

/// `value && typeof value === 'object'`: a plain object or an array.
pub(crate) fn object_like(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

/// `Number.isInteger(n)`.
pub(crate) fn is_integer(n: f64) -> bool {
    n.is_finite() && n.trunc() == n
}

/// `v < 1` for a value that is neither `null` nor `undefined`, by the abstract relational
/// comparison (ToPrimitive with hint number, then ToNumber), with D2's reading of an own
/// `toString`.
pub(crate) fn less_than_one(v: &JValue) -> bool {
    let n = match v {
        JValue::Num(n) => *n,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Str(s) => string_to_number(s),
        JValue::Arr(_) | JValue::Obj(_) => string_to_number(&to_string_plain(v)),
        JValue::Null => 0.0,
        JValue::Undefined => f64::NAN,
    };
    n < 1.0
}

/// `s.slice(0, n)` over UTF-16 units.
pub(crate) fn slice(s: &str, n: usize) -> String {
    wtf16::slice(s, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_of_text() {
        for (s, n) in [
            ("", 0.0),
            (" \u{feff}12\u{3000}", 12.0),
            ("0x1f", 31.0),
            ("0b101", 5.0),
            ("0o17", 15.0),
            ("-3", -3.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e3", 1000.0),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
        ] {
            assert_eq!(string_to_number(s), n, "{s:?}");
        }
        for s in [
            "abc", ".", "1e", "-0x1", "0x", "infinity", "NaN", "\u{85}1", "1_0", "inf",
        ] {
            assert!(string_to_number(s).is_nan(), "{s:?}");
        }
    }

    #[test]
    fn units_keep_lone_surrogates() {
        let lone = wtf16::lone_char(0xD800)
            .map(String::from)
            .unwrap_or_default();
        assert_eq!(
            units(&format!("a{lone}\u{1F600}")),
            [0x61, 0xD800, 0xD83D, 0xDE00]
        );
        assert_eq!(byte_length(&lone), 3);
        assert_eq!(byte_length("\u{1F600}é"), 6);
    }
}
