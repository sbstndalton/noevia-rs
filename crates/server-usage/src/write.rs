//! The usage writer (track R12): core `server/usage.cjs` `recordUsage` and `recordToolUse`, the daily
//! rollups in `users/<id>/usage.json`. Pure: the file's bytes in, the bytes to write out (or why not).
//!
//! In Node's terms: `readUsage` (a file that does not parse, or whose `days` is not an object, is
//! `{ days: {} }`; anything else at the top level is kept), the day of the local civil date, the reply's
//! tokens and model, the reply's local hour, the tool's name cut to 80 UTF-16 units, the days before
//! the 365-day cutoff dropped (a string comparison of the keys), and `atomicJson`'s bytes
//! (`JSON.stringify(store, null, 2)`). The clock is the caller's ([`Clock`]).
//!
//! Difference from Node, fail-closed: a file Node wrote holds numbers in every count, objects in
//! `models`, `hours` and `tools`. A hand-edited file that holds anything else where this adds (a
//! string count, a list of days, a model entry that is not an object) is left exactly as it is and the
//! record is skipped ([`Outcome::Unusual`]), where Node's `+=` would concatenate strings or write
//! `null`. A file this port cannot hold (a number past the `f64` range, deep nesting) is skipped too.

use js_json::{wtf16, JValue};

/// `USAGE_RETENTION_DAYS`.
pub const RETENTION_DAYS: i64 = 365;

/// When the record is made, in the server's local time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clock {
    /// `usageDayKey()`: `YYYY-MM-DD` of the local civil date.
    pub day: String,
    /// `new Date().getHours()`.
    pub hour: u32,
    /// `usageDayKey(new Date(Date.now() - 365 * 86400000))`.
    pub cutoff: String,
}

/// What to do with the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Write these bytes.
    Write(String),
    /// Node returns without writing (no usage, no tokens, no tool name).
    Nothing,
    /// The file holds what this port does not add to (see the module header): left as it is.
    Unusual,
}

/// JS `StringToNumber` (`Number(text)`): whitespace trimmed, '' is 0, decimal, `Infinity`, and the
/// unsigned `0x`/`0o`/`0b` forms; anything else NaN.
fn string_to_number(text: &str) -> f64 {
    let is_space = |c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    };
    let s = text.trim_matches(is_space);
    if s.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = s.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut n = 0.0f64;
            for c in digits.chars() {
                let Some(d) = c.to_digit(radix) else {
                    return f64::NAN;
                };
                n = n * f64::from(radix) + f64::from(d);
            }
            return n;
        }
    }
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, s.strip_prefix('+').unwrap_or(s)),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    // StrDecimalLiteral: digits with at most one '.', at least one digit, an optional exponent.
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
        digits += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return f64::NAN;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return f64::NAN;
        }
    }
    if i != bytes.len() {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// Where Node's code throws (caught there: nothing is recorded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thrown;

/// JS `Number(value)` for a JSON value.
fn to_number(v: &JValue) -> Result<f64, Thrown> {
    Ok(match v {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Num(n) => *n,
        JValue::Str(s) => string_to_number(s),
        // ToPrimitive (hint number): valueOf gives the object back, then toString.
        other => string_to_number(&js_json::to_js_string(other).map_err(|_| Thrown)?),
    })
}

/// `Number(x) || 0`.
fn number_or_zero(v: &JValue) -> Result<f64, Thrown> {
    let n = to_number(v)?;
    Ok(if n.is_nan() || n == 0.0 { 0.0 } else { n })
}

/// `readUsage(workspace)` on the file's bytes. `Err(())`: a file this port cannot hold.
fn read(stored: Option<&[u8]>) -> Result<Vec<(String, JValue)>, ()> {
    let empty = || vec![("days".to_string(), JValue::obj(Vec::<(String, JValue)>::new()))];
    let Some(bytes) = stored else {
        return Ok(empty());
    };
    let parsed = match wtf16::parse(&String::from_utf8_lossy(bytes)) {
        Ok(v) => v,
        Err(wtf16::Refusal::Invalid) => return Ok(empty()),
        Err(wtf16::Refusal::Unsupported) => return Err(()),
    };
    // `parsed && typeof parsed.days === 'object' && parsed.days ? parsed : { days: {} }`
    match &parsed {
        JValue::Obj(items) if matches!(parsed.get("days"), JValue::Obj(_) | JValue::Arr(_)) => {
            Ok(items.clone())
        }
        _ => Ok(empty()),
    }
}

/// `store.days` as an object to change, or `None` when it is a list (unusual).
fn days_of(store: &mut [(String, JValue)]) -> Option<&mut Vec<(String, JValue)>> {
    match store.iter_mut().find(|(k, _)| k == "days").map(|(_, v)| v) {
        Some(JValue::Obj(days)) => Some(days),
        _ => None,
    }
}

/// `store.days[key] || { input: 0, output: 0, replies: 0, models: {} }`, or `None` for a day that is
/// truthy but not an object (unusual).
fn day_of(days: &[(String, JValue)], key: &str) -> Option<Vec<(String, JValue)>> {
    match days.iter().find(|(k, _)| k == key).map(|(_, v)| v) {
        Some(JValue::Obj(items)) => Some(items.clone()),
        Some(v) if v.truthy() => None,
        _ => Some(vec![
            ("input".into(), JValue::Num(0.0)),
            ("output".into(), JValue::Num(0.0)),
            ("replies".into(), JValue::Num(0.0)),
            ("models".into(), JValue::obj(Vec::<(String, JValue)>::new())),
        ]),
    }
}

fn get<'a>(items: &'a [(String, JValue)], key: &str) -> &'a JValue {
    items
        .iter()
        .find(|(k, _)| k == key)
        .map_or(&JValue::Undefined, |(_, v)| v)
}

