//! core `tool-rate-limit.cjs` (#1693, #1701): a tool provider that answered 429 is not called again
//! inside its wait. Keyed by the caller (server id, plus the account for a per-account credential),
//! so one account never pauses another. The backoff doubles on each 429 (a `Retry-After` wins, within
//! the cap) and a success clears it. Nothing here names a provider.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const BASE_MS: f64 = 15_000.0;
pub const MAX_MS: f64 = 5.0 * 60.0 * 1000.0;

pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

#[derive(Default)]
pub struct ToolRateLimit {
    state: Mutex<HashMap<String, (f64, u32)>>,
}

impl ToolRateLimit {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (f64, u32)>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remaining_at(state: &mut HashMap<String, (f64, u32)>, key: &str, at: f64) -> f64 {
        state.retain(|_, (until, _)| at - *until <= MAX_MS);
        match state.get(key) {
            Some((until, _)) if *until > at => *until - at,
            _ => 0.0,
        }
    }

    /// Milliseconds left before this key may call again (0: go ahead).
    pub fn remaining(&self, key: &str) -> f64 {
        Self::remaining_at(&mut self.lock(), key, now_ms())
    }

    /// A 429 for this key; returns the wait in milliseconds.
    pub fn hit(&self, key: &str, retry_after_ms: Option<f64>) -> f64 {
        self.hit_at(key, retry_after_ms, now_ms())
    }

    pub fn hit_at(&self, key: &str, retry_after_ms: Option<f64>, at: f64) -> f64 {
        let mut state = self.lock();
        Self::remaining_at(&mut state, key, at);
        let strikes = state.get(key).map_or(0, |(_, s)| *s) + 1;
        let backoff = (BASE_MS * 2f64.powi(strikes as i32 - 1)).min(MAX_MS);
        let wait = match retry_after_ms {
            Some(ms) if ms.is_finite() && ms > 0.0 => ms.clamp(1000.0, MAX_MS),
            _ => backoff,
        };
        state.insert(key.to_owned(), (at + wait, strikes));
        wait
    }

    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    pub fn size(&self) -> usize {
        self.lock().len()
    }
}

/// `isRateLimitedText`: a tool result that reports a rate limit in its text (an MCP `isError`
/// result rather than an HTTP status), judged on its first 2000 UTF-16 units.
///
/// /\b(?:HTTP|status(?:\s+code)?)[\s:]*429\b|\brate[\s-]?limit(?:ed|ing)?\b|\btoo many requests\b
///  |\bquota (?:exceeded|exhausted)\b|\busage limit\b/i  (ASCII word boundaries and case folding).
pub fn is_rate_limited_text(text: &str) -> bool {
    let head: String = js_json::js_slice(text, 2000);
    // /i without /u folds ASCII only; `\b` is the ASCII word boundary; `\s` is JavaScript's set.
    let chars: Vec<char> = head.chars().map(|c| c.to_ascii_lowercase()).collect();
    let cword = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let cboundary = |i: usize| -> bool {
        let before = i > 0 && chars.get(i - 1).is_some_and(|c| cword(*c));
        let after = chars.get(i).is_some_and(|c| cword(*c));
        before != after
    };
    let cspace = crate::mcp_broker::js_ws;
    let starts = |i: usize, lit: &str| -> bool {
        lit.chars()
            .enumerate()
            .all(|(k, c)| chars.get(i + k) == Some(&c))
    };
    for i in 0..chars.len() {
        if !cboundary(i) {
            continue;
        }
        // HTTP|status(\s+code)? [\s:]* 429 \b
        for head_lit in ["http", "status"] {
            if !starts(i, head_lit) {
                continue;
            }
            let mut j = i + head_lit.len();
            let mut variants = vec![j];
            if head_lit == "status" {
                let mut k = j;
                while chars.get(k).is_some_and(|c| cspace(*c)) {
                    k += 1;
                }
                if k > j && starts(k, "code") {
                    variants.push(k + 4);
                }
            }
            for start in variants.drain(..) {
                j = start;
                // Backtracking over [\s:]* is unnecessary: 429 is neither a space nor a colon.
                while chars.get(j).is_some_and(|c| cspace(*c) || *c == ':') {
                    j += 1;
                }
                if starts(j, "429") && cboundary(j + 3) {
                    return true;
                }
            }
        }
        // rate[\s-]?limit(ed|ing)?
        if starts(i, "rate") {
            let mut j = i + 4;
            if chars.get(j).is_some_and(|c| cspace(*c) || *c == '-') {
                j += 1;
            }
            if starts(j, "limit") {
                let end = j + 5;
                for tail in ["ed", "ing", ""] {
                    if starts(end, tail) && cboundary(end + tail.len()) {
                        return true;
                    }
                }
            }
        }
        for lit in [
            "too many requests",
            "quota exceeded",
            "quota exhausted",
            "usage limit",
        ] {
            if starts(i, lit) && cboundary(i + lit.chars().count()) {
                return true;
            }
        }
    }
    false
}

