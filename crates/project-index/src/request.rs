//! The JSON contract a store route answers for the three writes projects.cjs makes, so the front can
//! take them from Node's client the way `/v1/retrieval/search` takes searches (R4's pattern):
//!
//! - `file`: `{projectId, fileName, text, userId?}` → `indexProjectFile`'s outcome;
//! - `tail`: `{projectId, fileName, headText, tail: [{body, page?}], userId?, limit?, fromPage?,
//!   toPage?, final?}` → `appendFileTail`'s outcome (defaults as Node's: limit 8, final true);
//! - `delete`: `{projectId, fileName, userId?, keepMedia?}` → `{ok: true}` or `{ok: false, error}`.
//!
//! Tail items are read as Node reads them (`String(t?.body || '')`, a page only when a positive
//! integer). The ids and texts must be strings: Node's client always sends strings, so anything else
//! is refused as `{ok: false, reason: "bad-request"}` instead of being coerced. An empty `userId` is
//! no account, as in Node.

use crate::index::{Embed, TailChunk};
use crate::indexer::{Host, Indexer};
use serde_json::{json, Map, Value};

fn bad() -> Value {
    json!({ "ok": false, "reason": "bad-request" })
}

fn string<'a>(body: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    body.get(key)?.as_str()
}

/// `userId`: absent, null or a string.
fn user(body: &Map<String, Value>) -> Result<Option<&str>, ()> {
    match body.get("userId") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(()),
    }
}

/// JavaScript truthiness.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `String(v)` for a truthy value.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => crate::embed::js_number_string(n),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        Value::Null => "null".into(),
    }
}

/// `Number.isInteger(v)` as an i64 (None otherwise or out of range).
fn integer(v: Option<&Value>) -> Option<i64> {
    let f = v?.as_f64()?;
    (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
}

/// A tail item as `appendFileTailNow` reads it; None for an empty body (skipped there too).
pub fn tail_item(t: &Value) -> Option<TailChunk> {
    let body = t.get("body").filter(|b| truthy(b)).map(js_string)?;
    let page = integer(t.get("page")).filter(|p| *p > 0);
    Some(TailChunk { body, page })
}

/// Answers one write. `op` is `file`, `tail` or `delete`.
pub fn answer<H: Host>(ix: &Indexer<H>, op: &str, body: &Value, embed: &mut dyn Embed) -> Value {
    let Some(body) = body.as_object() else {
        return bad();
    };
    let (Some(project), Some(file), Ok(user)) = (
        string(body, "projectId"),
        string(body, "fileName"),
        user(body),
    ) else {
        return bad();
    };
    match op {
        "file" => match string(body, "text") {
            Some(text) => ix.index_file(project, file, text, user, embed).to_json(),
            None => bad(),
        },
        "tail" => {
            let Some(head) = string(body, "headText") else {
                return bad();
            };
            // `Array.isArray(tail) ? tail : []`
            let tail: Vec<TailChunk> = match body.get("tail") {
                Some(Value::Array(items)) => items.iter().filter_map(tail_item).collect(),
                _ => Vec::new(),
            };
            let limit = match body.get("limit") {
                None | Some(Value::Null) => 8,
                Some(v) => match integer(Some(v)).and_then(|n| usize::try_from(n).ok()) {
                    Some(n) => n,
                    None => return bad(),
                },
            };
            let final_window = match body.get("final") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => return bad(),
            };
            let window = (
                limit,
                integer(body.get("fromPage")),
                integer(body.get("toPage")),
                final_window,
            );
            match ix.append_tail(project, file, head, &tail, user, window, embed) {
                Ok(out) => out.to_json(),
                Err(e) => json!({ "ok": false, "error": e }),
            }
        }
        "delete" => {
            let keep_media = body.get("keepMedia").is_some_and(truthy);
            match ix.delete_file(project, file, user, keep_media) {
                Ok(()) => json!({ "ok": true }),
                Err(e) => json!({ "ok": false, "error": e }),
            }
        }
        _ => bad(),
    }
}