/// `obj[key] += n` where the stored value must be a number (else unusual).
fn add(items: &mut Vec<(String, JValue)>, key: &str, n: f64) -> Option<()> {
    let JValue::Num(now) = get(items, key) else {
        return None;
    };
    let next = now + n;
    wtf16::set(items, key, JValue::Num(next));
    Some(())
}

/// `Object.assign(Object.create(null), obj[key] || {})` where the value must be an object or falsy
/// (else unusual).
fn object_field(items: &[(String, JValue)], key: &str) -> Option<Vec<(String, JValue)>> {
    match get(items, key) {
        JValue::Obj(fields) => Some(fields.clone()),
        v if !v.truthy() => Some(Vec::new()),
        _ => None,
    }
}

/// `obj[key] = (Number(obj[key]) || 0) + 1` (`None` where `Number()` throws: Node records nothing).
fn bump(items: &mut Vec<(String, JValue)>, key: &str) -> Option<()> {
    let now = number_or_zero(get(items, key)).ok()?;
    wtf16::set(items, key, JValue::Num(now + 1.0));
    Some(())
}

/// `atomicJson`'s bytes, after the retention trim.
fn finish(mut store: Vec<(String, JValue)>, key: &str, day: Vec<(String, JValue)>, cutoff: Option<&str>) -> Outcome {
    let Some(days) = days_of(&mut store) else {
        return Outcome::Unusual;
    };
    wtf16::set(days, key, JValue::Obj(day));
    if let Some(cutoff) = cutoff {
        // `k < cutoff`: UTF-16 code unit order.
        days.retain(|(k, _)| !k.encode_utf16().lt(cutoff.encode_utf16()));
    }
    wtf16::stringify_pretty(&JValue::Obj(store)).map_or(Outcome::Unusual, Outcome::Write)
}

