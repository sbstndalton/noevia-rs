//! noevia-core's built-in chat tool `get_current_time` (the branch of `toolboxes.cjs` runToolCall)
//! and `current-time-part.cjs` calendarFacts/factsLine, replayed byte for byte on recorded Node 22
//! output (tests/fixtures/current-time-node.v1.json).
//!
//! The tool answers `Current time: <time>[ (<zone>)] | ISO: <toISOString> | <factsLine>`. With a
//! `timezone` argument (a non-empty string) the time is `Intl.DateTimeFormat('en-GB', { timeZone,
//! dateStyle: 'full', timeStyle: 'long' })`; without one it is `Date#toString` in the server's own
//! zone, and the facts are for that zone. A zone Intl refuses is the failure text
//! `ERROR: unknown IANA timezone "<the zone, first 64 UTF-16 units>"`.
//!
//! Zones are looked up as Intl does: an ID of ICU's (ASCII case-insensitively) or a UTC offset
//! (`±HH`, `±HHMM`, `±HH:MM`, hours to 23). The IDs Node accepts and the zone names it prints
//! (en-GB short such as `CET`, `GMT`, `GMT+9`, `GMT-3:30`; en-US long for `Date#toString`, such as
//! `Central European Standard Time`) come from the recorded table in `zones.rs`; the offsets come
//! from chrono-tz. Named differences, each pinned by tests/fixtures.rs (the replayed calls that
//! differ and the zone days whose offsets differ are listed there, so a new one fails the test):
//! - chrono-tz's IANA release (2025b) is older than Node's ICU data (2026c when recorded), and the
//!   difference is live: Africa/Casablanca and Africa/El_Aaiun are at +00 in Node from
//!   2026-09-20T01:00Z (+01 here, outside Ramadan), and America/Vancouver, America/Edmonton and
//!   their links (America/Yellowknife, Canada/Pacific, Canada/Mountain) keep daylight time in Node
//!   after 2026-11-01 (standard time here). The time, the zone name and near midnight the facts
//!   differ with the offset. Europe/Chisinau (and Europe/Tiraspol) differ for an hour on its four
//!   transition days of 2026-2027. The test compares every zone at 00:00 UTC on every day of 2026
//!   and 2027.
//! - The `SystemV/*` IDs Node accepts have no chrono-tz zone: this port refuses them (as unknown).
//!   Every chrono-tz zone is one Node accepts. Legacy IDs that are not chrono-tz zones (`PST`,
//!   `US/Pacific-New`) use their ICU canonical zone's rules.
//! - Names are the ones recorded for 2026. In history (an offset none of the zone's 2026 ones, or
//!   a metazone that did not yet apply) and past ICU's metazone data (far-future years) ICU may
//!   print a different abbreviation or the GMT form; `Date#toString`'s daylight name for a zone
//!   without daylight time in 2026 (Moscow in 1995) is its standard name here.
//! - chrono-tz's transitions end in 2099; from 2100 the offset is taken at the equivalent date in
//!   2072-2099 (same leap-ness and weekdays), as ICU keeps applying the zone's rules.
//! - An ID Node accepts that was not among the recorded candidates (every IANA ID, chrono-tz's list
//!   and ICU's legacy three-letter and SystemV IDs) is refused here.
//! - A zone cut inside a surrogate pair at unit 64 keeps a lone surrogate in Node; here it is
//!   dropped (a recorded case).
//!
//! Node's quirks are kept: years before 1 print as era years (0 as 1, -1 as 2), the facts read the
//! years 0-99 as 1900-1999 (`Date.UTC`), `Date#toString` writes -0001, and its zone name outside
//! 1970-2038 is the one for V8's equivalent year. Dates are proleptic Gregorian, as in Node.

use chrono::{Offset, TimeZone};
use chrono_tz::Tz;
use js_json::JValue;

mod zones;

#[rustfmt::skip]
const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
#[rustfmt::skip]
const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const DAY_MS: i64 = 86_400_000;

