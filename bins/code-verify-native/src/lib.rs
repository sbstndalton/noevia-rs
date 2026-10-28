//! Executable-private boundaries for the complete native verifier (unpublished work in progress).
//! These routines do not establish execution, isolation, cleanup or measured-report authority.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_TAIL_BYTES: usize = 256 * 1024;
// Internal Git stdout capture is larger; operator test tails are separately capped above.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
pub const MAX_SAFE_COUNTER: u64 = 9_007_199_254_740_991;

fn js_space(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | '\u{20}' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
/// Node's positive operator option conversion, within the native JS-safe duration range.
/// Fractional positive values can floor to zero; constructor-specific defaults are separate.
pub fn operator_positive(raw: Option<&str>, fallback: u64) -> u64 {
    let Some(raw) = raw else { return fallback };
    let text = raw.trim_matches(js_space);
    let number = if let Some((digits, radix)) = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .map(|s| (s, 16))
        .or_else(|| {
            text.strip_prefix("0o")
                .or_else(|| text.strip_prefix("0O"))
                .map(|s| (s, 8))
        })
        .or_else(|| {
            text.strip_prefix("0b")
                .or_else(|| text.strip_prefix("0B"))
                .map(|s| (s, 2))
        }) {
        if digits.is_empty() {
            return fallback;
        }
        let mut value = 0.0;
        for c in digits.chars() {
            let Some(d) = c.to_digit(radix) else {
                return fallback;
            };
            value = value * f64::from(radix) + f64::from(d);
        }
        value
    } else {
        match text.parse::<f64>() {
            Ok(n) => n,
            Err(_) => return fallback,
        }
    };
    if !number.is_finite() || number <= 0.0 {
        return fallback;
    }
    number.min(MAX_SAFE_COUNTER as f64).floor() as u64
}

/// ECMAScript operator-field trim, including FEFF and excluding NEL.
pub fn operator_trim(raw: &str) -> &str {
    raw.trim_matches(js_space)
}
fn repo_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
fn lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The order and first-valid-entry semantics match the operator's Node configuration.
/// Deliberately no Debug implementation: commands are not diagnostic output.
#[derive(Clone, Default)]
pub struct Commands(Vec<(String, String)>);
impl Commands {
    pub fn parse(raw: &str) -> Self {
        let mut entries = Vec::new();
        for line in raw.split('\n') {
            let Some((name, command)) = line.split_once('|') else {
                continue;
            };
            let name = name.trim_matches(js_space);
            let command = command.trim_matches(js_space);
            if !repo_name(name)
                || command.is_empty()
                || command.encode_utf16().count() > 4096
                || entries.iter().any(|(n, _)| n == name)
            {
                continue;
            }
            entries.push((name.to_owned(), command.to_owned()));
        }
        Self(entries)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.as_str())
    }
    pub fn entries(&self) -> &[(String, String)] {
        &self.0
    }
}

