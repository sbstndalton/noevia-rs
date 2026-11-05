//! GET /api/workspace-trash, ported from services diary/agent/workspace_trash.py (`list_trash`,
//! `decode`, `summary`, `identity`) and app.py `api_workspace_trash_list`: one page of the
//! recovery capsules (`.noevia-trash/<operation id>.json`) of an app-managed Diary.
//!
//! Python is lenient where this side is strict (`uuid.UUID`, `json.loads`, `base64.b64decode`).
//! Wherever the two could read a stored value differently this side DECLINES and Python answers;
//! it answers itself only when both are certain to agree.

use crate::managed::{open_ro, Managed};
use crate::workspace::{safe_path, Reply, MAX_FILE};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = ".noevia-trash/";
const PAGE: usize = 100;
const BAD_ID: &str = "A valid recovery operation ID is required";

/// What `identity()` makes of a value: the canonical id, a refusal (ValueError → 409), or a form
/// only Python's `uuid.UUID` can judge.
enum Ident {
    Id(String),
    Invalid,
    Unsure,
}

/// `str(uuid.UUID(value))` for the forms whose reading is certain: hex digits and hyphens only.
/// `UUID` drops every hyphen and needs exactly 32 hex digits; anything else it accepts or refuses
/// (urn:uuid:, braces, int()'s own number syntax) is left to Python.
fn identity(value: &str) -> Ident {
    if !value.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Ident::Unsure;
    }
    let hex: String = value
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if hex.len() != 32 {
        return Ident::Invalid;
    }
    match (
        hex.get(0..8),
        hex.get(8..12),
        hex.get(12..16),
        hex.get(16..20),
        hex.get(20..32),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e)) => Ident::Id(format!("{a}-{b}-{c}-{d}-{e}")),
        _ => Ident::Unsure,
    }
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `summary(record)`: the keys in Python's order (serde_json's own map would sort them).
#[derive(Serialize)]
#[serde(untagged)]
enum Record {
    Valid {
        id: Value,
        path: Value,
        version: Value,
        state: Value,
        #[serde(rename = "trashedAt")]
        trashed_at: Value,
    },
    Invalid {
        id: String,
        path: Option<()>,
        version: Option<()>,
        state: &'static str,
        #[serde(rename = "trashedAt")]
        trashed_at: Option<()>,
        invalid: bool,
    },
}

#[derive(Serialize)]
struct Page {
    records: Vec<Record>,
    next: Option<String>,
}

enum Decoded {
    Summary(Record),
    Invalid,
    Decline,
}

/// `base64.b64decode(data, validate=True)` for canonical input; Ok(None): Python refuses it
/// (outside the alphabet or padding shape); Err: only Python can judge it.
fn strict_b64(data: &str) -> Result<Option<Vec<u8>>, ()> {
    let body = data.trim_end_matches('=');
    let pad = data.len() - body.len();
    if !body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
        || pad > 2
    {
        return Ok(None);
    }
    // Python's regex accepted the shape; the decoder's own padding rules are left to it unless the
    // input is exactly what an encoder writes.
    if data.len() % 4 != 0 || (pad == 0 && body.len() % 4 != 0) {
        return Err(());
    }
    let val = |b: u8| -> u32 {
        match b {
            b'A'..=b'Z' => u32::from(b - b'A'),
            b'a'..=b'z' => u32::from(b - b'a') + 26,
            b'0'..=b'9' => u32::from(b - b'0') + 52,
            b'+' => 62,
            _ => 63,
        }
    };
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    let bytes = body.as_bytes();
    for chunk in bytes.chunks(4) {
        let n = chunk.iter().fold(0u32, |acc, b| (acc << 6) | val(*b));
        match chunk.len() {
            4 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            3 => {
                // trailing bits must be zero for a canonical encoding
                if n & 0x3 != 0 {
                    return Err(());
                }
                out.extend_from_slice(&[(n >> 10) as u8, (n >> 2) as u8]);
            }
            2 => {
                if n & 0xf != 0 {
                    return Err(());
                }
                out.push((n >> 4) as u8);
            }
            _ => return Err(()),
        }
    }
    Ok(Some(out))
}

/// A number Python's float repr would print the way serde_json does (plain decimal notation).
fn plain_number(n: &serde_json::Number) -> bool {
    match n.as_f64() {
        Some(f) if n.is_f64() => f.is_finite() && (f == 0.0 || (1e-4..1e16).contains(&f.abs())),
        _ => true,
    }
}

