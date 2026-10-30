//! When the off-site backup may run on its own. Core ran a snapshot when `new Date().getHours()`
//! equalled `OFFSITE_BACKUP_HOUR` (default 3) and the last one was over 20 hours old, checked every
//! 15 minutes, without regard to the host's own jobs. This keeps that rule and adds two quiet
//! windows a scheduled run neither starts in nor continues through (an administrator's own "Run now"
//! is not held back):
//!
//! - the Unraid appdata backup, 04:05 to 04:25 on the host's clock;
//! - the NAS window, 02:20 to 04:30 America/New_York.
//!
//! Two zones are in play and they are named, never guessed:
//!
//! - the HOUR is read on the process clock, as core's `getHours()` did: the `TZ` the process runs
//!   with, UTC when it has none (the production web container sets none, so there it is UTC);
//! - the QUIET WINDOWS are evaluated in explicit zones: the NAS window always in America/New_York,
//!   the appdata window in the host's zone, which is `OFFSITE_BACKUP_TIMEZONE` (default
//!   America/New_York: the Unraid host runs on Eastern time while the container has no `TZ`).
//!
//! A zone name is one of `America/New_York` (and `US/Eastern`, `EST5EDT`, `America/Detroit`), which
//! use the United States daylight-saving rule, or `UTC`, `Etc/UTC`, `GMT`. Any other name is not
//! known ([`Zone::from_tz`] says so) and the caller must say so loudly rather than assume UTC.
//!
//! A due run also catches up: within [`GRACE_HOURS`] after the configured hour it still starts
//! (core would have skipped the day if the server was busy or down at that exact hour), so a run
//! deferred past a quiet window still happens. A failed run is retried after a growing pause
//! ([`backoff_ms`]) and at most [`MAX_ATTEMPTS_PER_WINDOW`] times in one window, so a broken
//! destination costs three walks a night, not eighteen.

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
    /// The zone a `TZ` value names, and whether the name was recognised. No name at all is UTC,
    /// which is what a process without `TZ` runs in; an unrecognised name is UTC with `false`.
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

    /// The canonical name the status and the log show.
    pub fn name(self) -> &'static str {
        match self {
            Zone::Utc => "UTC",
            Zone::NewYork => "America/New_York",
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

/// Failed scheduled attempts allowed in one grace window (it lies inside one local day).
pub const MAX_ATTEMPTS_PER_WINDOW: u32 = 3;
const BACKOFF_FIRST: i64 = 15 * MINUTE;
const BACKOFF_CAP: i64 = 2 * HOUR;

/// How long to wait after the `failures`-th failure in a row: 15 minutes doubling, at most 2 hours.
pub fn backoff_ms(failures: u32) -> i64 {
    if failures == 0 {
        return 0;
    }
    let doublings = (failures - 1).min(16);
    BACKOFF_FIRST
        .saturating_mul(1i64 << doublings)
        .min(BACKOFF_CAP)
}

/// The last failed backup, persisted in the status file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// When the latest attempt failed.
    pub at: i64,
    /// Failures in a row since the last good backup.
    pub failures: u32,
    /// Failed scheduled attempts on the local day of `at` (an administrator's own run is not one).
    pub attempts: u32,
}

/// The local calendar day number (days since the epoch on the zone's wall clock).
pub fn local_day(zone: Zone, utc_ms: i64) -> i64 {
    (utc_ms + zone.offset_ms(utc_ms)).div_euclid(DAY)
}

impl Retry {
    /// The record after another failure at `now`; `scheduled` is whether the schedule made the run.
    pub fn failed(prev: Option<Retry>, zone: Zone, now: i64, scheduled: bool) -> Retry {
        let same_day = prev.is_some_and(|p| local_day(zone, p.at) == local_day(zone, now));
        let before = prev.map_or(0, |p| if same_day { p.attempts } else { 0 });
        Retry {
            at: now,
            failures: prev.map_or(0, |p| p.failures).saturating_add(1),
            attempts: before + u32::from(scheduled),
        }
    }
}

