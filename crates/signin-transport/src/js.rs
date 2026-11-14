//! The JavaScript coercions the transport reads its inputs with.

use js_json::JValue;

/// JS `WhiteSpace` and `LineTerminator`, what `String.prototype.trim` and `Number()` strip.
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `Number(text)` for a string.
pub fn string_to_number(text: &str) -> f64 {
    let s = text.trim_matches(is_js_space);
    if s.is_empty() {
        return 0.0;
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
            return digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let (sign, body) = match s.as_bytes().first() {
        Some(b'+') => (1.0, s.get(1..).unwrap_or("")),
        Some(b'-') => (-1.0, s.get(1..).unwrap_or("")),
        _ => (1.0, s),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !decimal_literal(body) {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |v| sign * v)
}

/// `StrUnsignedDecimalLiteral` without `Infinity`: digits [. digits] | . digits, then an exponent.
fn decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    let int = digits(&mut i);
    let mut frac = false;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac = digits(&mut i);
    }
    if !int && !frac {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

/// `Number(value)` for a value that came out of `JSON.parse`.
pub fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Num(n) => *n,
        JValue::Str(s) => string_to_number(s),
        JValue::Arr(_) => js_json::to_js_string(v).map_or(f64::NAN, |s| string_to_number(&s)),
        JValue::Obj(_) => f64::NAN,
    }
}

/// `String(value)`; the values a transport reads never fail to coerce, the rest read as `""`.
pub fn to_string(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `value || fallback` as a string: `String(value)` when truthy.
pub fn or_str(v: &JValue, fallback: &str) -> String {
    if v.truthy() {
        to_string(v)
    } else {
        fallback.to_string()
    }
}

const DAY_MS: f64 = 86_400_000.0;

/// `new Date(ms).toISOString()`, or `None` where JS throws (an invalid date: not finite, or more
/// than 8.64e15 ms from the epoch).
pub fn iso_string(ms: f64) -> Option<String> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return None;
    }
    let t = ms.trunc();
    let days = (t / DAY_MS).floor();
    let in_day = t - days * DAY_MS;
    // Both are exact integers in range here.
    #[allow(clippy::cast_possible_truncation)]
    let (days, in_day) = (days as i64, in_day as i64);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss, ms) = (
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
        "{year}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z"
    ))
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_as_js() {
        assert_eq!(string_to_number(" 12 "), 12.0);
        assert_eq!(string_to_number(""), 0.0);
        assert_eq!(string_to_number("0x1f"), 31.0);
        assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
        assert!(string_to_number("inf").is_nan());
        assert!(string_to_number("1e").is_nan());
        assert!(string_to_number("-0x1").is_nan());
        assert_eq!(string_to_number(".5"), 0.5);
        assert_eq!(string_to_number("5."), 5.0);
        assert_eq!(to_number(&JValue::Arr(vec![JValue::Num(7.0)])), 7.0);
        assert_eq!(to_number(&JValue::Bool(true)), 1.0);
    }

    #[test]
    fn iso_as_js() {
        assert_eq!(iso_string(0.0).as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert_eq!(
            iso_string(-1.0).as_deref(),
            Some("1969-12-31T23:59:59.999Z")
        );
        assert_eq!(
            iso_string(8.64e15).as_deref(),
            Some("+275760-09-13T00:00:00.000Z")
        );
        assert_eq!(
            iso_string(-8.64e15).as_deref(),
            Some("-271821-04-20T00:00:00.000Z")
        );
        assert_eq!(
            iso_string(253_402_300_800_000.0).as_deref(),
            Some("+010000-01-01T00:00:00.000Z")
        );
        assert_eq!(iso_string(8.64e15 + 1.0), None);
    }
}
