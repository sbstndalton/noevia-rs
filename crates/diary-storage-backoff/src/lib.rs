//! Pure decisions for Diary remote-storage cooldowns.
//!
//! No HTTP transport, credentials, registry, mutex, filesystem, database, or
//! wall/monotonic clock is owned here. The caller supplies clock samples and
//! serializes access to a gate shared by one credential fingerprint.
use chrono::NaiveDateTime;

mod email_date;
mod unicode;

pub const LOGIN_COOLDOWN_S: f64 = 300.0;
pub const DEFAULT_RETRY_AFTER_S: f64 = 60.0;
pub const MAX_RETRY_AFTER_S: f64 = 3600.0;
pub const RETRY_RESET_MIN_INTERVAL_S: f64 = 30.0;
/// Explicit bound for untrusted HTTP header text; oversize returns the default.
pub const MAX_HEADER_BYTES: usize = 8192;
/// Finite clock domain for deterministic arithmetic (~31,000 years of seconds).
pub const MAX_CLOCK_ABS_S: f64 = 1.0e12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAfterSource {
    Absent,
    Numeric,
    HttpDate,
    Invalid,
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryAfter {
    pub seconds: f64,
    pub source: RetryAfterSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    Login,
    Throttle,
}

impl GateKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Throttle => "throttle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    NonFiniteOrOutOfRange,
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("clock sample must be finite and within ±1e12 seconds")
    }
}

impl std::error::Error for ClockError {}

fn valid_clock(value: f64) -> bool {
    value.is_finite() && value.abs() <= MAX_CLOCK_ABS_S
}

fn py_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