/// `get_current_time` with the tool's parsed `args` object at `now_ms` (`new Date()`'s time value)
/// on a server whose zone is `server_zone`: `Intl.DateTimeFormat().resolvedOptions().timeZone` of
/// the Node process (an ICU ID such as `UTC`, `Europe/Berlin`, `Asia/Calcutta`), or `""` when ICU
/// resolved none (the process zone is then GMT+00:00 and the facts are for UTC, as
/// `serverTimeZone()` falls back). `Ok` is the tool text, `Err` the text returned with the failure
/// flag. A `now_ms` that is not a valid time value makes Node throw; here it is `Err` too.
pub fn run(args: &JValue, now_ms: f64, server_zone: &str) -> Result<String, String> {
    let tz = args.get("timezone").as_str().filter(|s| !s.is_empty());
    let unknown = |tz: &str| {
        format!(
            "ERROR: unknown IANA timezone \"{}\"",
            js_json::js_slice(tz, 64)
        )
    };
    let Some(t) = time_clip(now_ms) else {
        return Err(tz.map_or_else(|| "ERROR: invalid time value".to_string(), unknown));
    };
    let (formatted, facts_zone) = match tz {
        Some(name) => {
            let zone = lookup(name).ok_or_else(|| unknown(name))?;
            let local =
                Local::at(t, zone).ok_or_else(|| "ERROR: invalid time value".to_string())?;
            let text = format!(
                "{}, {} {} {} at {:02}:{:02}:{:02} {}",
                weekday(local.day),
                local.d,
                month(local.m),
                intl_year(local.y),
                local.hour,
                local.minute,
                local.second,
                short_name(zone, local.offset_sec)
            );
            (text, zone)
        }
        None => {
            let zone = lookup(server_zone).filter(|z| matches!(z, Zone::Named(..)));
            let local = match zone {
                Some(z) => Local::at(t, z),
                None => Local::at(t, Zone::Offset(0)),
            }
            .ok_or_else(|| "ERROR: invalid time value".to_string())?;
            let long = match zone {
                Some(z) => {
                    long_name(z, t).ok_or_else(|| "ERROR: invalid time value".to_string())?
                }
                None => "GMT+00:00".to_string(),
            };
            let off = local.offset_sec / 60;
            // V8's "%04d", or "%05d" for a negative year (-0001).
            let year = if local.y < 0 {
                format!("{:05}", local.y)
            } else {
                format!("{:04}", local.y)
            };
            let text = format!(
                "{} {} {:02} {year} {:02}:{:02}:{:02} GMT{}{:02}{:02} ({long})",
                weekday(local.day).get(..3).unwrap_or(""),
                month(local.m).get(..3).unwrap_or(""),
                local.d,
                local.hour,
                local.minute,
                local.second,
                if off < 0 { '-' } else { '+' },
                off.abs() / 60,
                off.abs() % 60
            );
            (text, zone.unwrap_or(Zone::Offset(0)))
        }
    };
    let facts = Local::at(t, facts_zone)
        .map(|l| facts_of(intl_year(l.y), l.m, l.d))
        .ok_or_else(|| "ERROR: invalid time value".to_string())?;
    let paren = tz.map(|z| format!(" ({z})")).unwrap_or_default();
    Ok(format!(
        "Current time: {formatted}{paren} | ISO: {} | {}",
        iso_string(t),
        facts_line(&facts)
    ))
}

/// `calendarFacts(now, timeZone)`, from the calendar date `now_ms` has in `time_zone` (looked up as
/// Intl does). `None` where Node throws: an unknown zone or an invalid time value.
pub fn calendar_facts(now_ms: f64, time_zone: &str) -> Option<CalendarFacts> {
    let local = Local::at(time_clip(now_ms)?, lookup(time_zone)?)?;
    Some(facts_of(intl_year(local.y), local.m, local.d))
}

/// The UTC offset in minutes of `time_zone` at `now_ms`, if this port knows the zone.
pub fn utc_offset_minutes(time_zone: &str, now_ms: f64) -> Option<i64> {
    Some(offset_sec(time_clip(now_ms)?, lookup(time_zone)?)? / 60)
}

/// Where this port's answer was proven equal to Node's: from 2026-01-01T00:00Z to the last compared
/// midnight, 2027-12-31T00:00Z (the recording's daily span).
const VERIFIED_FROM_MS: i64 = 1_767_225_600_000;
const VERIFIED_TO_MS: i64 = 1_830_211_200_000;

/// Zone IDs (lower-cased, sorted) whose offsets or transition instants in the verified span differ
/// between chrono-tz and the recorded Node data (tests/fixtures.rs pins this list to the ones the
/// recording shows; see the crate documentation).
const DIVERGENT: &[&str] = &[
    "africa/casablanca",
    "africa/el_aaiun",
    "america/edmonton",
    "america/vancouver",
    "america/yellowknife",
    "canada/mountain",
    "canada/pacific",
    "europe/chisinau",
    "europe/tiraspol",
];