/// `parseRetryAfter`: seconds (a number) or an HTTP date, in milliseconds.
pub fn parse_retry_after(value: Option<&str>, now: f64) -> Option<f64> {
    let s = crate::mcp_broker::js_ws;
    let v = value?.trim_matches(s);
    if v.is_empty() {
        return None;
    }
    let mut parts = v.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let frac = parts.next();
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
    if digits(whole) && frac.is_none_or(digits) {
        return v.parse::<f64>().ok().map(|n| (n * 1000.0).round());
    }
    http_date_ms(v).map(|at| (at - now).max(0.0))
}

/// `Date.parse` of an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`), the HTTP date form.
///
/// **Known deviation from core:** only the IMF-fixdate is read. `Date.parse` also accepts the obsolete RFC 850 and
/// asctime dates (and other forms); here those fall back to the exponential wait instead of honouring the date.
/// The wait stays bounded (clamped to 300 s) either way.
fn http_date_ms(v: &str) -> Option<f64> {
    let rest = v.split_once(", ")?.1;
    let mut it = rest.split(' ');
    let day: i64 = it.next()?.parse().ok()?;
    let name = it.next()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == name)? as i64
        + 1;
    let year: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':');
    let (h, m, s): (i64, i64, i64) = (
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
        hms.next()?.parse().ok()?,
    );
    if it.next()? != "GMT" || it.next().is_some() {
        return None;
    }
    // days from civil
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + m * 60 + s) * 1000) as f64)
}

fn seconds(ms: f64) -> u64 {
    (ms / 1000.0).ceil().max(1.0) as u64
}

/// The model's text for a 429 just received. Starts with "ERROR: " so the call counts as failed.
pub fn rate_limited_text(name: &str, wait_ms: f64) -> String {
    format!("ERROR: {name} was rate limited by its provider (HTTP 429). This is retryable, but not now: wait about {} s before calling it again. Answer from the results you already have, or say the search is rate limited.", seconds(wait_ms))
}

/// The model's text for a call skipped because the provider is still in its wait.
pub fn cooling_down_text(name: &str, wait_ms: f64) -> String {
    format!("ERROR: {name} was not called: its provider is rate limiting requests (HTTP 429), retry in about {} s. Answer from the results you already have, or say the search is rate limited.", seconds(wait_ms))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_a_retry_after_wins_and_a_success_clears() {
        let l = ToolRateLimit::default();
        assert_eq!(l.hit_at("k", None, 0.0), 15_000.0);
        assert_eq!(l.hit_at("k", None, 1.0), 30_000.0);
        assert_eq!(l.hit_at("k", Some(500.0), 2.0), 1000.0);
        assert_eq!(l.hit_at("k", Some(9e9), 3.0), MAX_MS);
        l.clear("k");
        assert_eq!(l.size(), 0);
        assert_eq!(l.hit_at("k", Some(f64::NAN), 0.0), 15_000.0);
    }

    #[test]
    fn the_text_detector_matches_the_pattern() {
        for yes in [
            "HTTP 429",
            "status code: 429",
            "status 429!",
            "Rate limited",
            "rate-limit",
            "ratelimiting",
            "Too Many Requests",
            "Quota exceeded",
            "usage limit reached",
        ] {
            assert!(is_rate_limited_text(yes), "{yes}");
        }
        for no in [
            "http 4290",
            "migrate limited",
            "rate  limit",
            "ok",
            "status codes 429",
            "xHTTP 429",
        ] {
            assert!(!is_rate_limited_text(no), "{no}");
        }
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        assert_eq!(parse_retry_after(Some("3"), 0.0), Some(3000.0));
        assert_eq!(parse_retry_after(Some(" 1.5 "), 0.0), Some(1500.0));
        assert_eq!(parse_retry_after(None, 0.0), None);
        assert_eq!(parse_retry_after(Some(""), 0.0), None);
        let at = parse_retry_after(Some("Thu, 01 Jan 1970 00:00:10 GMT"), 4000.0);
        assert_eq!(at, Some(6000.0));
        assert_eq!(parse_retry_after(Some("soon"), 0.0), None);
    }
}