/// usage.cjs `recordUsage(workspace, model, usage)` at `clock`.
pub fn record_usage(stored: Option<&[u8]>, model: &JValue, usage: &JValue, clock: &Clock) -> Outcome {
    if !usage.truthy() {
        return Outcome::Nothing;
    }
    let (Ok(input), Ok(output)) = (
        number_or_zero(usage.get("promptTokens")),
        number_or_zero(usage.get("completionTokens")),
    ) else {
        return Outcome::Nothing;
    };
    if !input.is_finite() || !output.is_finite() || input < 0.0 || output < 0.0 || (input == 0.0 && output == 0.0) {
        return Outcome::Nothing;
    }
    let name = if model.truthy() {
        match js_json::to_js_string(model) {
            Ok(n) => n,
            Err(_) => return Outcome::Nothing,
        }
    } else {
        "unknown".to_string()
    };
    let Ok(mut store) = read(stored) else {
        return Outcome::Unusual;
    };
    let Some(days) = days_of(&mut store) else {
        return Outcome::Unusual;
    };
    let Some(mut day) = day_of(days, &clock.day) else {
        return Outcome::Unusual;
    };
    let changed = (|| -> Option<()> {
        add(&mut day, "input", input)?;
        add(&mut day, "output", output)?;
        add(&mut day, "replies", 1.0)?;
        let mut models = object_field(&day, "models")?;
        let mut per_model = match get(&models, &name) {
            JValue::Obj(fields) => fields.clone(),
            v if !v.truthy() => vec![
                ("input".into(), JValue::Num(0.0)),
                ("output".into(), JValue::Num(0.0)),
                ("replies".into(), JValue::Num(0.0)),
            ],
            _ => return None,
        };
        add(&mut per_model, "input", input)?;
        add(&mut per_model, "output", output)?;
        add(&mut per_model, "replies", 1.0)?;
        wtf16::set(&mut models, &name, JValue::Obj(per_model));
        wtf16::set(&mut day, "models", JValue::Obj(models));
        let mut hours = object_field(&day, "hours")?;
        bump(&mut hours, &clock.hour.to_string())?;
        wtf16::set(&mut day, "hours", JValue::Obj(hours));
        Some(())
    })();
    if changed.is_none() {
        return Outcome::Unusual;
    }
    finish(store, &clock.day, day, Some(&clock.cutoff))
}

/// usage.cjs `recordToolUse(workspace, name)` at `clock` (no retention trim, as in Node).
pub fn record_tool(stored: Option<&[u8]>, name: &JValue, clock: &Clock) -> Outcome {
    if !name.truthy() {
        return Outcome::Nothing;
    }
    let Ok(text) = js_json::to_js_string(name) else {
        return Outcome::Nothing;
    };
    let tool = wtf16::slice(&text, 80);
    let Ok(mut store) = read(stored) else {
        return Outcome::Unusual;
    };
    let Some(days) = days_of(&mut store) else {
        return Outcome::Unusual;
    };
    let Some(mut day) = day_of(days, &clock.day) else {
        return Outcome::Unusual;
    };
    let Some(mut tools) = object_field(&day, "tools") else {
        return Outcome::Unusual;
    };
    if bump(&mut tools, &tool).is_none() {
        return Outcome::Unusual;
    }
    wtf16::set(&mut day, "tools", JValue::Obj(tools));
    finish(store, &clock.day, day, None)
}

/// `usageDayKey(at)` and the hour, for a local date-time.
fn day_key<T: chrono::Datelike>(at: &T) -> String {
    format!("{:04}-{:02}-{:02}", at.year(), at.month(), at.day())
}

/// The clock at `now_ms` in time zone `tz` (as Node's local time with that `TZ`).
pub fn clock_in<Z: chrono::TimeZone>(now_ms: i64, tz: &Z) -> Option<Clock> {
    use chrono::Timelike;
    let at = chrono::DateTime::from_timestamp_millis(now_ms)?.with_timezone(tz);
    let before = chrono::DateTime::from_timestamp_millis(now_ms - RETENTION_DAYS * 86_400_000)?
        .with_timezone(tz);
    Some(Clock {
        day: day_key(&at),
        hour: at.hour(),
        cutoff: day_key(&before),
    })
}

/// The clock now, in the process's local time (the `TZ` Node's process has: same container).
pub fn clock_now() -> Option<Clock> {
    let now = chrono::Utc::now().timestamp_millis();
    clock_in(now, &chrono::Local)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn number_rules() {
        for (s, n) in [("", 0.0), (" 12 ", 12.0), ("0x1f", 31.0), ("1e3", 1000.0), (".5", 0.5), ("5.", 5.0), ("-Infinity", f64::NEG_INFINITY)] {
            assert_eq!(string_to_number(s), n, "{s}");
        }
        for s in ["1e", "e3", "-0x1", "inf", "NaN", "1 2", "."] {
            assert!(string_to_number(s).is_nan(), "{s}");
        }
    }
}
