//! noevia-core `server/tool-rate-limit.cjs` (#1693, #1701) in Rust: a tool provider that answered 429
//! (rate limited, or its quota spent) is not called again at once. The back-off doubles on each 429 from
//! 15 s up to 5 min (a `Retry-After` the provider sends wins, within the cap, never under 1 s), a success
//! clears it, and entries whose wait ended more than one maximum wait ago are pruned on every lookup.
//! Keyed by the caller (the server id, plus the account for a per-account credential), so one account's
//! own key never pauses another's.
//!
//! Times are milliseconds as `f64`, as core's `Date.now()` arithmetic is. The caller passes the clock.
//!
//! Proven against core: `tests/fixtures/tool-rate-limit.v1.json` is recorded from core's own module by
//! `tests/gen-fixtures.cjs` (the source file's sha256 is in the fixture).
//!
//! Named difference (tested in `tests/fixtures.rs`): `parseRetryAfter` hands a non-number to V8's
//! `Date.parse`, which accepts many date spellings. Here an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00
//! GMT`, the form HTTP servers send) and an ISO 8601 UTC date-time (`2015-10-21T07:28:00Z`, optional
//! `.sss`) are dates; any other spelling is no `Retry-After`, so the doubling back-off is used. Most of
//! V8's extra spellings that a server could send land in the past (`"1."`, `"-1"`, `"1.25.3"` are years),
//! which core turns into 0 and `hit` ignores exactly as it ignores `None`.
use policy_leaves::{is_js_space, js_trim};
use std::collections::HashMap;

/// core `BASE_MS`.
pub const BASE_MS: f64 = 15_000.0;
/// core `MAX_MS`.
pub const MAX_MS: f64 = 300_000.0;

/// One key's state: until when it waits, and how many 429s in a row it has had.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Entry {
    until: f64,
    strikes: u32,
}

/// core `createToolRateLimit()`.
#[derive(Debug)]
pub struct RateLimit {
    state: HashMap<String, Entry>,
    base_ms: f64,
    max_ms: f64,
}

impl Default for RateLimit {
    fn default() -> Self {
        RateLimit::with(BASE_MS, MAX_MS)
    }
}

impl RateLimit {
    /// `createToolRateLimit({ baseMs, maxMs })`.
    pub fn with(base_ms: f64, max_ms: f64) -> RateLimit {
        RateLimit {
            state: HashMap::new(),
            base_ms,
            max_ms,
        }
    }

    fn prune(&mut self, at: f64) {
        let max = self.max_ms;
        // Times are finite (a clock), so `<=` is core's `!(at - until > maxMs)`.
        self.state.retain(|_, s| at - s.until <= max);
    }

    /// `remaining(key)`: ms left before this key may call again (0: go ahead).
    pub fn remaining(&mut self, key: &str, now: f64) -> f64 {
        self.prune(now);
        match self.state.get(key) {
            Some(s) if s.until > now => s.until - now,
            _ => 0.0,
        }
    }

    /// `hit(key, retryAfterMs)`: a 429 for this key; returns the wait in ms.
    pub fn hit(&mut self, key: &str, retry_after_ms: Option<f64>, now: f64) -> f64 {
        self.remaining(key, now);
        let strikes = self
            .state
            .get(key)
            .map_or(0, |s| s.strikes)
            .saturating_add(1);
        // `baseMs * 2 ** (strikes - 1)`: past 2^1100 it is Infinity in JS too; `min` gives maxMs either way.
        let exponent = i32::try_from(strikes - 1).unwrap_or(i32::MAX).min(1100);
        let backoff = self.max_ms.min(self.base_ms * 2f64.powi(exponent));
        let wait = match retry_after_ms {
            Some(r) if r.is_finite() && r > 0.0 => self.max_ms.min(r.max(1000.0)),
            _ => backoff,
        };
        self.state.insert(
            key.to_owned(),
            Entry {
                until: now + wait,
                strikes,
            },
        );
        wait
    }

    /// `clear(key)`.
    pub fn clear(&mut self, key: &str) {
        self.state.remove(key);
    }

    /// `size()`: entries held.
    pub fn len(&self) -> usize {
        self.state.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }
}

fn word(u: Option<&u16>) -> bool {
    u.is_some_and(|&u| {
        u < 0x80 && {
            let b = u as u8;
            b.is_ascii_alphanumeric() || b == b'_'
        }
    })
}

/// `\b` at `i`.
fn boundary(t: &[u16], i: usize) -> bool {
    let before = i.checked_sub(1).and_then(|p| t.get(p));
    word(before) != word(t.get(i))
}