fn numeric_text(s: &str) -> Option<String> {
    let normalized: String = s
        .chars()
        .map(|c| {
            unicode::decimal(c)
                .and_then(|digit| char::from_digit(digit, 10))
                .unwrap_or(c)
        })
        .collect();
    let chars: Vec<char> = normalized.chars().collect();
    let mut out = String::with_capacity(normalized.len());
    for (index, c) in chars.iter().copied().enumerate() {
        if c == '_' {
            if index == 0
                || !chars.get(index - 1).is_some_and(char::is_ascii_digit)
                || !chars.get(index + 1).is_some_and(char::is_ascii_digit)
            {
                return None;
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn numeric_seconds(s: &str) -> Option<f64> {
    let normalized = numeric_text(s)?;
    if normalized.is_empty() {
        return None;
    }
    // Python float accepts case-insensitive inf/infinity and nan. Rust's
    // parser handles most spellings, but handle all Python spellings directly.
    let lower = normalized.to_ascii_lowercase();
    let unsigned = lower
        .strip_prefix('+')
        .or_else(|| lower.strip_prefix('-'))
        .unwrap_or(&lower);
    if unsigned == "inf" || unsigned == "infinity" {
        return Some(if lower.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if unsigned == "nan" {
        return Some(f64::NAN);
    }
    normalized.parse::<f64>().ok()
}

/// Parse a Retry-After delta or HTTP date against an explicit Unix wall-clock
/// sample. Numeric values use Python's Unicode-decimal/underscore `float`
/// syntax. The default is returned unchanged for absent or invalid headers.
/// Date grammar follows shipped Python email.utils; this convenience entry
/// interprets naive/unknown-zone dates under UTC. A non-UTC caller must use
/// `parse_retry_after_with_local_date` with its actual local-date resolver.
/// A header over `MAX_HEADER_BYTES` returns the default.
pub fn parse_retry_after(value: Option<&str>, default: f64, wall_now: f64) -> RetryAfter {
    parse_retry_after_with_local_date(value, default, wall_now, |date| {
        Some(date.and_utc().timestamp() as f64)
    })
}

/// Full shipped email-date grammar with host-supplied naive-date resolution.
/// The resolver must reproduce the caller's Python local timezone, including
/// DST/fold and timestamp refusal. It is never called for explicit offsets.
/// Returning None treats an unresolvable local date as invalid, retaining the
/// supplied default; this function performs no timezone/clock/transport IO.
pub fn parse_retry_after_with_local_date(
    value: Option<&str>,
    default: f64,
    wall_now: f64,
    mut resolve_local: impl FnMut(NaiveDateTime) -> Option<f64>,
) -> RetryAfter {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return RetryAfter {
            seconds: default,
            source: RetryAfterSource::Absent,
        };
    };
    if value.len() > MAX_HEADER_BYTES {
        return RetryAfter {
            seconds: default,
            source: RetryAfterSource::TooLong,
        };
    }
    let text = value.trim_matches(py_space);
    let numeric = numeric_seconds(text);
    let (seconds, source) = if let Some(seconds) = numeric {
        (seconds, RetryAfterSource::Numeric)
    } else if valid_clock(wall_now) {
        if let Some(timestamp) = email_date::timestamp(text, &mut resolve_local) {
            (timestamp - wall_now, RetryAfterSource::HttpDate)
        } else {
            (default, RetryAfterSource::Invalid)
        }
    } else {
        (default, RetryAfterSource::Invalid)
    };
    if seconds.is_nan() {
        RetryAfter {
            seconds: default,
            source: RetryAfterSource::Invalid,
        }
    } else if source == RetryAfterSource::Invalid {
        RetryAfter {
            seconds: default,
            source,
        }
    } else {
        RetryAfter {
            seconds: seconds.clamp(1.0, MAX_RETRY_AFTER_S),
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateState {
    pub kind: Option<GateKind>,
    pub remaining: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Blocked {
    pub kind: Option<GateKind>,
    pub remaining: f64,
    /// Mirrors `StorageBackoff`: `max(1, int(remaining + 0.999))`.
    pub retry_after: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordEffect {
    LoginCooldown,
    ThrottleCooldown,
    KeptActiveLogin,
    ClearedLogin,
    Unchanged,
}

/// One credential's pure state. Keep it behind the caller's shared lock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StorageGate {
    until: f64,
    kind: Option<GateKind>,
    last_reset: f64,
}

impl Default for StorageGate {
    fn default() -> Self {
        Self {
            until: 0.0,
            kind: None,
            last_reset: -RETRY_RESET_MIN_INTERVAL_S,
        }
    }
}

impl StorageGate {
    pub fn new() -> Self {
        Self::default()
    }

    fn remaining(&self, now: f64) -> Result<f64, ClockError> {
        if !valid_clock(now) {
            return Err(ClockError::NonFiniteOrOutOfRange);
        }
        Ok(self.until - now)
    }

    pub fn state(&self, now_mono: f64) -> Result<GateState, ClockError> {
        let remaining = self.remaining(now_mono)?;
        if remaining > 0.0 {
            Ok(GateState {
                kind: self.kind,
                remaining,
            })
        } else {
            Ok(GateState {
                kind: None,
                remaining: 0.0,
            })
        }
    }

    pub fn check(&self, now_mono: f64) -> Result<Option<Blocked>, ClockError> {
        let state = self.state(now_mono)?;
        if state.remaining <= 0.0 {
            return Ok(None);
        }
        let retry_after = ((state.remaining + 0.999).trunc() as u64).max(1);
        Ok(Some(Blocked {
            kind: state.kind,
            remaining: state.remaining,
            retry_after,
        }))
    }

    pub fn record(
        &mut self,
        now_mono: f64,
        wall_now: f64,
        status: u16,
        retry_after: Option<&str>,
        root: bool,
    ) -> Result<RecordEffect, ClockError> {
        self.record_with_local_date(now_mono, wall_now, status, retry_after, root, |date| {
            Some(date.and_utc().timestamp() as f64)
        })
    }

    /// Same transitions with caller-supplied local-date resolution, for hosts
    /// whose Python naive timestamps are not UTC. See the parser counterpart.
    pub fn record_with_local_date(
        &mut self,
        now_mono: f64,
        wall_now: f64,
        status: u16,
        retry_after: Option<&str>,
        root: bool,
        resolve_local: impl FnMut(NaiveDateTime) -> Option<f64>,
    ) -> Result<RecordEffect, ClockError> {
        if !valid_clock(now_mono) || !valid_clock(wall_now) {
            return Err(ClockError::NonFiniteOrOutOfRange);
        }
        if status == 401 || (status == 403 && root) {
            self.kind = Some(GateKind::Login);
            self.until = now_mono + LOGIN_COOLDOWN_S;
            return Ok(RecordEffect::LoginCooldown);
        }
        if status == 429 {
            let until = now_mono
                + parse_retry_after_with_local_date(
                    retry_after,
                    DEFAULT_RETRY_AFTER_S,
                    wall_now,
                    resolve_local,
                )
                .seconds;
            if self.kind == Some(GateKind::Login) && self.until > now_mono {
                return Ok(RecordEffect::KeptActiveLogin);
            }
            self.until = until.max(if self.kind == Some(GateKind::Throttle) {
                self.until
            } else {
                0.0
            });
            self.kind = Some(GateKind::Throttle);
            return Ok(RecordEffect::ThrottleCooldown);
        }
        if status < 400 && self.kind == Some(GateKind::Login) {
            self.kind = None;
            self.until = 0.0;
            return Ok(RecordEffect::ClearedLogin);
        }
        Ok(RecordEffect::Unchanged)
    }

    pub fn reset_login(&mut self, now_mono: f64) -> Result<bool, ClockError> {
        if !valid_clock(now_mono) {
            return Err(ClockError::NonFiniteOrOutOfRange);
        }
        if self.kind == Some(GateKind::Login)
            && self.until > now_mono
            && now_mono - self.last_reset >= RETRY_RESET_MIN_INTERVAL_S
        {
            self.kind = None;
            self.until = 0.0;
            self.last_reset = now_mono;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_default_and_reports_limit() {
        let long = "1".repeat(MAX_HEADER_BYTES + 1);
        let parsed = parse_retry_after(Some(&long), 7.25, 1_000.0);
        assert_eq!(parsed.seconds, 7.25);
        assert_eq!(parsed.source, RetryAfterSource::TooLong);
        let nan = parse_retry_after(Some("NaN"), 7.25, 1_000.0);
        assert_eq!(nan.seconds, 7.25);
        assert_eq!(nan.source, RetryAfterSource::Invalid);
    }

    #[test]
    fn invalid_clock_cannot_mutate_gate() {
        let mut gate = StorageGate::new();
        let before = gate;
        assert_eq!(
            gate.record(f64::NAN, 1_000.0, 401, None, true),
            Err(ClockError::NonFiniteOrOutOfRange)
        );
        assert_eq!(gate, before);
        assert_eq!(
            gate.reset_login(MAX_CLOCK_ABS_S + 1.0),
            Err(ClockError::NonFiniteOrOutOfRange)
        );
        assert_eq!(gate, before);
    }

    #[test]
    fn negative_fake_clock_matches_initial_python_gate() -> Result<(), ClockError> {
        let gate = StorageGate::new();
        let state = gate.state(-1.0)?;
        assert_eq!(state.kind, None);
        assert_eq!(state.remaining, 1.0);
        let blocked = gate.check(-1.0)?;
        assert_eq!(blocked.map(|value| value.retry_after), Some(1));
        assert_eq!(blocked.and_then(|value| value.kind), None);
        Ok(())
    }
}
