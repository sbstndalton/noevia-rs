//! When the off-site backup may run on its own. Core ran a snapshot when `new Date().getHours()`
//! equalled `OFFSITE_BACKUP_HOUR` (default 3) and the last one was over 20 hours old, checked every
//! 15 minutes, without regard to the host's own jobs. This keeps that rule and adds two quiet
//! windows a scheduled run neither starts in nor continues through (an administrator's own "Run now"
//! is not held back):
//!
//! - the Unraid appdata backup, 04:05 to 04:25 server time (it stops and starts the containers);
//! - the NAS window, 02:20 to 04:30 America/New_York.
//!
//! "Server time" and "local" are the zone the container runs in. Without libc this reads the `TZ`
//! name: `America/New_York` (and `US/Eastern`, `EST5EDT`) use the United States daylight-saving
//! rule, `UTC`, `Etc/UTC`, `GMT` or nothing mean UTC. Any other name is treated as UTC, which
//! [`Zone::from_tz`] reports so the caller can say so once.
//!
//! A due run also catches up: within [`GRACE_HOURS`] after the configured hour it still starts
//! (core would have skipped the day if the server was busy or down at that exact hour), so a run
//! deferred past a quiet window still happens.

use crate::{civil_from_days, days_from_civil};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// How long after the configured hour a missed run may still start.
pub const GRACE_HOURS: u32 = 6;
/// A scheduled run waits until the last one is older than this (core's 20 hours).
pub const MIN_GAP_MS: i64 = 20 * HOUR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Utc,
    /// The United States Eastern rule (second Sunday of March 02:00 to first Sunday of November 02:00).
    NewYork,
}

impl Zone {
    /// The zone a `TZ` value names, and whether the name was recognised.
    pub fn from_tz(name: Option<&str>) -> (Zone, bool) {
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            None => (Zone::Utc, true),
            Some(n) => match n.trim_start_matches(':') {
                "UTC" | "Etc/UTC" | "GMT" | "Etc/GMT" | "Z" => (Zone::Utc, true),
                "America/New_York" | "US/Eastern" | "EST5EDT" | "America/Detroit" => {
                    (Zone::NewYork, true)
                }
                _ => (Zone::Utc, false),
            },
        }
    }

    /// Milliseconds to add to a UTC instant to get this zone's wall clock.
    pub fn offset_ms(self, utc_ms: i64) -> i64 {
        match self {
            Zone::Utc => 0,
            Zone::NewYork => {
                if us_eastern_dst(utc_ms) {
                    -4 * HOUR
                } else {
                    -5 * HOUR
                }
            }
        }
    }
}

/// The nth (1-based) Sunday of a month, as a day number since the epoch.
fn nth_sunday(year: i64, month: u32, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let weekday = (first + 4).rem_euclid(7); // 0 = Sunday
    first + (7 - weekday).rem_euclid(7) + (n - 1) * 7
}

/// Whether daylight-saving time is in force in US Eastern at this UTC instant (2007 rule).
fn us_eastern_dst(utc_ms: i64) -> bool {
    let (year, _, _) = civil_from_days(utc_ms.div_euclid(DAY));
    // 02:00 EST = 07:00 UTC on the second Sunday of March; 02:00 EDT = 06:00 UTC on the first of November.
    let start = nth_sunday(year, 3, 2) * DAY + 7 * HOUR;
    let end = nth_sunday(year, 11, 1) * DAY + 6 * HOUR;
    utc_ms >= start && utc_ms < end
}

/// The zone's wall-clock minute of the day (0..1440) at a UTC instant.
pub fn minute_of_day(zone: Zone, utc_ms: i64) -> i64 {
    (utc_ms + zone.offset_ms(utc_ms)).rem_euclid(DAY) / MINUTE
}

/// The zone's wall-clock hour (0..24).
pub fn hour_of_day(zone: Zone, utc_ms: i64) -> u32 {
    (minute_of_day(zone, utc_ms) / 60) as u32
}

/// One quiet window: a start and end minute of the day in a zone (never across midnight).
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub zone: Zone,
    pub from: i64,
    pub to: i64,
}

/// The windows a scheduled run keeps out of, given the zone the server's own clock is in.
pub fn windows(server: Zone) -> [Window; 2] {
    [
        // Appdata backup: 04:05 to 04:25 server time.
        Window {
            zone: server,
            from: 4 * 60 + 5,
            to: 4 * 60 + 25,
        },
        // NAS window: 02:20 to 04:30 America/New_York.
        Window {
            zone: Zone::NewYork,
            from: 2 * 60 + 20,
            to: 4 * 60 + 30,
        },
    ]
}