/// ASCII `lit` at `i`, ASCII letters folded (`/i` without `u` folds nothing else onto ASCII).
fn lit(t: &[u16], i: usize, lit: &str) -> bool {
    lit.bytes().enumerate().all(|(k, b)| {
        t.get(i + k).is_some_and(|&u| {
            u < 0x80 && (u as u8).eq_ignore_ascii_case(&b)
        })
    })
}

/// `[\s:]*429\b` from `j`.
fn tail_429(t: &[u16], mut j: usize) -> bool {
    while t.get(j).is_some_and(|&u| is_js_space(u) || u == u16::from(b':')) {
        j += 1;
    }
    lit(t, j, "429") && boundary(t, j + 3)
}

fn status_alt(t: &[u16], i: usize) -> bool {
    if lit(t, i, "http") && tail_429(t, i + 4) {
        return true;
    }
    if !lit(t, i, "status") {
        return false;
    }
    let j = i + 6;
    // `(?:\s+code)?`, greedy: with the group first, then without.
    let mut k = j;
    while t.get(k).is_some_and(|&u| is_js_space(u)) {
        k += 1;
    }
    if k > j && lit(t, k, "code") && tail_429(t, k + 4) {
        return true;
    }
    tail_429(t, j)
}

fn rate_alt(t: &[u16], i: usize) -> bool {
    if !lit(t, i, "rate") {
        return false;
    }
    let j = i + 4;
    let sep = t
        .get(j)
        .is_some_and(|&u| is_js_space(u) || u == u16::from(b'-'));
    let starts: &[usize] = if sep { &[j + 1, j] } else { &[j] };
    starts.iter().any(|&p| {
        lit(t, p, "limit") && {
            let q = p + 5;
            (lit(t, q, "ed") && boundary(t, q + 2))
                || (lit(t, q, "ing") && boundary(t, q + 3))
                || boundary(t, q)
        }
    })
}

fn phrase(t: &[u16], i: usize, s: &str) -> bool {
    lit(t, i, s) && boundary(t, i + s.len())
}

/// core `RATE_LIMIT_TEXT` (`/i`, no `u`) tested at every start.
fn rate_limit_match(t: &[u16]) -> bool {
    (0..t.len()).any(|i| {
        boundary(t, i)
            && (status_alt(t, i)
                || rate_alt(t, i)
                || phrase(t, i, "too many requests")
                || phrase(t, i, "quota exceeded")
                || phrase(t, i, "quota exhausted")
                || phrase(t, i, "usage limit"))
    })
}

/// `isRateLimitedText(text)`: whether a tool result reports a rate limit in its first 2000 UTF-16 units.
pub fn is_rate_limited_text(text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().take(2000).collect();
    rate_limit_match(&units)
}

fn digits(s: &[u16]) -> Option<usize> {
    let n = s
        .iter()
        .take_while(|&&u| (u16::from(b'0')..=u16::from(b'9')).contains(&u))
        .count();
    (n > 0).then_some(n)
}

