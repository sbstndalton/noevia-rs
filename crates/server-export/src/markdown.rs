//! Actual core965 `chatMarkdown` policy for explicitly admitted plain-data values.
//! No parser, clock, filesystem, history ownership, or HTTP route lives here.
use chrono::{Datelike, NaiveDate, Timelike, Utc};
use js_json::JValue;
use std::collections::HashSet;

// Per chat (meta and history together). Sized for the export route (#1315), whose whole payload
// is bounded at 64 MiB and whose history files are read up to 12 MiB each (#1824, #1830).
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_NODES: usize = 1_000_000;
pub const MAX_DEPTH: usize = 32;
pub const MAX_RENDER_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limit,
    Allocation,
    /// An admitted operation would throw Node TypeError.
    Type,
    /// A numeric Date lies outside ECMAScript TimeClip's range (Node RangeError).
    Date,
    /// Date form/year outside this library's explicitly supported admission.
    UnsupportedDate,
    /// A manually constructed object has duplicate keys (not canonical parsed JValue).
    UnsupportedInput,
}

struct Budget {
    nodes: usize,
    bytes: usize,
}
impl Budget {
    fn nodes(&mut self, n: usize) -> Result<(), Error> {
        self.nodes = self.nodes.checked_add(n).ok_or(Error::Limit)?;
        if self.nodes > MAX_NODES {
            return Err(Error::Limit);
        }
        Ok(())
    }
    fn bytes(&mut self, n: usize) -> Result<(), Error> {
        self.bytes = self.bytes.checked_add(n).ok_or(Error::Limit)?;
        if self.bytes > MAX_INPUT_BYTES {
            return Err(Error::Limit);
        }
        Ok(())
    }
}
// Charge all siblings before reserving any per-object key set. Across a recursive path,
// sets cannot reserve more keys than the global node budget, including refused input.
fn preflight(v: &JValue, depth: usize, budget: &mut Budget) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Limit);
    }
    match v {
        JValue::Str(s) => budget.bytes(s.len())?,
        JValue::Arr(items) => {
            budget.nodes(items.len())?;
            for item in items {
                preflight(item, depth + 1, budget)?;
            }
        }
        JValue::Obj(items) => {
            budget.nodes(items.len())?;
            let mut keys = HashSet::new();
            keys.try_reserve(items.len())
                .map_err(|_| Error::Allocation)?;
            for (key, item) in items {
                budget.bytes(key.len())?;
                if !keys.insert(key.as_str()) {
                    return Err(Error::UnsupportedInput);
                }
                preflight(item, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}
struct Buffer(String);
impl Buffer {
    fn new() -> Self {
        Self(String::new())
    }
    fn push(&mut self, s: &str) -> Result<(), Error> {
        let len = self.0.len().checked_add(s.len()).ok_or(Error::Limit)?;
        if len > MAX_RENDER_BYTES {
            return Err(Error::Limit);
        }
        if len > self.0.capacity() {
            // Bounded geometric growth: total copied bytes are linear, tiny chats do
            // not retain a maximum-sized buffer, and every growth is fallible.
            let mut target = self.0.capacity().max(64);
            while target < len {
                target = target
                    .checked_mul(2)
                    .ok_or(Error::Limit)?
                    .min(MAX_RENDER_BYTES);
            }
            self.0
                .try_reserve_exact(target.checked_sub(self.0.len()).ok_or(Error::Limit)?)
                .map_err(|_| Error::Allocation)?;
        }
        self.0.push_str(s);
        Ok(())
    }
    fn clear(&mut self) {
        self.0.clear();
    }
}
// String-hint ToPrimitive for plain JSON data. Inherited Object.toString is callable
// even when an own valueOf is not; unlike js-json::to_js_string, do not refuse that case.
fn coerce(v: &JValue, out: &mut Buffer) -> Result<(), Error> {
    match v {
        JValue::Undefined => out.push("undefined"),
        JValue::Null => out.push("null"),
        JValue::Bool(b) => out.push(if *b { "true" } else { "false" }),
        // Existing Number::toString implementation uses only fixed scalar-sized temporaries.
        JValue::Num(n) => out.push(&js_json::number_to_string(*n)),
        JValue::Str(s) => out.push(s),
        JValue::Obj(items) => {
            if items.iter().any(|(key, _)| key == "toString") {
                return Err(Error::Type);
            }
            out.push("[object Object]")
        }
        JValue::Arr(items) => {
            for (i, item) in items.iter().enumerate() {
                if i != 0 {
                    out.push(",")?;
                }
                if !item.is_nullish() {
                    coerce(item, out)?;
                }
            }
            Ok(())
        }
    }
}
fn is_space(c: char) -> bool {
    u16::try_from(u32::from(c)).is_ok_and(policy_leaves::is_js_space)
}
fn digits(s: &str) -> Option<u32> {
    s.bytes().try_fold(0u32, |n, b| {
        if !b.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u32::from(b - b'0'))
    })
}
fn canonical_millis(s: &str) -> Option<i64> {
    if s.len() != 24 || !s.is_ascii() {
        return None;
    }
    for (i, expected) in [
        (4, "-"),
        (7, "-"),
        (10, "T"),
        (13, ":"),
        (16, ":"),
        (19, "."),
        (23, "Z"),
    ] {
        if s.get(i..i + 1)? != expected {
            return None;
        }
    }
    let year = i32::try_from(digits(s.get(0..4)?)?).ok()?;
    let date = NaiveDate::from_ymd_opt(year, digits(s.get(5..7)?)?, digits(s.get(8..10)?)?)?;
    let time = date.and_hms_milli_opt(
        digits(s.get(11..13)?)?,
        digits(s.get(14..16)?)?,
        digits(s.get(17..19)?)?,
        digits(s.get(20..23)?)?,
    )?;
    Some(time.and_utc().timestamp_millis())
}
fn put_digits(out: &mut [u8], mut n: u32) -> Result<(), Error> {
    for b in out.iter_mut().rev() {
        *b = b'0' + u8::try_from(n % 10).map_err(|_| Error::UnsupportedDate)?;
        n /= 10;
    }
    if n != 0 {
        return Err(Error::UnsupportedDate);
    }
    Ok(())
}
fn iso(v: &JValue) -> Result<[u8; 24], Error> {
    let ms = match v {
        JValue::Num(n) => {
            if !n.is_finite() || n.abs() > 8_640_000_000_000_000.0 {
                return Err(Error::Date);
            }
            n.trunc() as i64 // TimeClip truncates toward zero; range above fits i64.
        }
        JValue::Bool(true) => 1,
        JValue::Str(s) => canonical_millis(s).ok_or(Error::UnsupportedDate)?,
        _ => return Err(Error::UnsupportedDate),
    };
    let date = chrono::DateTime::<Utc>::from_timestamp_millis(ms).ok_or(Error::UnsupportedDate)?;
    let year = u32::try_from(date.year()).map_err(|_| Error::UnsupportedDate)?;
    if year > 9999 {
        return Err(Error::UnsupportedDate);
    }
    let mut out = *b"0000-00-00T00:00:00.000Z";
    for (start, end, n) in [
        (0, 4, year),
        (5, 7, date.month()),
        (8, 10, date.day()),
        (11, 13, date.hour()),
        (14, 16, date.minute()),
        (17, 19, date.second()),
        (20, 23, date.timestamp_subsec_millis()),
    ] {
        put_digits(out.get_mut(start..end).ok_or(Error::UnsupportedDate)?, n)?;
    }
    Ok(out)
}

/// Render actual admitted Node conversation Markdown. Entire input, including ignored
/// fields/roles, is preflight-bounded before any formatting. See README for date refusals.
pub fn render(chat: &JValue, history: &JValue) -> Result<String, Error> {
    let mut budget = Budget { nodes: 2, bytes: 0 };
    preflight(chat, 0, &mut budget)?;
    preflight(history, 0, &mut budget)?;
    if chat.is_nullish() {
        return Err(Error::Type);
    }
    let mut out = Buffer::new();
    let mut scratch = Buffer::new();
    out.push("# ")?;
    let title = chat.get("title");
    if title.truthy() {
        coerce(title, &mut out)?;
    } else {
        out.push("Untitled chat")?;
    }
    out.push("\n")?;
    let updated = chat.get("updatedAt");
    if updated.truthy() {
        out.push("\n_Last updated ")?;
        out.push(std::str::from_utf8(&iso(updated)?).map_err(|_| Error::UnsupportedDate)?)?;
        out.push("_\n")?;
    }
    let messages = match history {
        JValue::Arr(items) => Some(items),
        JValue::Str(_) => None, // String's iterable characters never have user/assistant role.
        _ => return Err(Error::Type),
    };
    if let Some(messages) = messages {
        for message in messages {
            if message.is_nullish() {
                return Err(Error::Type);
            }
            let role = message.get("role").as_str();
            if role != Some("user") && role != Some("assistant") {
                continue;
            }
            out.push(if role == Some("user") {
                "\n## You\n\n"
            } else {
                "\n## Assistant\n\n"
            })?;
            scratch.clear();
            let content = message.get("content");
            if content.truthy() {
                coerce(content, &mut scratch)?;
            }
            let trimmed = scratch.0.trim_matches(is_space);
            // sbstndalton/noevia#2116: an assistant turn with a non-empty `failure` string is a reply that failed, not an
            // answer. core's chatMarkdown (chat-export.cjs) says the same words.
            let failed = role == Some("assistant")
                && message
                    .get("failure")
                    .as_str()
                    .is_some_and(|f| !f.trim_matches(is_space).is_empty());
            if failed {
                if trimmed.is_empty() {
                    out.push("_(The reply failed; no answer was written.)_")?;
                } else {
                    out.push(trimmed)?;
                    out.push("\n\n_(The reply failed before it finished.)_")?;
                }
            } else {
                out.push(if trimmed.is_empty() {
                    "_(no text)_"
                } else {
                    trimmed
                })?;
            }
            out.push("\n")?;
            let tools = message.get("toolCalls");
            if tools.truthy() {
                let JValue::Arr(items) = tools else {
                    return Err(Error::Type);
                };
                scratch.clear();
                let mut any = false;
                for tool in items {
                    if !tool.truthy() {
                        continue;
                    }
                    let name = tool.get("name");
                    if !name.truthy() {
                        continue;
                    }
                    if any {
                        scratch.push(", ")?;
                    }
                    coerce(name, &mut scratch)?;
                    any = true;
                }
                if any {
                    out.push("\n_Tools used: ")?;
                    out.push(&scratch.0)?;
                    out.push("_\n")?;
                }
            }
        }
    }
    Ok(out.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_output_does_not_reserve_the_maximum() -> Result<(), Error> {
        let mut out = Buffer::new();
        out.push("tiny")?;
        assert!(out.0.capacity() < MAX_RENDER_BYTES);
        for _ in 0..1000 {
            out.push("abc")?;
        }
        assert!(out.0.capacity() >= out.0.len());
        assert!(out.0.capacity() <= out.0.len() * 2 + 64);
        Ok(())
    }
    #[test]
    fn output_and_arithmetic_limits_refuse() -> Result<(), Error> {
        let mut out = Buffer::new();
        out.push(&"x".repeat(MAX_RENDER_BYTES))?;
        assert_eq!(out.push("x"), Err(Error::Limit));
        let mut budget = Budget {
            nodes: usize::MAX,
            bytes: usize::MAX,
        };
        assert_eq!(budget.nodes(1), Err(Error::Limit));
        assert_eq!(budget.bytes(1), Err(Error::Limit));
        Ok(())
    }
}