/// Whether [`run`] at `now_ms` with `zone` (a `timezone` argument, or the server zone of a call
/// without one) gives the text Node gave in the recording, as far as the recording can show: `now_ms`
/// lies in 2026-01-01T00:00Z..=2027-12-31T00:00Z, and `zone` is a UTC offset or an ID this port knows
/// whose offsets at every midnight and whose transition instants in that span equal Node's. A caller
/// that must answer as Node does hands every other call to Node. Not covered: a zone whose offset
/// changes twice between two midnights and back, and zone-name changes in 2027 (the names are the
/// ones recorded for 2026, by offset).
pub fn verified(zone: &str, now_ms: f64) -> bool {
    let Some(t) = time_clip(now_ms) else {
        return false;
    };
    if !(VERIFIED_FROM_MS..=VERIFIED_TO_MS).contains(&t) {
        return false;
    }
    match lookup(zone) {
        Some(Zone::Offset(_)) => true,
        Some(Zone::Named(..)) => DIVERGENT
            .binary_search(&zone.to_ascii_lowercase().as_str())
            .is_err(),
        None => false,
    }
}

/// The server zone [`run`] needs for a call without a `timezone` argument, from the process's `TZ`
/// environment value, when this port can be sure it is the one Node's ICU uses: an IANA zone ID
/// spelled exactly as tzdb spells it (no leading `:`, no POSIX rule string, no offset) that this port
/// knows. `None` otherwise (also for `TZ` unset, where ICU reads the system's zone file): a caller
/// that must answer as Node does hands such calls to Node.
pub fn server_zone_from_tz(tz: Option<&str>) -> Option<&str> {
    let tz = tz?;
    let exact = tz.parse::<Tz>().is_ok_and(|z| z.name() == tz);
    (exact && matches!(lookup(tz), Some(Zone::Named(..)))).then_some(tz)
}

/// The IDs [`verified`] leaves to Node, for the tests.
#[doc(hidden)]
pub fn divergent_ids() -> &'static [&'static str] {
    DIVERGENT
}

/// What `calendarFacts` returns (ISO 8601 weeks: Monday to Sunday, week 1 holds the first Thursday).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarFacts {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    /// 1 Monday .. 7 Sunday.
    pub weekday_number: i64,
    pub iso_year: i64,
    pub iso_week: i64,
    /// `"<d> <Month> <yyyy>"`: the Monday ISO week 1 of `iso_year` began.
    pub iso_week1_starts: String,
    pub day_of_year: i64,
    pub days_in_year: i64,
    pub days_left_in_year: i64,
}

/// `factsLine(facts)`: one compact line for a prompt or a tool result.
pub fn facts_line(f: &CalendarFacts) -> String {
    let week = if f.iso_year == f.year {
        format!("ISO week {} of {}", f.iso_week, f.iso_year)
    } else {
        format!(
            "ISO week {} of {} (the ISO year is not the calendar year {})",
            f.iso_week, f.iso_year, f.year
        )
    };
    format!(
        "Calendar facts, computed by the server (use them as given, do not recount): {week} (weeks run Monday to Sunday; ISO week 1 of {} began Monday {}); day {} of {} in {}, {} days left; the year ends on 31 December.",
        f.iso_year, f.iso_week1_starts, f.day_of_year, f.days_in_year, f.year, f.days_left_in_year
    )
}

/// The year ICU prints (no era shown): before year 1 the era year, so 0 is 1 and -1 is 2.
fn intl_year(y: i64) -> i64 {
    if y <= 0 {
        1 - y
    } else {
        y
    }
}

/// `dayNumber(y, m, d)`: `Date.UTC`, which reads the years 0-99 as 1900-1999.
fn js_day_number(y: i64, m: i64, d: i64) -> i64 {
    day_number(if (0..=99).contains(&y) { y + 1900 } else { y }, m, d)
}

/// `calendarFacts` from the year, month and day Intl printed.
fn facts_of(y: i64, m: i64, d: i64) -> CalendarFacts {
    let today = js_day_number(y, m, d);
    let weekday = iso_weekday(today);
    let thursday = today + 4 - weekday;
    let iso_year = date_of(thursday).0;
    let iso_week = (thursday - js_day_number(iso_year, 1, 1)).div_euclid(7) + 1;
    let jan4 = js_day_number(iso_year, 1, 4);
    let (wy, wm, wd) = date_of(jan4 - iso_weekday(jan4) + 1);
    let days_in_year = if is_leap(y) { 366 } else { 365 };
    let day_of_year = today - js_day_number(y, 1, 1) + 1;
    CalendarFacts {
        year: y,
        month: m,
        day: d,
        weekday_number: weekday,
        iso_year,
        iso_week,
        iso_week1_starts: format!("{wd} {} {wy}", month(wm)),
        day_of_year,
        days_in_year,
        days_left_in_year: days_in_year - day_of_year,
    }
}

