//! The parts of Python's text and number behaviour the search rules lean on, reproduced exactly
//! (`str.strip`, `urllib.parse.quote_plus`, `round`, `f"{x:.1f}"`, `utils.human_bytes`, int/int
//! true division, `datetime.fromisoformat`, `int(str)`).
//!
//! Anything this module cannot follow exactly is reported as [`Unsupported`], never guessed.

/// Python would give an answer this side does not reproduce; the route answers its native refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported;

/// `str.isspace()` for one character (`str.strip()` strips these).
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// `text.strip()`.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

fn hex(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase())
}

/// `urllib.parse.quote_plus(text)` (UTF-8, `safe=""`): letters, digits and `_.-~` stay, a space
/// is `+`, every other byte is `%XX`.
pub fn quote_plus(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(hex(byte >> 4));
                out.push(hex(byte & 15));
            }
        }
    }
    out
}

/// `f"{value:.{digits}f}"`.
pub fn fixed(value: f64, digits: usize) -> String {
    if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        format!("{value:.digits$}")
    }
}

/// `round(value, digits)`: the correctly rounded decimal of the exact binary value (ties to
/// even), read back as a float.
pub fn round_to(value: f64, digits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.digits$}").parse().unwrap_or(value)
}

/// `utils.human_bytes(n)`.
pub fn human_bytes(bytes: i128) -> String {
    let mut n = bytes as f64;
    for unit in ["B", "KB", "MB", "GB", "TB", "PB"] {
        if n.abs() < 1024.0 {
            return if unit == "B" {
                format!("{} {unit}", n as i64)
            } else {
                format!("{} {unit}", fixed(n, 1))
            };
        }
        n /= 1024.0;
    }
    format!("{} EB", fixed(n, 1))
}

/// `numerator / denominator` of two Python ints, correctly rounded (Python's int true division),
/// for a positive denominator and a numerator under 2**64 in magnitude.
pub fn ratio(numerator: i128, denominator: u64) -> Result<f64, Unsupported> {
    let magnitude = numerator.unsigned_abs();
    if magnitude >= 1 << 64 || denominator == 0 {
        return Err(Unsupported);
    }
    if magnitude == 0 {
        return Ok(0.0);
    }
    let sign = if numerator < 0 { -1.0 } else { 1.0 };
    let d = u128::from(denominator);
    let bits_n = 128 - magnitude.leading_zeros();
    let bits_d = 128 - d.leading_zeros();
    // A quotient of 55 or 56 bits, and whether anything was left over.
    let shift = 55 + i64::from(bits_d) - i64::from(bits_n);
    let (quotient, sticky) = if shift >= 0 {
        let scaled = magnitude << u32::try_from(shift).map_err(|_| Unsupported)?;
        (scaled / d, !scaled.is_multiple_of(d))
    } else {
        let divisor = d << u32::try_from(-shift).map_err(|_| Unsupported)?;
        (magnitude / divisor, !magnitude.is_multiple_of(divisor))
    };
    let width = 128 - quotient.leading_zeros();
    let extra = width.checked_sub(53).ok_or(Unsupported)?;
    let mut mantissa = quotient >> extra;
    let low = quotient & ((1u128 << extra) - 1);
    let half = 1u128 << (extra - 1);
    if low > half || (low == half && (sticky || mantissa & 1 == 1)) {
        mantissa += 1;
    }
    let exponent = i64::from(extra) - shift;
    if !(-1000..=1000).contains(&exponent) {
        return Err(Unsupported);
    }
    let scale = f64::from_bits(u64::try_from(exponent + 1023).map_err(|_| Unsupported)? << 52);
    Ok(sign * (mantissa as f64) * scale)
}

// ---- datetime.fromisoformat -----------------------------------------------------------------