/// Whether a scheduled run is due: the clock is within [`GRACE_HOURS`] after the configured hour
/// (local) and the last backup is more than [`MIN_GAP_MS`] old (`last_ms` 0: never); after a
/// failure also past its backoff and under the day's attempt cap.
pub fn due(local: Zone, utc_ms: i64, hour: u32, last_ms: i64, retry: Option<Retry>) -> bool {
    let now_hour = hour_of_day(local, utc_ms);
    if !(now_hour >= hour
        && now_hour < (hour + GRACE_HOURS).min(24)
        && utc_ms - last_ms > MIN_GAP_MS)
    {
        return false;
    }
    match retry {
        None => true,
        Some(r) => {
            utc_ms >= r.at.saturating_add(backoff_ms(r.failures))
                && !(local_day(local, r.at) == local_day(local, utc_ms)
                    && r.attempts >= MAX_ATTEMPTS_PER_WINDOW)
        }
    }
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
        assert!(!due(Zone::Utc, day(2, 59), 3, last, None));
        assert!(due(Zone::Utc, day(3, 0), 3, last, None));
        assert!(
            due(Zone::Utc, day(8, 59), 3, last, None),
            "within the grace"
        );
        assert!(!due(Zone::Utc, day(9, 0), 3, last, None), "past the grace");
        assert!(
            !due(Zone::Utc, day(3, 30), 3, day(3, 10) - 19 * HOUR, None),
            "ran 19 hours ago"
        );
        assert!(due(Zone::Utc, day(3, 30), 3, 0, None), "never ran");
        assert!(!due(
            Zone::Utc,
            day(23, 59),
            23,
            day(23, 59) - 3 * HOUR,
            None
        ));
    }

    #[test]
    fn backoff_doubles_from_fifteen_minutes_to_two_hours() {
        let mins: Vec<i64> = (0..8).map(|n| backoff_ms(n) / MINUTE).collect();
        assert_eq!(mins, [0, 15, 30, 60, 120, 120, 120, 120]);
        assert_eq!(backoff_ms(u32::MAX), 2 * HOUR, "no overflow");
    }

    #[test]
    fn a_failed_run_waits_out_its_backoff_and_is_capped_per_window() {
        let day = |h, mi| utc(2026, 10, 30, h, mi);
        let last = day(3, 0) - 2 * DAY;
        let z = Zone::Utc;
        // First failure at 03:00 (scheduled): not again before 03:15.
        let r1 = Retry::failed(None, z, day(3, 0), true);
        assert_eq!(
            r1,
            Retry {
                at: day(3, 0),
                failures: 1,
                attempts: 1
            }
        );
        assert!(!due(z, day(3, 14), 3, last, Some(r1)));
        assert!(due(z, day(3, 15), 3, last, Some(r1)));
        // Second failure at 03:15: 30 minutes.
        let r2 = Retry::failed(Some(r1), z, day(3, 15), true);
        assert_eq!((r2.failures, r2.attempts), (2, 2));
        assert!(!due(z, day(3, 44), 3, last, Some(r2)));
        assert!(due(z, day(3, 45), 3, last, Some(r2)));
        // Third failure at 03:45: an hour, and the window's cap is spent.
        let r3 = Retry::failed(Some(r2), z, day(3, 45), true);
        assert_eq!((r3.failures, r3.attempts), (3, 3));
        assert!(!due(z, day(4, 44), 3, last, Some(r3)));
        assert!(
            !due(z, day(8, 59), 3, last, Some(r3)),
            "three attempts in this window are all it gets, however late"
        );
        // The next night the cap is new but the pause is still the failures' (capped at 2 hours).
        let next = day(3, 0) + DAY;
        assert!(due(z, next, 3, last, Some(r3)));
        let r4 = Retry::failed(Some(r3), z, next, true);
        assert_eq!(
            (r4.failures, r4.attempts),
            (4, 1),
            "attempts count per day, failures do not"
        );
        assert!(!due(z, next + 119 * MINUTE, 3, last, Some(r4)));
        assert!(due(z, next + 120 * MINUTE, 3, last, Some(r4)));
        // An administrator's own failed run backs the schedule off but does not spend its cap.
        let manual = Retry::failed(None, z, day(2, 0), false);
        assert_eq!(manual.attempts, 0);
        assert!(due(z, day(3, 0), 3, last, Some(manual)));
    }

    #[test]
    fn the_hour_is_read_on_the_process_clock_the_windows_in_their_own_zones() {
        // 12:30 UTC on 30 October is 08:30 EDT. A process on UTC (the production container has no
        // TZ) reads hour 12, a process on New York time hour 8: the same setting, two nights.
        let t = utc(2026, 10, 30, 12, 30);
        assert!(due(Zone::Utc, t, 10, 0, None));
        assert!(!due(Zone::NewYork, t, 10, 0, None));
        assert!(due(Zone::NewYork, t, 8, 0, None));
        // The windows do not follow that zone: 06:30 UTC is inside the NAS window whatever TZ says.
        assert!(quiet_until(Zone::Utc, utc(2026, 10, 30, 6, 30)).is_some());
        assert!(quiet_until(Zone::NewYork, utc(2026, 10, 30, 6, 30)).is_some());
    }

    #[test]
    fn the_spring_forward_day_has_no_two_oclock() {
        // 2026-03-08: at 07:00 UTC New York jumps from 02:00 EST to 03:00 EDT.
        let before = utc(2026, 3, 8, 6, 59); // 01:59 EST
        let after = utc(2026, 3, 8, 7, 0); // 03:00 EDT
        assert_eq!(minute_of_day(Zone::NewYork, before), 119);
        assert_eq!(minute_of_day(Zone::NewYork, after), 180);
        // The NAS window (02:20-04:30) can only be entered at 03:00 EDT, and ends at 04:30 EDT = 08:30 UTC.
        assert_eq!(quiet_until(Zone::Utc, before), None);
        assert_eq!(quiet_until(Zone::Utc, after), Some(utc(2026, 3, 8, 8, 30)));
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 3, 8, 8, 30)), None);
        // The appdata window on a New York host is 04:05-04:25 EDT = 08:05-08:25 UTC that day.
        assert_eq!(
            quiet_until(Zone::NewYork, utc(2026, 3, 8, 8, 4)),
            Some(utc(2026, 3, 8, 8, 30))
        );
        assert_eq!(quiet_until(Zone::NewYork, utc(2026, 3, 8, 8, 30)), None);
        // A day later the same wall clock is an hour earlier in UTC.
        assert_eq!(
            quiet_until(Zone::Utc, utc(2026, 3, 9, 6, 20)),
            Some(utc(2026, 3, 9, 8, 30)),
            "02:20 EDT"
        );
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 3, 9, 6, 19)), None);
        // Hour 2 never happens that day; a run set for 02:00 starts at 03:00 local, in its grace.
        let ny = |h, mi| utc(2026, 3, 8, h, mi);
        assert!(!due(Zone::NewYork, ny(6, 59), 2, 0, None), "01:59 EST");
        assert!(
            due(Zone::NewYork, ny(7, 0), 2, 0, None),
            "03:00 EDT, inside 02:00 + 6 h"
        );
        assert!(due(Zone::NewYork, ny(11, 59), 2, 0, None), "07:59 EDT");
        assert!(
            !due(Zone::NewYork, ny(12, 0), 2, 0, None),
            "08:00 EDT, past the grace"
        );
    }

    #[test]
    fn the_fall_back_day_repeats_one_oclock() {
        // 2026-11-01: at 06:00 UTC New York falls back from 02:00 EDT to 01:00 EST.
        let first = utc(2026, 11, 1, 5, 30); // 01:30 EDT
        let second = utc(2026, 11, 1, 6, 30); // 01:30 EST
        assert_eq!(minute_of_day(Zone::NewYork, first), 90);
        assert_eq!(minute_of_day(Zone::NewYork, second), 90);
        assert_eq!(quiet_until(Zone::Utc, first), None);
        assert_eq!(quiet_until(Zone::Utc, second), None);
        // The NAS window 02:20-04:30 is now EST: 07:20-09:30 UTC (it was 06:20-08:30 UTC the day before).
        assert_eq!(quiet_until(Zone::Utc, utc(2026, 11, 1, 7, 19)), None);
        assert_eq!(
            quiet_until(Zone::Utc, utc(2026, 11, 1, 7, 20)),
            Some(utc(2026, 11, 1, 9, 30))
        );
        assert_eq!(
            quiet_until(Zone::Utc, utc(2026, 10, 31, 6, 20)),
            Some(utc(2026, 10, 31, 8, 30))
        );
        // The appdata window of a New York host is 04:05-04:25 EST = 09:05-09:25 UTC, inside the NAS one.
        assert_eq!(
            quiet_until(Zone::NewYork, utc(2026, 11, 1, 9, 10)),
            Some(utc(2026, 11, 1, 9, 30))
        );
        // A run set for 01:00 may start in either 01:00 hour, but the 20-hour gap stops a second.
        let hour_one = |t: i64, last: i64| due(Zone::NewYork, t, 1, last, None);
        assert!(hour_one(first, 0));
        assert!(
            !hour_one(second, first),
            "the repeated hour is the same night"
        );
        // The local day does not change at the repeated hour, so a day's attempt cap spans both.
        assert_eq!(
            local_day(Zone::NewYork, first),
            local_day(Zone::NewYork, second)
        );
        assert_ne!(
            local_day(Zone::NewYork, utc(2026, 11, 1, 3, 59)),
            local_day(Zone::NewYork, utc(2026, 11, 1, 4, 0))
        );
    }

    #[test]
    fn an_explicit_zone_is_never_a_silent_utc() {
        assert_eq!(Zone::from_tz(Some("US/Eastern")), (Zone::NewYork, true));
        assert_eq!(
            Zone::from_tz(Some(":America/New_York")),
            (Zone::NewYork, true)
        );
        assert_eq!(Zone::from_tz(Some("Etc/UTC")), (Zone::Utc, true));
        // Unknown names are flagged so the caller can warn.
        assert_eq!(Zone::from_tz(Some("Mars/Olympus")), (Zone::Utc, false));
        assert_eq!(Zone::from_tz(Some("est")), (Zone::Utc, false));
    }
}