/// `decode()` then `summary()` for one capsule.
fn decode(data: &[u8], operation: &str) -> Decoded {
    let text = String::from_utf8_lossy(data);
    // Python's json.loads also takes NaN/Infinity, a BOM, UTF-16/32 and nesting this parser
    // refuses: when the strict parse fails and any of that could be in play, decline.
    let lenient = data.contains(&0)
        || data.starts_with(&[0xef, 0xbb, 0xbf])
        || text.contains("NaN")
        || text.contains("Infinity");
    let record: Value = match serde_json::from_slice(data) {
        Ok(v) => v,
        Err(e) => {
            return if lenient || e.to_string().contains("recursion limit") {
                Decoded::Decline
            } else {
                Decoded::Invalid
            };
        }
    };
    let Value::Object(map) = &record else {
        return Decoded::Invalid;
    };
    let get = |k: &str| map.get(k);
    if get("format").and_then(Value::as_str) != Some("noevia-trash-v1")
        || get("id").and_then(Value::as_str) != Some(operation)
        || !matches!(
            get("state").and_then(Value::as_str),
            Some("trashed" | "restored")
        )
    {
        return Decoded::Invalid;
    }
    let Some(path) = get("path").and_then(Value::as_str) else {
        return Decoded::Invalid;
    };
    if safe_path(path, false).is_err() {
        return Decoded::Invalid;
    }
    let Some(encoded) = get("data").and_then(Value::as_str) else {
        return Decoded::Invalid;
    };
    let raw = match strict_b64(encoded) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Decoded::Invalid,
        Err(()) => return Decoded::Decline,
    };
    if raw.len() > MAX_FILE || get("version").and_then(Value::as_str) != Some(&sha256_hex(&raw)) {
        return Decoded::Invalid;
    }
    // summary(): a missing key is a KeyError outside decode's try (Python's 500): decline.
    let pick = |key: &str| -> Option<Value> {
        match map.get(key) {
            Some(Value::Number(n)) if !plain_number(n) => None,
            other => other.cloned(),
        }
    };
    let (Some(id), Some(path), Some(version), Some(state), Some(trashed_at)) = (
        pick("id"),
        pick("path"),
        pick("version"),
        pick("state"),
        pick("trashedAt"),
    ) else {
        return Decoded::Decline;
    };
    Decoded::Summary(Record::Valid {
        id,
        path,
        version,
        state,
        trashed_at,
    })
}

fn detail(status: u16, text: &str) -> Reply {
    match serde_json::to_vec(&json!({ "detail": text })) {
        Ok(body) => Reply::Json(status, body),
        Err(_) => Reply::Decline,
    }
}

/// `list_trash(store, after)` for a ready managed tenant (`require_managed` holds by
/// construction). The Ok body goes out with `Cache-Control: no-store`.
pub fn list(store: &Managed, after: &str) -> Reply {
    let after = if after.is_empty() {
        None
    } else {
        match identity(after) {
            Ident::Id(id) => Some(id),
            Ident::Invalid => return detail(409, BAD_ID),
            Ident::Unsure => return Reply::Decline,
        }
    };
    let Ok(conn) = open_ro(store.db_path()) else {
        return Reply::Decline;
    };
    let bound = match &after {
        Some(id) => format!("{PREFIX}{id}.json"),
        None => PREFIX.to_string(),
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT path,data FROM files WHERE substr(path,1,?)=? AND path>? ORDER BY path LIMIT 101",
    ) else {
        return Reply::Decline;
    };
    let rows: Result<Vec<(String, Vec<u8>)>, _> = stmt
        .query_map(
            rusqlite::params![
                i64::try_from(PREFIX.chars().count()).unwrap_or(0),
                PREFIX,
                bound
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )
        .and_then(|it| it.collect());
    let Ok(rows) = rows else {
        return Reply::Decline;
    };
    let mut records: Vec<Record> = Vec::new();
    let mut last_id = String::new();
    for (path, data) in rows.iter().take(PAGE) {
        // row['path'][len(PREFIX):-5], by code point
        let chars: Vec<char> = path.chars().collect();
        let name: String = chars
            .get(PREFIX.chars().count()..chars.len().saturating_sub(5))
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        let operation = match identity(&name) {
            Ident::Id(id) => id,
            Ident::Invalid => return detail(409, BAD_ID),
            Ident::Unsure => return Reply::Decline,
        };
        match decode(data, &operation) {
            Decoded::Summary(v) => records.push(v),
            Decoded::Invalid => records.push(Record::Invalid {
                id: operation.clone(),
                path: None,
                version: None,
                state: "invalid",
                trashed_at: None,
                invalid: true,
            }),
            Decoded::Decline => return Reply::Decline,
        }
        last_id = operation;
    }
    let next = (rows.len() > PAGE).then_some(last_id);
    match serde_json::to_vec(&Page { records, next }) {
        Ok(body) => Reply::JsonNoStore(body),
        Err(_) => Reply::Decline,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn identity_reads_only_what_it_is_sure_of() {
        let id = "0b5f6c1e-3a2d-4c8e-9f10-1a2b3c4d5e6f";
        assert!(matches!(identity(id), Ident::Id(ref s) if s == id));
        assert!(matches!(identity(&id.to_uppercase()), Ident::Id(ref s) if s == id));
        assert!(matches!(identity(&id.replace('-', "")), Ident::Id(ref s) if s == id));
        assert!(matches!(identity("0b5f6c1e-3a2d"), Ident::Invalid));
        assert!(matches!(identity("not-a-uuid"), Ident::Unsure));
        assert!(matches!(
            identity("urn:uuid:0b5f6c1e3a2d4c8e9f101a2b3c4d5e6f"),
            Ident::Unsure
        ));
        assert!(matches!(
            identity("{0b5f6c1e3a2d4c8e9f101a2b3c4d5e6f}"),
            Ident::Unsure
        ));
    }

    #[test]
    fn strict_base64_matches_an_encoders_output() {
        assert_eq!(strict_b64("QUJD").unwrap().unwrap(), b"ABC");
        assert_eq!(strict_b64("QUI=").unwrap().unwrap(), b"AB");
        assert_eq!(strict_b64("QQ==").unwrap().unwrap(), b"A");
        assert_eq!(strict_b64("").unwrap().unwrap(), b"");
        assert!(strict_b64("QU Jd").unwrap().is_none());
        assert!(strict_b64("QQ===").unwrap().is_none());
        assert!(strict_b64("QUJD\n").unwrap().is_none());
        // shapes the regex admits but only Python's decoder can judge
        assert!(strict_b64("QQ").is_err());
        assert!(strict_b64("QR==").is_err());
    }
}