/// A zone Intl accepts that this port can compute.
#[derive(Debug, Clone, Copy)]
enum Zone {
    /// chrono-tz's rules and the recorded names by offset (minutes, en-GB short, en-US long).
    Named(Tz, &'static [(i32, &'static str, &'static str)]),
    /// A UTC offset string, in minutes east of UTC.
    Offset(i64),
}

fn lookup(name: &str) -> Option<Zone> {
    if let Some(minutes) = offset_zone(name) {
        return Some(Zone::Offset(minutes));
    }
    let key = name.to_ascii_lowercase();
    let i = zones::ZONES
        .binary_search_by(|(id, _, _)| (*id).cmp(key.as_str()))
        .ok()?;
    let (_, target, names) = zones::ZONES.get(i)?;
    let tz: Tz = target.parse().ok()?;
    Some(Zone::Named(tz, zones::NAMES.get(usize::from(*names))?))
}

fn digits(s: &[u8]) -> Option<i64> {
    if s.len() != 2 || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
}

/// `±HH`, `±HHMM` or `±HH:MM` (hours 0-23, minutes 0-59; the minus may be U+2212): minutes east
/// of UTC.
fn offset_zone(name: &str) -> Option<i64> {
    let (sign, rest) = match name.chars().next()? {
        '+' => (1, name.get(1..)?),
        '-' => (-1, name.get(1..)?),
        '\u{2212}' => (-1, name.get('\u{2212}'.len_utf8()..)?),
        _ => return None,
    };
    let b = rest.as_bytes();
    let (h, m) = match b.len() {
        2 => (b, &b"00"[..]),
        4 => (b.get(0..2)?, b.get(2..4)?),
        5 if b.get(2) == Some(&b':') => (b.get(0..2)?, b.get(3..5)?),
        _ => return None,
    };
    let (h, m) = (digits(h)?, digits(m)?);
    (h <= 23 && m <= 59).then_some(sign * (h * 60 + m))
}

/// The en-GB short name at a UTC offset of `offset_sec`: the recorded one, else ICU's localized
/// GMT form (`GMT`, `GMT+9`, `GMT-3:30`, `GMT+0:53:28`).
fn short_name(zone: Zone, offset_sec: i64) -> String {
    if let Zone::Named(_, names) = zone {
        if let Some((_, short, _)) = names.iter().find(|n| i64::from(n.0) * 60 == offset_sec) {
            return short.to_string();
        }
    }
    let (sign, a) = (if offset_sec < 0 { '-' } else { '+' }, offset_sec.abs());
    match (a / 3600, a / 60 % 60, a % 60) {
        (0, 0, 0) => "GMT".to_string(),
        (h, 0, 0) => format!("GMT{sign}{h}"),
        (h, m, 0) => format!("GMT{sign}{h}:{m:02}"),
        (h, m, s) => format!("GMT{sign}{h}:{m:02}:{s:02}"),
    }
}

/// `Date#toString`'s zone name for a named zone at `t`. V8 asks ICU for the standard or the
/// daylight name by whether `t` is in daylight time, and outside 1970-2038 it asks that of V8's
/// equivalent time (the same date in a year of 2008-2035 with the same leap-ness and weekday).
/// Here: the recorded name for the offset at that time, else the zone's standard (first) name.
fn long_name(zone: Zone, t: i64) -> Option<String> {
    let Zone::Named(_, names) = zone else {
        return None;
    };
    let t = if !(0..=2_147_483_647_000).contains(&t) {
        equivalent_time(t, 2008)
    } else {
        t
    };
    let off = offset_sec(t, zone)?;
    let hit = names.iter().find(|n| i64::from(n.0) * 60 == off);
    hit.or(names.first()).map(|n| n.2.to_string())
}

/// V8's DateCache::EquivalentTime: the same date and time in the year of `base..base + 28` with
/// the same leap-ness and weekday of 1 January (V8's base is 2008).
fn equivalent_time(t: i64, base: i64) -> i64 {
    let days = t.div_euclid(DAY_MS);
    let (y, m, d) = date_of(days);
    let week_day = (day_number(y, 1, 1) + 4).rem_euclid(7); // 0 Sunday
    let recent = if is_leap(y) { 1956 } else { 1967 } + (week_day * 12) % 28;
    let year = base + (recent - base).rem_euclid(28);
    (day_number(year, m, 1) + d - 1) * DAY_MS + t.rem_euclid(DAY_MS)
}

/// The zone's UTC offset in seconds at `t`. chrono-tz's transitions end in 2099 (its last offset
/// holds after), where ICU keeps applying the zone's rules: from 2100 on the offset is the one at
/// the equivalent time in 2072-2099 (same leap-ness and weekdays, so the same rule dates). Before chrono's
/// range (about year -262,000) it is the zone's first, fixed offset.
fn offset_sec(t: i64, zone: Zone) -> Option<i64> {
    const YEAR_2100: i64 = 4_102_444_800_000;
    const FAR_PAST: i64 = -8_000_000_000_000_000;
    match zone {
        Zone::Named(tz, _) => {
            let t = if t >= YEAR_2100 {
                equivalent_time(t, 2072)
            } else {
                t.max(FAR_PAST)
            };
            let utc = chrono::DateTime::from_timestamp_millis(t)?.naive_utc();
            Some(i64::from(
                tz.offset_from_utc_datetime(&utc).fix().local_minus_utc(),
            ))
        }
        Zone::Offset(minutes) => Some(minutes * 60),
    }
}

/// An instant's wall-clock fields in a zone.
struct Local {
    offset_sec: i64,
    /// Days since 1970-01-01 of the local date.
    day: i64,
    y: i64,
    m: i64,
    d: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

impl Local {
    fn at(t: i64, zone: Zone) -> Option<Local> {
        let offset_sec = offset_sec(t, zone)?;
        let local = t.checked_add(offset_sec.checked_mul(1000)?)?;
        let day = local.div_euclid(DAY_MS);
        let ms = local.rem_euclid(DAY_MS);
        let (y, m, d) = date_of(day);
        Some(Local {
            offset_sec,
            day,
            y,
            m,
            d,
            hour: ms / 3_600_000,
            minute: ms / 60_000 % 60,
            second: ms / 1000 % 60,
        })
    }
}

/// ECMAScript TimeClip: the integral time value, or `None` for NaN, the infinities and
/// magnitudes over 8.64e15.
fn time_clip(x: f64) -> Option<i64> {
    // The range check keeps the cast exact.
    #[allow(clippy::cast_possible_truncation)]
    (x.is_finite() && x.abs() <= 8.64e15).then(|| x.trunc() as i64)
}

/// `Date#toISOString`.
fn iso_string(t: i64) -> String {
    let (y, m, d) = date_of(t.div_euclid(DAY_MS));
    let ms = t.rem_euclid(DAY_MS);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else {
        format!("{}{:06}", if y < 0 { '-' } else { '+' }, y.abs())
    };
    format!(
        "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn day_number(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The proleptic Gregorian date of day `n` (days since 1970-01-01).
fn date_of(n: i64) -> (i64, i64, i64) {
    let z = n + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// 1 Monday .. 7 Sunday (day 0, 1970-01-01, was a Thursday).
fn iso_weekday(n: i64) -> i64 {
    (n + 3).rem_euclid(7) + 1
}

fn weekday(day: i64) -> &'static str {
    usize::try_from(iso_weekday(day) - 1)
        .ok()
        .and_then(|i| WEEKDAYS.get(i))
        .copied()
        .unwrap_or("")
}

fn month(m: i64) -> &'static str {
    usize::try_from(m - 1)
        .ok()
        .and_then(|i| MONTHS.get(i))
        .copied()
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_table_is_sorted_for_the_binary_search() {
        assert!(zones::ZONES.windows(2).all(|w| match w {
            [a, b] => a.0 < b.0,
            _ => false,
        }));
        assert!(zones::ZONES.iter().all(
            |(id, _, n)| *id == id.to_ascii_lowercase() && usize::from(*n) < zones::NAMES.len()
        ));
    }

    #[test]
    fn days_round_trip() {
        for n in [-800_000, -1, 0, 1, 19_000, 20_000, 2_932_896] {
            let (y, m, d) = date_of(n);
            assert_eq!(day_number(y, m, d), n);
        }
    }
}
