//! Framing and validation of the control exchange, mirroring the oracle's `responseHeaders`,
//! `closedKeys`, `reportValue` and `endpointAddress`.
use crate::{err, ControlError, Reason};
use serde_json::Value;

pub const MAX_REQUEST: usize = 32 * 1024;
pub const MAX_RESPONSE: usize = 64 * 1024;
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn is_hex(text: &str, bytes: usize) -> bool {
    text.len() == bytes * 2
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if !is_hex(text, N) {
        return None;
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        let s = std::str::from_utf8(pair).ok()?;
        *slot = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

/// `TASK = /^[A-Za-z0-9._:-]{1,128}$/`.
pub fn valid_task(task: &str) -> bool {
    !task.is_empty()
        && task.len() <= 128
        && task
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

/// `endpointAddress`: `host:port` or `[v6]:port`.
pub fn endpoint_address(endpoint: &str) -> Result<(String, u16), ControlError> {
    let (host_part, port_text) = endpoint
        .rsplit_once(':')
        .ok_or(ControlError(Reason::Endpoint))?;
    if port_text.is_empty()
        || port_text.len() > 5
        || port_text.starts_with('0')
        || !port_text.bytes().all(|c| c.is_ascii_digit())
    {
        return err(Reason::Endpoint);
    }
    let port: u32 = port_text
        .parse()
        .map_err(|_| ControlError(Reason::Endpoint))?;
    if port == 0 || port > 65535 || host_part.len() > 253 {
        return err(Reason::Endpoint);
    }
    if let Some(inner) = host_part
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
    {
        if inner.parse::<std::net::Ipv6Addr>().is_err()
            || !inner
                .bytes()
                .all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.')
        {
            return err(Reason::Endpoint);
        }
        return Ok((inner.to_string(), port as u16));
    }
    if host_part.is_empty()
        || !host_part
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    {
        return err(Reason::Endpoint);
    }
    Ok((host_part.to_string(), port as u16))
}

/// The response headers the proxy may send, nothing else, once each; the framing must agree with
/// the body. Returns the `x-noevia-control-mac` value.
pub fn response_mac_header(raw: &[(String, String)], body: &[u8]) -> Result<String, ControlError> {
    if raw.len() > 32 {
        return err(Reason::Headers);
    }
    let mut seen: Vec<String> = Vec::new();
    let mut size = 2usize;
    let (mut content_type, mut length, mut cache, mut connection, mut mac) =
        (None, None, None, None, None);
    for (name, value) in raw {
        let name = name.to_ascii_lowercase();
        if seen.contains(&name)
            || !matches!(
                name.as_str(),
                "date"
                    | "content-type"
                    | "content-length"
                    | "cache-control"
                    | "connection"
                    | "x-noevia-control-mac"
            )
        {
            return err(Reason::Headers);
        }
        size += name.len() + value.len() + 4;
        if size > 16384 {
            return err(Reason::Headers);
        }
        match name.as_str() {
            "content-type" => content_type = Some(value.as_str()),
            "content-length" => length = Some(value.as_str()),
            "cache-control" => cache = Some(value.as_str()),
            "connection" => connection = Some(value.as_str()),
            "x-noevia-control-mac" => mac = Some(value.clone()),
            _ => {}
        }
        seen.push(name);
    }
    if content_type != Some("application/json")
        || length != Some(body.len().to_string().as_str())
        || cache != Some("no-store")
        || connection != Some("close")
    {
        return err(Reason::Headers);
    }
    Ok(mac.unwrap_or_default())
}

// ---- JSON number helpers (JS number semantics) ----

pub fn number(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// `Number.isSafeInteger(v)` and `>= 1` (or `>= 0`).
pub fn safe_int_value(v: &Value, positive: bool) -> Result<u64, ControlError> {
    let n = number(v).ok_or(ControlError(Reason::Integer))?;
    if n.fract() != 0.0 || n.abs() > MAX_SAFE_INTEGER || n < if positive { 1.0 } else { 0.0 } {
        return err(Reason::Integer);
    }
    Ok(n as u64)
}

pub fn safe_int(n: u64) -> Result<u64, ControlError> {
    if n == 0 || n as f64 > MAX_SAFE_INTEGER {
        return err(Reason::Integer);
    }
    Ok(n)
}

pub fn closed_keys(v: &Value, required: &[&str], optional: &[&str]) -> Result<(), ControlError> {
    let Some(map) = v.as_object() else {
        return err(Reason::Shape);
    };
    if required.iter().any(|k| !map.contains_key(*k))
        || map
            .keys()
            .any(|k| !required.contains(&k.as_str()) && !optional.contains(&k.as_str()))
    {
        return err(Reason::Shape);
    }
    Ok(())
}

pub fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A validated report (`reportValue`).
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub available: bool,
    pub state: String,
    pub final_: bool,
    pub partial: bool,
    pub overflow: bool,
    pub snapshot_id: Option<String>,
    pub allowed: u64,
    pub refused: u64,
    pub hosts: Vec<HostRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostRow {
    pub host: String,
    pub allowed: u64,
    pub refused: u64,
    pub reason: Option<String>,
}

impl Report {
    /// The shape Node's `reportValue` returned (counts only when the summary is available).
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("available".into(), Value::Bool(self.available));
        m.insert("state".into(), Value::String(self.state.clone()));
        m.insert("final".into(), Value::Bool(self.final_));
        m.insert("partial".into(), Value::Bool(self.partial));
        if let Some(id) = &self.snapshot_id {
            m.insert("overflow".into(), Value::Bool(self.overflow));
            m.insert("snapshotId".into(), Value::String(id.clone()));
        }
        if self.available {
            m.insert("allowed".into(), Value::from(self.allowed));
            m.insert("refused".into(), Value::from(self.refused));
            let rows = self
                .hosts
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "host": h.host, "allowed": h.allowed, "refused": h.refused,
                        "reason": h.reason.as_ref().map_or(Value::Null, |r| Value::String(r.clone())),
                    })
                })
                .collect();
            m.insert("hosts".into(), Value::Array(rows));
        }
        Value::Object(m)
    }
    /// A report with no native snapshot: `{available:false,state,final,partial}`.
    pub fn unavailable(state: &str, final_: bool) -> Self {
        Self {
            available: false,
            state: state.into(),
            final_,
            partial: true,
            overflow: false,
            snapshot_id: None,
            allowed: 0,
            refused: 0,
            hosts: Vec::new(),
        }
    }
}