fn number(s: &[u16]) -> Option<f64> {
    let whole = digits(s)?;
    let rest = s.get(whole..)?;
    let ok = rest.is_empty()
        || (rest.first() == Some(&u16::from(b'.'))
            && rest
                .get(1..)
                .and_then(digits)
                .is_some_and(|f| f + 1 == rest.len()));
    if !ok {
        return None;
    }
    String::from_utf16(s).ok()?.parse::<f64>().ok()
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn utc_ms(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64, ms: i64) -> Option<f64> {
    let valid = (1..=12).contains(&mo)
        && (1..=days_in_month(y, mo)).contains(&d)
        && (0..=23).contains(&h)
        && (0..=59).contains(&mi)
        && (0..=59).contains(&s);
    valid.then(|| {
        let days = days_from_civil(y, mo, d);
        ((days * 86_400 + h * 3600 + mi * 60 + s) * 1000 + ms) as f64
    })
}

fn fixed<const N: usize>(b: &[u8], at: usize) -> Option<i64> {
    let part = b.get(at..at + N)?;
    if !part.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(part).ok()?.parse().ok()
}

/// `Wed, 21 Oct 2015 07:28:00 GMT`.
fn imf_fixdate(b: &[u8]) -> Option<f64> {
    const DAYS: [&[u8]; 7] = [b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    if b.len() != 29 || !DAYS.contains(&b.get(0..3)?) || b.get(3..5)? != b", " {
        return None;
    }
    let d = fixed::<2>(b, 5)?;
    let month = b.get(8..11)?;
    let mo = MONTHS.iter().position(|m| *m == month)? as i64 + 1;
    let shape = b.get(7) == Some(&b' ')
        && b.get(11) == Some(&b' ')
        && b.get(16) == Some(&b' ')
        && b.get(19) == Some(&b':')
        && b.get(22) == Some(&b':')
        && b.get(25..)? == b" GMT";
    if !shape {
        return None;
    }
    let y = fixed::<4>(b, 12)?;
    utc_ms(y, mo, d, fixed::<2>(b, 17)?, fixed::<2>(b, 20)?, fixed::<2>(b, 23)?, 0)
}

/// `2015-10-21T07:28:00Z` or `2015-10-21T07:28:00.123Z`.
fn iso_utc(b: &[u8]) -> Option<f64> {
    let shape = b.get(4) == Some(&b'-')
        && b.get(7) == Some(&b'-')
        && b.get(10) == Some(&b'T')
        && b.get(13) == Some(&b':')
        && b.get(16) == Some(&b':');
    if !shape {
        return None;
    }
    let ms = match b.len() {
        20 if b.get(19) == Some(&b'Z') => 0,
        24 if b.get(19) == Some(&b'.') && b.get(23) == Some(&b'Z') => fixed::<3>(b, 20)?,
        _ => return None,
    };
    let s = fixed::<2>(b, 17)?;
    utc_ms(
        fixed::<4>(b, 0)?,
        fixed::<2>(b, 5)?,
        fixed::<2>(b, 8)?,
        fixed::<2>(b, 11)?,
        fixed::<2>(b, 14)?,
        s,
        ms,
    )
}

/// `parseRetryAfter(value, now)`: a `Retry-After` in ms (seconds, or a date, see the module docs), or
/// `None`. `value` is the header as text; `None` or empty is absent.
pub fn parse_retry_after(value: Option<&str>, now: f64) -> Option<f64> {
    let value = value.filter(|v| !v.is_empty())?;
    let units: Vec<u16> = value.encode_utf16().collect();
    let s = js_trim(&units);
    if let Some(n) = number(s) {
        // Math.round: half-way rounds up, which for these non-negative values is `f64::round`.
        return Some((n * 1000.0).round());
    }
    let text = String::from_utf16(s).ok()?;
    let at = imf_fixdate(text.as_bytes()).or_else(|| iso_utc(text.as_bytes()))?;
    Some((at - now).max(0.0))
}

/// core `seconds(ms)`: `Math.max(1, Math.ceil(ms / 1000))`.
fn seconds(ms: f64) -> String {
    let s = (ms / 1000.0).ceil().max(1.0);
    if s.is_finite() && s < 9e15 {
        format!("{}", s as i64)
    } else {
        format!("{s}")
    }
}

/// `rateLimitedText(name, waitMs)`: the model's text for a 429 just received.
pub fn rate_limited_text(name: &str, wait_ms: f64) -> String {
    format!(
        "ERROR: {name} was rate limited by its provider (HTTP 429). This is retryable, but not now: wait about {} s before calling it again. Answer from the results you already have, or say the search is rate limited.",
        seconds(wait_ms)
    )
}

/// `coolingDownText(name, waitMs)`: the model's text for a call skipped because the provider still waits.
pub fn cooling_down_text(name: &str, wait_ms: f64) -> String {
    format!(
        "ERROR: {name} was not called: its provider is rate limiting requests (HTTP 429), retry in about {} s. Answer from the results you already have, or say the search is rate limited.",
        seconds(wait_ms)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_429_doubles_until_the_cap_and_a_success_clears_it() {
        let mut r = RateLimit::default();
        assert_eq!(r.remaining("k", 0.0), 0.0);
        assert_eq!(r.hit("k", None, 0.0), 15_000.0);
        assert_eq!(r.remaining("k", 1_000.0), 14_000.0);
        assert_eq!(r.hit("k", None, 1_000.0), 30_000.0);
        for _ in 0..40 {
            r.hit("k", None, 1_000.0);
        }
        assert_eq!(r.hit("k", None, 1_000.0), MAX_MS);
        r.clear("k");
        assert_eq!(r.hit("k", None, 1_000.0), 15_000.0);
        assert_eq!(r.hit("other", Some(500.0), 0.0), 1_000.0);
        assert_eq!(r.hit("other", Some(1e12), 0.0), MAX_MS);
    }

    #[test]
    fn the_texts_are_cores() {
        assert!(rate_limited_text("t", 0.0).contains("wait about 1 s"));
        assert!(cooling_down_text("t", 15_001.0).contains("retry in about 16 s"));
    }
}