/// When the quiet windows next let a scheduled run proceed: `None` when `utc_ms` is outside every
/// window, else the instant the (chained) windows end.
pub fn quiet_until(server: Zone, utc_ms: i64) -> Option<i64> {
    let mut at = utc_ms;
    let mut inside = false;
    // The two windows overlap or touch at most once a day, so a handful of passes always settles.
    for _ in 0..4 {
        let mut moved = false;
        for w in windows(server) {
            let wall = (at + w.zone.offset_ms(at)).rem_euclid(DAY);
            let m = wall / MINUTE;
            if m >= w.from && m < w.to {
                // The window ends at wall-clock minute `to` of this day.
                let end = at + (w.to * MINUTE - wall);
                if end > at {
                    at = end;
                    moved = true;
                    inside = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
    inside.then_some(at)
}

/// Whether a scheduled run is due: the clock is within [`GRACE_HOURS`] after the configured hour
/// (local) and the last backup is more than [`MIN_GAP_MS`] old (`last_ms` 0: never).
pub fn due(local: Zone, utc_ms: i64, hour: u32, last_ms: i64) -> bool {
    let now_hour = hour_of_day(local, utc_ms);
    now_hour >= hour && now_hour < (hour + GRACE_HOURS).min(24) && utc_ms - last_ms > MIN_GAP_MS
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn utc(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * DAY + h * HOUR + mi * MINUTE
    }

    #[test]
    fn new_york_follows_the_us_rule() {
        // 2026: DST from Sunday 8 March to Sunday 1 November.
        assert_eq!(Zone::NewYork.offset_ms(utc(2026, 3, 8, 6, 59)), -5 * HOUR);
        assert_eq!(Zone::NewYork.offset_ms(utc(2026, 3, 8, 7, 0)), -4 * HOUR);
        assert_eq!(Zone::NewYork.offset_ms(utc(2026, 11, 1, 5, 59)), -4 * HOUR);
        assert_eq!(Zone::NewYork.offset_ms(utc(2026, 11, 1, 6, 0)), -5 * HOUR);
        assert_eq!(Zone::NewYork.offset_ms(utc(2026, 7, 4, 12, 0)), -4 * HOUR);
        assert_eq!(Zone::NewYork.offset_ms(utc(2026, 1, 4, 12, 0)), -5 * HOUR);
        assert_eq!(
            minute_of_day(Zone::NewYork, utc(2026, 10, 30, 8, 10)),
            4 * 60 + 10
        );
    }

    #[test]
    fn tz_names() {
        assert_eq!(Zone::from_tz(None), (Zone::Utc, true));
        assert_eq!(
            Zone::from_tz(Some("America/New_York")),
            (Zone::NewYork, true)
        );
        assert_eq!(Zone::from_tz(Some("Europe/Paris")), (Zone::Utc, false));
    }

    #[test]
    fn nas_window_in_new_york_time() {
        // 02:20 EDT is 06:20 UTC in October 2026.
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 10, 30, 6, 19)), None);
        let end = quiet_until(Zone::Utc, utc(2026, 10, 30, 6, 20)).unwrap();
        assert_eq!(end, utc(2026, 10, 30, 8, 30), "04:30 EDT");
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 10, 30, 8, 29)), Some(end));
        assert_eq!(quiet_until(Zone::Utc, end), None);
        // In winter the same wall clock is an hour later in UTC.
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 1, 15, 7, 19)), None);
        assert_eq!(
            quiet_until(Zone::Utc, utc(2026, 1, 15, 7, 20)),
            Some(utc(2026, 1, 15, 9, 30))
        );
    }

    #[test]
    fn appdata_window_follows_the_server_clock() {
        // A server on UTC: 04:05 to 04:25 UTC, which is outside the NAS window in the summer.
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 10, 30, 4, 4)), None);
        assert_eq!(
            quiet_until(Zone::Utc, utc(2026, 10, 30, 4, 5)),
            Some(utc(2026, 10, 30, 4, 25))
        );
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 10, 30, 4, 25)), None);
        // A server on New York time: both windows cover 04:10, the later end wins.
        assert_eq!(
            quiet_until(Zone::NewYork, utc(2026, 10, 30, 8, 10)),
            Some(utc(2026, 10, 30, 8, 30))
        );
    }

    #[test]
    fn a_missed_hour_catches_up_but_not_without_end() {
        let day = |h, mi| utc(2026, 10, 30, h, mi);
        let last = day(3, 10) - DAY; // yesterday at 03:10
        assert!(!due(Zone::Utc, day(2, 59), 3, last));
        assert!(due(Zone::Utc, day(3, 0), 3, last));
        assert!(due(Zone::Utc, day(8, 59), 3, last), "within the grace");
        assert!(!due(Zone::Utc, day(9, 0), 3, last), "past the grace");
        assert!(
            !due(Zone::Utc, day(3, 30), 3, day(3, 10) - 19 * HOUR),
            "ran 19 hours ago"
        );
        assert!(due(Zone::Utc, day(3, 30), 3, 0), "never ran");
        assert!(!due(Zone::Utc, day(23, 59), 23, day(23, 59) - 3 * HOUR));
    }
}
