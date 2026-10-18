//! `datetime.fromisoformat(text).timestamp()` (CPython 3.12) for the shapes `docker logs -t`
//! writes: `YYYY-MM-DDTHH:MM[:SS[(.|,)fraction]]` and an offset `+HH:MM`, `-HH:MM`, `+HHMM` or
//! `-HHMM` (a `Z` was already replaced by `+00:00`). Fractions past microseconds are truncated,
//! as Python truncates them. A value out of range (month 13, hour 24, year 0) is Python's
//! `ValueError`, which `_parse_ts` turns into 0.0. Any other shape is refused: Python accepts
//! many more (week dates, naive times read in the local zone), which this side does not follow.
use crate::Error;

const UNSUPPORTED: Error = Error::Unsupported("a log timestamp in a shape this side does not read");

fn digits(s: &str, n: usize) -> Result<i64, Error> {
    if s.len() != n || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UNSUPPORTED);
    }
    s.parse().map_err(|_| UNSUPPORTED)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(crate) fn fromisoformat_timestamp(text: &str) -> Result<f64, Error> {
    if !text.is_ascii() {
        return Err(UNSUPPORTED);
    }
    let (date, rest) = text.split_once('T').ok_or(UNSUPPORTED)?;
    let mut d = date.split('-');
    let (y, mo, da) = match (d.next(), d.next(), d.next(), d.next()) {
        (Some(y), Some(m), Some(dd), None) => (digits(y, 4)?, digits(m, 2)?, digits(dd, 2)?),
        _ => return Err(UNSUPPORTED),
    };
    // The offset: the last '+' or '-' in the time part.
    let at = rest.rfind(['+', '-']).ok_or(UNSUPPORTED)?;
    let (clock, offset) = rest.split_at(at);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let off = offset.get(1..).ok_or(UNSUPPORTED)?;
    let (oh, om) = match (off.get(..2), off.get(2..3), off.get(3..), off.get(2..)) {
        (Some(h), Some(":"), Some(m), _) if off.len() == 5 => (digits(h, 2)?, digits(m, 2)?),
        (Some(h), _, _, Some(m)) if off.len() == 4 => (digits(h, 2)?, digits(m, 2)?),
        _ => return Err(UNSUPPORTED),
    };
    if oh > 23 || om > 59 {
        return Err(UNSUPPORTED);
    }
    let (hms, fraction) = match clock.find(['.', ',']) {
        Some(i) => (&clock[..i], Some(&clock[i + 1..])),
        None => (clock, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, mi, s) = match parts.as_slice() {
        [h, m] if fraction.is_none() => (digits(h, 2)?, digits(m, 2)?, 0),
        [h, m, s] => (digits(h, 2)?, digits(m, 2)?, digits(s, 2)?),
        _ => return Err(UNSUPPORTED),
    };
    let micros = match fraction {
        None => 0,
        Some(f) if !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) => {
            let mut six: String = f.chars().take(6).collect();
            while six.len() < 6 {
                six.push('0');
            }
            six.parse::<i64>().map_err(|_| UNSUPPORTED)?
        }
        Some(_) => return Err(UNSUPPORTED),
    };
    // Python's ValueError (caught: 0.0).
    if y < 1
        || !(1..=12).contains(&mo)
        || da < 1
        || da > days_in_month(y, mo)
        || h > 23
        || mi > 59
        || s > 59
    {
        return Ok(0.0);
    }
    let seconds = days_from_civil(y, mo, da) * 86_400 + h * 3_600 + mi * 60 + s
        - sign * (oh * 3_600 + om * 60);
    let total = seconds * 1_000_000 + micros;
    if total.abs() >= 1 << 53 {
        return Err(UNSUPPORTED);
    }
    // An int divided by 10**6: one correctly rounded division, as timedelta.total_seconds().
    Ok(total as f64 / 1_000_000.0)
}
