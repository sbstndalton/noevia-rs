//! The few JavaScript semantics the executors depend on, over [`js_json::wtf16`] strings (a lone
//! surrogate is held as one mapped character): whitespace, `trim`, UTF-16 `length`/`slice`,
//! `Date#toISOString`, `ToNumber`, strict equality and `localeCompare`.

use js_json::wtf16;
use js_json::JValue;
use std::cmp::Ordering;

/// `/\s/` and the `trim` set (ECMAScript WhiteSpace + LineTerminator).
pub fn is_space(c: char) -> bool {
    u16::try_from(u32::from(c)).is_ok_and(mcp_frame::is_js_space)
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.length`.
pub fn len(s: &str) -> usize {
    wtf16::len(s)
}

/// `s.slice(0, n)`.
pub fn slice_to(s: &str, n: usize) -> String {
    wtf16::slice(s, n)
}

/// The string as UTF-16 code units (a mapped lone surrogate as its unit).
pub fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u16; 2];
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// Code units back to a string, a lone surrogate mapped.
pub fn from_units(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => out.push(c),
            Err(e) => {
                if let Some(c) = wtf16::lone_char(e.unpaired_surrogate()) {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// `s.slice(from)` by code units.
pub fn slice_from(s: &str, from: usize) -> String {
    let u = units(s);
    from_units(u.get(from.min(u.len())..).unwrap_or(&[]))
}

/// What Node's `Buffer.from(s)` / `hash.update(s)` encode: UTF-8, a lone surrogate as U+FFFD.
pub fn utf8(s: &str) -> Vec<u8> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(if wtf16::lone_unit(c).is_some() {
            '\u{FFFD}'
        } else {
            c
        });
    }
    out.into_bytes()
}

/// `Buffer.byteLength(s)`.
pub fn byte_len(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                3
            } else {
                c.len_utf8()
            }
        })
        .sum()
}

/// `new Date(ms).toISOString()`; `None` where it throws (a time outside the Date range).
pub fn iso(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let t = ms.trunc();
    if t.abs() > 8.64e15 {
        return None;
    }
    // Exact: |t| <= 8.64e15 < 2^53.
    #[allow(clippy::cast_possible_truncation)]
    let t = t as i64;
    let day = t.div_euclid(86_400_000);
    let in_day = t.rem_euclid(86_400_000);
    let (y, m, d) = civil(day);
    let (h, mi, s, milli) = (
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000,
    );
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    Some(format!(
        "{year}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z"
    ))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `Number(v)` for the values chat data holds. (Arrays and objects are NaN here; JS would coerce
/// `[]` to 0 and `[5]` to 5. Chat lists never hold them in these fields.)
pub fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Str(s) => string_to_number(s),
        JValue::Undefined | JValue::Arr(_) | JValue::Obj(_) => f64::NAN,
    }
}

fn string_to_number(s: &str) -> f64 {
    let t = trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |p: &str, r: u32| {
        t.strip_prefix(p)
            .or_else(|| t.strip_prefix(&p.to_uppercase()))
            .map(|d| {
                if d.is_empty() || !d.chars().all(|c| c.is_digit(r)) {
                    f64::NAN
                } else {
                    d.chars()
                        .filter_map(|c| c.to_digit(r))
                        .fold(0.0, |a, x| a * f64::from(r) + f64::from(x))
                }
            })
    };
    if let Some(n) = radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
    {
        return n;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if t.chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
    {
        t.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `a === b` for JSON values: objects and arrays are never equal (two reads are two objects).
pub fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        #[allow(clippy::float_cmp)]
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `a == null`.
pub fn loose_null(v: &JValue) -> bool {
    v.is_nullish()
}

/// `a.localeCompare(b)` (V8/ICU root collation, non-ignorable). Exact for ASCII text, which is
/// what chat ids are made of: punctuation and symbols in the root order, then digits, then
/// letters, case-insensitive at the first level and lowercase first at the third. Other characters
/// sort after the letters by code point (an approximation of the full table).
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let pa: Vec<u32> = a.chars().map(primary).collect();
    let pb: Vec<u32> = b.chars().map(primary).collect();
    pa.cmp(&pb).then_with(|| {
        let ta: Vec<u8> = a.chars().map(tertiary).collect();
        let tb: Vec<u8> = b.chars().map(tertiary).collect();
        ta.cmp(&tb)
    })
}

/// CLDR root order of the ASCII punctuation and symbols (whitespace first).
const ROOT_PUNCT: &str = "\t\n\u{b}\u{c}\r _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> u32 {
    if let Some(i) = ROOT_PUNCT.chars().position(|p| p == c) {
        return 1 + u32::try_from(i).unwrap_or(0);
    }
    if c.is_ascii_digit() {
        return 100 + u32::from(c) - u32::from('0');
    }
    if c.is_ascii_alphabetic() {
        return 200 + u32::from(c.to_ascii_lowercase()) - u32::from('a');
    }
    if u32::from(c) < 0x20 || c == '\u{7f}' {
        // Controls are ignorable in ICU; weight them lowest here.
        return 0;
    }
    0x1000 + u32::from(c)
}

fn tertiary(c: char) -> u8 {
    u8::from(c.is_ascii_uppercase())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn iso_matches_date() {
        assert_eq!(iso(0.0), Some("1970-01-01T00:00:00.000Z".into()));
        assert_eq!(
            iso(1_700_000_000_123.9),
            Some("2023-11-14T22:13:20.123Z".into())
        );
        assert_eq!(iso(8.64e15), Some("+275760-09-13T00:00:00.000Z".into()));
        assert_eq!(iso(-1.0), Some("1969-12-31T23:59:59.999Z".into()));
        assert_eq!(
            iso(-62_198_755_200_000.0),
            Some("-000001-01-01T00:00:00.000Z".into())
        );
        assert_eq!(iso(8.64e15 + 1.0), None);
    }

    #[test]
    fn locale_order() {
        let mut v = vec!["b", "B", "a", "_x", "-x", "1", "A", "a1", "a-"];
        v.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(v, ["_x", "-x", "1", "a", "A", "a-", "a1", "b", "B"]);
    }
}