/// `_days_before_year`.
fn days_before_year(year: i64) -> i64 {
    let y = year - 1;
    y * 365 + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

fn days_before_month(year: i64, month: i64) -> i64 {
    (1..month).map(|m| days_in_month(year, m)).sum()
}

/// `date.toordinal()` of a valid date (0001-01-01 is day 1).
fn ordinal(year: i64, month: i64, day: i64) -> i64 {
    days_before_year(year) + days_before_month(year, month) + day
}

/// `_ord2ymd`.
fn from_ordinal(ord: i64) -> Option<(i64, i64, i64)> {
    if ord < 1 {
        return None;
    }
    let mut n = ord - 1;
    let n400 = n.div_euclid(146_097);
    n = n.rem_euclid(146_097);
    let n100 = n / 36_524;
    n %= 36_524;
    let n4 = n / 1461;
    n %= 1461;
    let n1 = n / 365;
    n %= 365;
    let year = n400 * 400 + n100 * 100 + n4 * 4 + n1 + 1;
    if n1 == 4 || n100 == 4 {
        // The last day of a leap year.
        return Some((year - 1, 12, 31));
    }
    let mut month = 1;
    let mut left = n;
    while month < 12 && left >= days_in_month(year, month) {
        left -= days_in_month(year, month);
        month += 1;
    }
    Some((year, month, left + 1))
}

/// An aware datetime's UTC instant: microseconds since 0001-01-01T00:00:00.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instant(pub i128);

/// `parse_digits`: exactly `count` ASCII digits at `at` (the text is followed by a NUL).
fn digits(text: &[u8], at: usize, count: usize) -> Option<i64> {
    let mut value = 0i64;
    for i in 0..count {
        let byte = byte_at(text, at.checked_add(i)?);
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(byte - b'0');
    }
    Some(value)
}

fn byte_at(text: &[u8], at: usize) -> u8 {
    text.get(at).copied().unwrap_or(0)
}

/// `_find_isoformat_datetime_separator`.
fn separator(text: &[u8]) -> Option<usize> {
    let at = |i: usize| byte_at(text, i);
    let len = text.len();
    if len == 7 {
        return Some(7);
    }
    if at(4) == b'-' {
        if at(5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && at(8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && at(10).is_ascii_digit() {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if at(4) == b'W' {
        let mut idx = 7;
        while idx < len && at(idx).is_ascii_digit() {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

/// `iso_to_ymd`: the Gregorian date of an ISO year, week and weekday.
fn iso_to_ymd(year: i64, week: i64, day: i64) -> Option<(i64, i64, i64)> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    if week <= 0 || week >= 53 {
        let mut out_of_range = true;
        if week == 53 {
            let first_weekday = (ordinal(year, 1, 1) - 1).rem_euclid(7);
            // Monday is 0 here: 0001-01-01 (ordinal 1) is a Monday.
            if first_weekday == 3 || (first_weekday == 2 && is_leap(year)) {
                out_of_range = false;
            }
        }
        if out_of_range {
            return None;
        }
    }
    if day <= 0 || day >= 8 {
        return None;
    }
    // The ordinal of the Monday of ISO week 1: the Monday on or before January 4th.
    let jan4 = ordinal(year, 1, 4);
    let monday = jan4 - (jan4 - 1).rem_euclid(7);
    from_ordinal(monday + (week - 1) * 7 + (day - 1))
}

/// `parse_isoformat_date` on `text` with the date ending at `end`.
fn parse_date(text: &[u8], end: usize) -> Option<(i64, i64, i64)> {
    let year = digits(text, 0, 4)?;
    let mut at = 4;
    let uses_separator = byte_at(text, at) == b'-';
    if uses_separator {
        at += 1;
    }
    if byte_at(text, at) == b'W' {
        at += 1;
        let week = digits(text, at, 2)?;
        at += 2;
        let day = if at < end {
            if uses_separator {
                let c = byte_at(text, at);
                at += 1;
                if c != b'-' {
                    return None;
                }
            }
            digits(text, at, 1)?
        } else {
            1
        };
        return iso_to_ymd(year, week, day);
    }
    let month = digits(text, at, 2)?;
    at += 2;
    if uses_separator {
        let c = byte_at(text, at);
        at += 1;
        if c != b'-' {
            return None;
        }
    }
    let day = digits(text, at, 2)?;
    Some((year, month, day))
}

/// `parse_hh_mm_ss_ff` of `text[start..end]` (the text is followed by a NUL): the clock, and
/// whether the character that ended it was not the end of the string.
fn parse_clock(text: &[u8], start: usize, end: usize) -> Option<((i64, i64, i64, i64), bool)> {
    let mut values = [0i64; 3];
    let mut at = start;
    let mut has_separator = true;
    for i in 0..3 {
        *values.get_mut(i)? = digits(text, at, 2)?;
        at += 2;
        let c = byte_at(text, at);
        at += 1;
        if i == 0 {
            has_separator = c == b':';
        }
        if at >= end {
            let [h, m, s] = values;
            return Some(((h, m, s, 0), c != 0));
        }
        if has_separator && c == b':' {
            continue;
        }
        if c == b'.' || c == b',' {
            break;
        }
        if has_separator {
            return None;
        }
        at -= 1;
    }
    let remaining = end.saturating_sub(at);
    let to_parse = remaining.min(6);
    let mut micro = digits(text, at, to_parse)?;
    at += to_parse;
    if to_parse < 6 && to_parse > 0 {
        let correction = [100_000, 10_000, 1000, 100, 10];
        micro *= *correction.get(to_parse - 1)?;
    }
    while byte_at(text, at).is_ascii_digit() {
        at += 1;
    }
    let [h, m, s] = values;
    Some(((h, m, s, micro), byte_at(text, at) != 0))
}

/// `datetime.fromisoformat(text)` as an instant, `Ok(None)` when Python raises or returns a
/// naive datetime (the age computation then fails and Python uses 3650 days), `Err` for text
/// this side does not follow (anything but printable ASCII).
pub fn from_iso_instant(text: &str) -> Result<Option<Instant>, Unsupported> {
    if !text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(Unsupported);
    }
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 7 {
        return Ok(None);
    }
    let Some(split) = separator(bytes) else {
        return Ok(None);
    };
    let Some((year, month, day)) = parse_date(bytes, split) else {
        return Ok(None);
    };
    let (mut hour, mut minute, mut second, mut micro) = (0, 0, 0, 0);
    let mut offset_micro: Option<i128> = None;
    if len > split {
        let start = split + 1;
        // The first `Z`, `+` or `-` of the time part, else its end.
        let mut zone = start;
        while zone < len && !matches!(byte_at(bytes, zone), b'Z' | b'+' | b'-') {
            zone += 1;
        }
        let Some((clock, trailing)) = parse_clock(bytes, start, zone) else {
            return Ok(None);
        };
        (hour, minute, second, micro) = clock;
        if zone == len {
            if trailing {
                return Ok(None);
            }
        } else if byte_at(bytes, zone) == b'Z' {
            if zone + 1 != len {
                return Ok(None);
            }
            offset_micro = Some(0);
        } else {
            let sign: i128 = if byte_at(bytes, zone) == b'-' { -1 } else { 1 };
            let Some(((h, m, s, f), trailing)) = parse_clock(bytes, zone + 1, len) else {
                return Ok(None);
            };
            if trailing {
                return Ok(None);
            }
            let total = i128::from(h * 3600 + m * 60 + s) * 1_000_000 + i128::from(f);
            if total >= 86_400 * 1_000_000 {
                return Ok(None);
            }
            offset_micro = Some(sign * total);
        }
    }
    // `check_date_args` and `check_time_args`.
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return Ok(None);
    }
    if day < 1 || day > days_in_month(year, month) {
        return Ok(None);
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Ok(None);
    }
    let Some(offset_micro) = offset_micro else {
        // Naive: the subtraction from an aware `now` raises TypeError.
        return Ok(None);
    };
    let days = i128::from(ordinal(year, month, day));
    let local = ((days * 86_400 + i128::from(hour * 3600 + minute * 60 + second)) * 1_000_000)
        + i128::from(micro);
    Ok(Some(Instant(local - offset_micro)))
}

/// Microseconds since 0001-01-01 of a Unix time in seconds (`datetime.fromtimestamp`, ties to
/// even on the microsecond).
pub fn instant_of_unix(seconds: f64) -> Result<Instant, Unsupported> {
    if !seconds.is_finite() {
        return Err(Unsupported);
    }
    let whole = seconds.floor();
    let micros = ((seconds - whole) * 1e6).round_ties_even();
    let epoch = i128::from(ordinal(1970, 1, 1)) * 86_400 * 1_000_000;
    Ok(Instant(
        epoch + (whole as i128) * 1_000_000 + micros as i128,
    ))
}

/// `discover._age_days(last_modified)` at `now`.
pub fn age_days(last_modified: &str, now: Instant) -> Result<f64, Unsupported> {
    // `last_modified.replace("Z", "+00:00")`.
    let text = last_modified.replace('Z', "+00:00");
    let Some(parsed) = from_iso_instant(&text)? else {
        return Ok(3650.0);
    };
    let seconds = ratio(now.0 - parsed.0, 1_000_000)?;
    let days = seconds / 86_400.0;
    Ok(if days > 0.0 { days } else { 0.0 })
}

// ---- int(str) -------------------------------------------------------------------------------

/// `int(text)` of a Python str, bounded to what an `i64` holds. `Ok(Err(()))` is Python's
/// ValueError, `Err` a value this side does not follow (non-ASCII text, a number past `i64`).
pub fn parse_int(text: &str) -> Result<Result<i64, ()>, Unsupported> {
    if text.chars().any(|c| !c.is_ascii() && !is_space(c)) {
        return Err(Unsupported);
    }
    let trimmed = strip(text);
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    // Digits, with single underscores between digits.
    let valid = !body.is_empty()
        && !body.starts_with('_')
        && !body.ends_with('_')
        && !body.contains("__")
        && body.bytes().all(|b| b.is_ascii_digit() || b == b'_');
    if !valid {
        return Ok(Err(()));
    }
    let digits: String = body.chars().filter(|c| *c != '_').collect();
    // Python refuses more than 4300 digits.
    if digits.len() > 4300 {
        return Ok(Err(()));
    }
    let signed = if negative {
        format!("-{digits}")
    } else {
        digits
    };
    match signed.parse::<i64>() {
        Ok(value) => Ok(Ok(value)),
        Err(_) => Err(Unsupported),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn strip_matches_python_whitespace() {
        assert_eq!(strip("\u{1c} \u{a0}x y\u{2003}\u{85}\n"), "x y");
        assert_eq!(strip("\u{200b}x"), "\u{200b}x");
    }

    #[test]
    fn quote_plus_matches_python() {
        assert_eq!(quote_plus("a b*c~d/\u{fc}%"), "a+b%2Ac~d%2F%C3%BC%25");
        assert_eq!(quote_plus(""), "");
    }

    #[test]
    fn round_and_fixed_follow_the_exact_binary_value() {
        // round(0.125, 2) == 0.12 (a tie, to even); round(0.375, 2) == 0.38;
        // round(2.675, 2) == 2.67 (the double is below the tie).
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(round_to(0.375, 2), 0.38);
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(0.25, 1), 0.2);
        assert_eq!(round_to(-0.001, 2).to_bits(), (-0.0f64).to_bits());
        assert_eq!(fixed(0.25, 1), "0.2");
        assert_eq!(fixed(0.35, 1), "0.3");
        assert_eq!(fixed(1.0e22, 1), "10000000000000000000000.0");
        assert_eq!(fixed(f64::INFINITY, 1), "inf");
        assert_eq!(fixed(f64::NAN, 1), "nan");
    }

    #[test]
    fn human_bytes_matches_python() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(human_bytes(-1024), "-1.0 KB");
        assert_eq!(human_bytes(1 << 50), "1.0 PB");
        assert_eq!(human_bytes(1 << 60), "1.0 EB");
        assert_eq!(human_bytes(1 << 61), "2.0 EB");
    }

    #[test]
    fn ratio_is_correctly_rounded() {
        assert_eq!(ratio(1, 3).unwrap(), 1.0 / 3.0);
        assert_eq!(ratio(0, 7).unwrap(), 0.0);
        assert_eq!(ratio(-5, 2).unwrap(), -2.5);
        // 2**53 + 1 over 1 rounds to even; over 10**6 it is the nearest double.
        assert_eq!(ratio((1 << 53) + 1, 1).unwrap(), 9_007_199_254_740_992.0);
        assert_eq!(ratio((1 << 53) + 3, 1).unwrap(), 9_007_199_254_740_996.0);
        assert_eq!(
            ratio(123_456_789_012_345_678, 1_000_000).unwrap(),
            123_456_789_012.345_68
        );
        assert_eq!(ratio(1, 1_000_000).unwrap(), 1e-6);
        assert!(ratio(1 << 64, 1).is_err());
    }

    #[test]
    fn ordinals_round_trip() {
        for (y, m, d) in [
            (1, 1, 1),
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 12, 31),
            (9999, 12, 31),
            (1900, 3, 1),
        ] {
            assert_eq!(
                from_ordinal(ordinal(y, m, d)),
                Some((y, m, d)),
                "{y}-{m}-{d}"
            );
        }
        assert_eq!(ordinal(1, 1, 1), 1);
        assert_eq!(ordinal(1970, 1, 1), 719_163);
        for ord in (1..3_652_059).step_by(97) {
            let (y, m, d) = from_ordinal(ord).unwrap();
            assert_eq!(ordinal(y, m, d), ord);
        }
    }

    #[test]
    fn iso_strings() {
        let now = instant_of_unix(1_800_000_000.5).unwrap();
        let micros = |text: &str| from_iso_instant(text).unwrap().map(|i| i.0);
        let utc = |s: &str| micros(s).unwrap();
        assert_eq!(
            utc("1970-01-01T00:00:00Z".replace('Z', "+00:00").as_str()),
            i128::from(ordinal(1970, 1, 1)) * 86_400_000_000
        );
        assert_eq!(
            utc("2024-05-01T12:34:56.789+00:00") - utc("2024-05-01T12:34:56+00:00"),
            789_000
        );
        assert_eq!(
            utc("2024-05-01T12:00:00+02:00"),
            utc("2024-05-01T10:00:00+00:00")
        );
        assert_eq!(
            utc("20240501T120000+0200"),
            utc("2024-05-01T10:00:00+00:00")
        );
        assert_eq!(micros("2024-05-01"), None);
        assert_eq!(micros("2024-05-01T12:00:00"), None);
        assert_eq!(micros("not a date"), None);
        assert_eq!(micros(""), None);
        assert!(from_iso_instant("2024-05-01T12:00:00\u{e9}").is_err());
        assert_eq!(age_days("", now).unwrap(), 3650.0);
        assert_eq!(age_days("2099-01-01T00:00:00Z", now).unwrap(), 0.0);
        let aged = age_days("2026-01-01T00:00:00.000Z", now).unwrap();
        assert!(aged > 0.0);
    }

    #[test]
    fn int_of_str() {
        assert_eq!(parse_int(" 12 "), Ok(Ok(12)));
        assert_eq!(parse_int("-1_000"), Ok(Ok(-1000)));
        assert_eq!(parse_int("+7"), Ok(Ok(7)));
        for bad in ["", "-", "1__0", "_1", "1_", "1.5", "0x10", "1e3", "abc"] {
            assert_eq!(parse_int(bad), Ok(Err(())), "{bad}");
        }
        assert_eq!(parse_int("\u{a0}5\u{2003}"), Ok(Ok(5)));
        assert_eq!(parse_int("\u{663}"), Err(Unsupported));
        assert_eq!(parse_int(&"9".repeat(4301)), Ok(Err(())));
        assert_eq!(parse_int("99999999999999999999"), Err(Unsupported));
    }
}