/// A resolved path can be embedded only as a quoted Git configuration value.
/// Filesystem containment and descriptor ownership must be checked separately.
pub fn config_safe_path(s: &str) -> bool {
    !s.is_empty()
        && !s
            .chars()
            .any(|c| matches!(c, '\0'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '"' | '\\'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    noevia: String,
    repo: String,
    source: String,
    #[serde(rename = "headSha")]
    head_sha: String,
    nonce: String,
}
/// A request names an operator-configured job; it never supplies a command.
/// No Debug implementation, since a source path/nonce is not a public diagnostic.
pub struct Request {
    pub repo: String,
    pub source: String,
    pub head_sha: String,
    pub nonce: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    TooLong,
    Invalid,
}
impl Request {
    /// Parse exactly the first line, excluding its terminating newline.
    /// The accepted amendment is a UTF-8 byte cap including that newline.
    pub fn parse_line(line: &[u8]) -> Result<Self, RequestError> {
        if line.len() >= MAX_REQUEST_BYTES {
            return Err(RequestError::TooLong);
        }
        let v: WireRequest = serde_json::from_slice(line).map_err(|_| RequestError::Invalid)?;
        if v.noevia != "verify"
            || !repo_name(&v.repo)
            || !matches!(v.head_sha.len(), 40 | 64)
            || !lower_hex(&v.head_sha)
            || !(32..=128).contains(&v.nonce.len())
            || !lower_hex(&v.nonce)
            || v.source.is_empty()
            || v.source.contains('\0')
        {
            return Err(RequestError::Invalid);
        }
        Ok(Self {
            repo: v.repo,
            source: v.source,
            head_sha: v.head_sha,
            nonce: v.nonce,
        })
    }
}

/// A rolling byte tail. Invalid UTF-8 is output data, never a protocol verdict.
pub struct Tail {
    bytes: VecDeque<u8>,
    cap: usize,
    total: u64,
}
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TailOutput {
    pub tail: String,
    pub tail_bytes: usize,
    pub total_bytes: u64,
    pub truncated: bool,
}
impl Tail {
    pub fn new(cap: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            cap: cap.clamp(1, MAX_CAPTURE_BYTES),
            total: 0,
        }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        self.total = self
            .total
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .min(MAX_SAFE_COUNTER);
        if bytes.len() >= self.cap {
            self.bytes.clear();
            self.bytes.extend(
                bytes
                    .iter()
                    .skip(bytes.len().saturating_sub(self.cap))
                    .copied(),
            );
        } else {
            let discard = self
                .bytes
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(self.cap);
            self.bytes.drain(..discard);
            self.bytes.extend(bytes.iter().copied());
        }
    }
    pub fn read(&self) -> TailOutput {
        let raw: Vec<u8> = self.bytes.iter().copied().collect();
        TailOutput {
            tail: String::from_utf8_lossy(&raw).into_owned(),
            tail_bytes: raw.len(),
            total_bytes: self.total,
            truncated: self.total > u64::try_from(raw.len()).unwrap_or(u64::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn oracle() -> Result<Value, Box<dyn std::error::Error>> {
        Ok(serde_json::from_str(include_str!(
            "../tests/fixtures/boundaries.json"
        ))?)
    }
    #[test]
    fn frozen_node_operator_commands_paths_and_byte_tails() -> Result<(), Box<dyn std::error::Error>>
    {
        let v = oracle()?;
        for x in v
            .get("numericOptions")
            .and_then(Value::as_array)
            .ok_or("numeric fixtures")?
        {
            let raw = x
                .get("input")
                .and_then(Value::as_str)
                .ok_or("numeric input")?;
            let expected = x
                .get("expected")
                .and_then(Value::as_u64)
                .ok_or("numeric expected")?;
            assert_eq!(operator_positive(Some(raw), 123), expected);
        }
        let configs = v
            .get("configs")
            .and_then(Value::as_array)
            .ok_or("config fixtures")?;
        for x in configs {
            let raw = x
                .get("input")
                .and_then(Value::as_str)
                .ok_or("config input")?;
            let expected: Vec<(String, String)> =
                serde_json::from_value(x.get("expected").ok_or("config expected")?.clone())?;
            assert_eq!(Commands::parse(raw).entries(), expected);
        }
        for x in v
            .get("paths")
            .and_then(Value::as_array)
            .ok_or("path fixtures")?
        {
            let raw = x.get("input").and_then(Value::as_str).ok_or("path input")?;
            assert_eq!(
                config_safe_path(raw),
                x.get("expected")
                    .and_then(Value::as_bool)
                    .ok_or("path expected")?
            );
        }
        for x in v
            .get("tails")
            .and_then(Value::as_array)
            .ok_or("tail fixtures")?
        {
            let cap = usize::try_from(x.get("cap").and_then(Value::as_u64).ok_or("tail cap")?)?;
            let chunks: Vec<Vec<u8>> =
                serde_json::from_value(x.get("chunks").ok_or("tail chunks")?.clone())?;
            let mut tail = Tail::new(cap);
            for chunk in chunks {
                tail.push(&chunk);
            }
            let expected: TailOutput =
                serde_json::from_value(x.get("expected").ok_or("tail expected")?.clone())?;
            assert_eq!(tail.read(), expected);
        }
        Ok(())
    }
    fn request() -> Value {
        json!({"noevia":"verify", "repo":"app", "source":"/source/app", "headSha":"a".repeat(40), "nonce":"b".repeat(32)})
    }
    #[test]
    fn malformed_or_command_bearing_requests_never_become_jobs(
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert!(Request::parse_line(&serde_json::to_vec(&request())?).is_ok());
        for (key, value) in [
            ("command", json!("touch /tmp/injected")),
            ("headSha", json!("A".repeat(40))),
            ("nonce", json!("b".repeat(31))),
            ("source", json!("/source/\0x")),
            ("repo", json!("../app")),
            ("repo", json!("雪")),
        ] {
            let mut raw = request();
            raw.as_object_mut()
                .ok_or("object")?
                .insert(key.to_owned(), value);
            assert!(Request::parse_line(&serde_json::to_vec(&raw)?).is_err());
        }
        let raw = serde_json::to_string(&request())?;
        let duplicate = raw.strip_suffix('}').ok_or("object end")?.to_owned()
            + ",\"nonce\":\""
            + &"c".repeat(32)
            + "\"}";
        assert!(Request::parse_line(duplicate.as_bytes()).is_err());
        assert!(Request::parse_line(b"\xff\xfe").is_err());
        Ok(())
    }
    #[test]
    fn request_cap_is_bytes_and_includes_newline() -> Result<(), Box<dyn std::error::Error>> {
        let mut v = request();
        v.as_object_mut().ok_or("object")?.insert(
            "source".to_owned(),
            json!("雪".repeat(MAX_REQUEST_BYTES / 3)),
        );
        assert_eq!(
            Request::parse_line(&serde_json::to_vec(&v)?).err(),
            Some(RequestError::TooLong)
        );
        let base = serde_json::to_vec(&request())?;
        let mut exact = base.clone();
        exact.resize(MAX_REQUEST_BYTES - 1, b' ');
        assert!(Request::parse_line(&exact).is_ok());
        exact.push(b' ');
        assert_eq!(
            Request::parse_line(&exact).err(),
            Some(RequestError::TooLong)
        );
        Ok(())
    }
    #[test]
    fn large_output_keeps_a_bounded_tail_without_overflow() {
        let mut tail = Tail::new(8);
        tail.push(&vec![b'x'; 1024 * 1024]);
        tail.push(b"last");
        assert_eq!(tail.read().tail, "xxxxlast");
        assert_eq!(tail.read().total_bytes, 1_048_580);
        tail.total = MAX_SAFE_COUNTER - 1;
        tail.push(b"abc");
        assert_eq!(tail.read().total_bytes, MAX_SAFE_COUNTER);
    }
}
