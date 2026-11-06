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

/// `a.localeCompare(b)` in the web container: Intl's default locale there is `en-US`, whose
/// collation is CLDR root (tertiary strength, non-ignorable), here through the ICU4X collator with
/// the same tables. (A lone surrogate is compared as its mapped character.)
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    use icu_collator::{options::CollatorOptions, Collator, CollatorBorrowed};
    use std::sync::OnceLock;
    static COLLATOR: OnceLock<Option<CollatorBorrowed<'static>>> = OnceLock::new();
    let c = COLLATOR.get_or_init(|| {
        Collator::try_new(
            icu_locale::locale!("en-US").into(),
            CollatorOptions::default(),
        )
        .ok()
    });
    match c {
        Some(c) => c.compare(a, b),
        // Compiled data is built in; this cannot fail. Code-point order keeps the sort total.
        None => a.cmp(b),
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (`day` may run past the month's end, as
/// in JS `MakeDay`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + d - 1
}

/// `Date.parse(s)` for the date-time strings V8 reads (NaN otherwise): the ECMAScript Date Time
/// String Format (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`, `±YYYYYY`; `THH:mm`, `:ss`, `.s…`; `Z` or
/// `±HH:mm`), and V8's lenient forms of it: surrounding whitespace, a lower-case `t`/`z`, a space
/// for `T`, `±HHmm`; a day past the month's end rolls over and `24:00` is the next midnight. A time
/// without an offset is local time, which in the web container is UTC (no TZ is set there), so it
/// reads as UTC, like a date alone. V8's other legacy formats (`Nov 14 2023`, `2023/11/14`, ...)
/// are not read: NaN, so the note fails and is retried, never written with another time.
pub fn parse_date(s: &str) -> f64 {
    parse_date_opt(trim(s)).unwrap_or(f64::NAN)
}

fn digits(b: &[u8], at: &mut usize, n: usize) -> Option<i64> {
    let part = b.get(*at..*at + n)?;
    if !part.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *at += n;
    Some(part.iter().fold(0i64, |v, d| v * 10 + i64::from(d - b'0')))
}

fn parse_date_opt(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let year = match b.first()? {
        sign @ (b'+' | b'-') => {
            i += 1;
            let y = digits(b, &mut i, 6)?;
            if *sign == b'-' {
                if y == 0 {
                    return None;
                }
                -y
            } else {
                y
            }
        }
        _ => digits(b, &mut i, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if b.get(i) == Some(&b'-') {
        i += 1;
        month = digits(b, &mut i, 2)?;
        if b.get(i) == Some(&b'-') {
            i += 1;
            day = digits(b, &mut i, 2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (mut h, mut mi, mut sec, mut ms, mut offset) = (0, 0, 0, 0, 0i64);
    if let Some(b'T' | b't' | b' ') = b.get(i) {
        i += 1;
        h = digits(b, &mut i, 2)?;
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        mi = digits(b, &mut i, 2)?;
        if b.get(i) == Some(&b':') {
            i += 1;
            sec = digits(b, &mut i, 2)?;
            if b.get(i) == Some(&b'.') {
                i += 1;
                let start = i;
                while b.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                let frac = b.get(start..i)?;
                if frac.is_empty() {
                    return None;
                }
                let mut three = [b'0'; 3];
                for (slot, d) in three.iter_mut().zip(frac) {
                    *slot = *d;
                }
                ms = three.iter().fold(0i64, |v, d| v * 10 + i64::from(d - b'0'));
            }
        }
        match b.get(i) {
            Some(b'Z' | b'z') => i += 1,
            Some(sign @ (b'+' | b'-')) => {
                let neg = *sign == b'-';
                i += 1;
                let oh = digits(b, &mut i, 2)?;
                if b.get(i) == Some(&b':') {
                    i += 1;
                }
                let om = digits(b, &mut i, 2)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                offset = (oh * 60 + om) * 60_000 * if neg { -1 } else { 1 };
            }
            _ => {}
        }
        let midnight = h == 24 && mi == 0 && sec == 0 && ms == 0;
        if !(h < 24 || midnight) || mi > 59 || sec > 59 {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    #[allow(clippy::cast_precision_loss)]
    let t = (days * 86_400_000 + ((h * 60 + mi) * 60 + sec) * 1000 + ms - offset) as f64;
    (t.abs() <= 8.64e15).then_some(t)
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
        // Node 22 (ICU 78.3, en-US): beyond ASCII (review 2026-11-05, P2).
        let mut w = vec![
            "é", "z", "E", "e", "ß", "ss", "中", "a", "Z", "_", "1", "ä", "b",
        ];
        w.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(
            w,
            ["_", "1", "a", "ä", "b", "e", "E", "é", "ss", "ß", "z", "Z", "中"]
        );
    }

    #[test]
    fn date_parse_matches_node() {
        // Node 22, TZ=UTC: `new Date(s).toISOString()` or NaN.
        let cases = [
            ("2023-11-14", Some("2023-11-14T00:00:00.000Z")),
            ("2023-11", Some("2023-11-01T00:00:00.000Z")),
            ("2023", Some("2023-01-01T00:00:00.000Z")),
            ("+002023-11-14T00:00:00Z", Some("2023-11-14T00:00:00.000Z")),
            ("-000000-01-01T00:00:00Z", None),
            ("2023-11-14T22:13", Some("2023-11-14T22:13:00.000Z")),
            ("2023-11-14T22:13:20", Some("2023-11-14T22:13:20.000Z")),
            ("2023-11-14T22:13:20.1Z", Some("2023-11-14T22:13:20.100Z")),
            (
                "2023-11-14T22:13:20.123456Z",
                Some("2023-11-14T22:13:20.123Z"),
            ),
            (
                "2023-11-14T22:13:20.123+02:00",
                Some("2023-11-14T20:13:20.123Z"),
            ),
            ("2023-11-14T24:00:00Z", Some("2023-11-15T00:00:00.000Z")),
            ("2023-11-14T24:00:01Z", None),
            ("2023-11-14t22:13:20z", Some("2023-11-14T22:13:20.000Z")),
            ("2023-11-14 22:13:20Z", Some("2023-11-14T22:13:20.000Z")),
            ("2023-02-30T00:00:00Z", Some("2023-03-02T00:00:00.000Z")),
            ("2023-11-14T22:13:20+0200", Some("2023-11-14T20:13:20.000Z")),
            ("1700000000000", None),
            ("2023-13-01", None),
            ("2023-11-14T22:13:20.Z", None),
            ("  2023-11-14", Some("2023-11-14T00:00:00.000Z")),
            ("2023-11-14T22:13:60Z", None),
            (
                "2023-11-14T22:13:20.123-00:00",
                Some("2023-11-14T22:13:20.123Z"),
            ),
            ("2023-11-14T22:13:20+02", None),
            ("2024-02-29", Some("2024-02-29T00:00:00.000Z")),
            ("1900-02-29", Some("1900-03-01T00:00:00.000Z")),
        ];
        for (s, want) in cases {
            assert_eq!(iso(parse_date(s)).as_deref(), want, "{s}");
        }
    }
}