/// The scope a report or reply must carry (`scope`).
pub struct Scope<'a> {
    pub boot: &'a str,
    pub session: &'a str,
    pub task: &'a str,
    pub nonce: &'a str,
}

pub fn check_scope(body: &Value, scope: &Scope<'_>) -> Result<(), ControlError> {
    if str_of(body, "bootId") != Some(scope.boot)
        || str_of(body, "sessionId") != Some(scope.session)
        || str_of(body, "taskId") != Some(scope.task)
        || str_of(body, "leaseNonce") != Some(scope.nonce)
    {
        return err(Reason::Scope);
    }
    Ok(())
}

pub fn report_value(body: &Value, scope: &Scope<'_>) -> Result<Report, ControlError> {
    closed_keys(
        body,
        &[
            "version",
            "requestId",
            "state",
            "bootId",
            "sessionId",
            "taskId",
            "leaseNonce",
            "barrierInstalled",
            "drained",
            "snapshot",
        ],
        &[],
    )?;
    check_scope(body, scope)?;
    let (Some(_barrier), Some(drained)) = (
        body.get("barrierInstalled").and_then(Value::as_bool),
        body.get("drained").and_then(Value::as_bool),
    ) else {
        return err(Reason::Scope);
    };
    let s = body.get("snapshot").unwrap_or(&Value::Null);
    closed_keys(
        s,
        &["id", "state", "final", "summary", "partial", "overflow"],
        &[],
    )?;
    let id = str_of(s, "id")
        .filter(|id| is_hex(id, 16))
        .ok_or(ControlError(Reason::Identity))?;
    let state = str_of(s, "state").unwrap_or("");
    let (Some(final_), Some(partial), Some(overflow)) = (
        s.get("final").and_then(Value::as_bool),
        s.get("partial").and_then(Value::as_bool),
        s.get("overflow").and_then(Value::as_bool),
    ) else {
        return err(Reason::Snapshot);
    };
    if !["complete", "partial", "interrupted", "evicted", "forgotten"].contains(&state)
        || final_ != drained
    {
        return err(Reason::Snapshot);
    }
    let summary = s.get("summary").unwrap_or(&Value::Null);
    let mut report = Report {
        available: !summary.is_null(),
        state: state.into(),
        final_,
        partial,
        overflow,
        snapshot_id: Some(id.into()),
        allowed: 0,
        refused: 0,
        hosts: Vec::new(),
    };
    if report.available {
        closed_keys(summary, &["allowed", "refused", "hosts"], &[])?;
        report.allowed = safe_int_value(field(summary, "allowed"), false)?;
        report.refused = safe_int_value(field(summary, "refused"), false)?;
        let Some(rows) = field(summary, "hosts").as_array().filter(|r| r.len() <= 50) else {
            return err(Reason::Summary);
        };
        let rendered = serde_json::to_string(summary).map_err(|_| ControlError(Reason::Summary))?;
        if rendered.len() > 48 * 1024 {
            return err(Reason::Summary);
        }
        for row in rows {
            closed_keys(row, &["host", "allowed", "refused", "reason"], &[])?;
            let host = str_of(row, "host")
                .filter(|h| !h.is_empty())
                .ok_or(ControlError(Reason::Summary))?;
            let reason = match field(row, "reason") {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                _ => return err(Reason::Summary),
            };
            report.hosts.push(HostRow {
                host: host.into(),
                allowed: safe_int_value(field(row, "allowed"), false)?,
                refused: safe_int_value(field(row, "refused"), false)?,
                reason,
            });
        }
        for (got, pick) in [(report.allowed, 0usize), (report.refused, 1)] {
            let total: u128 = report
                .hosts
                .iter()
                .map(|h| u128::from(if pick == 0 { h.allowed } else { h.refused }))
                .sum();
            let capped = total.min(MAX_SAFE_INTEGER as u128);
            if u128::from(got) != capped {
                return err(Reason::Summary);
            }
        }
    } else if !["evicted", "forgotten"].contains(&state) {
        return err(Reason::Summary);
    }
    Ok(report)
}
