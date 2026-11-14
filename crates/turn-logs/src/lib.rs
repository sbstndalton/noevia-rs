//! The two durable, text-free logs the Node process keeps beside a chat turn, ported from core
//! `server/decision-log.cjs` and `server/context-log.cjs` (behaviour, not lines):
//!
//! - [`decision`]: one JSON line per routing/supervision/tool-gate/outcome decision, bounded to one
//!   current file plus one rotated file (`system-one-decisions.jsonl` / `.1`), never failing a turn;
//!   [`decision::summarize`] counts them for reading back.
//! - [`context`]: opt-in (`CONTEXT_LOG=1`) per-round context accounting: estimated tokens by part and
//!   tool *names* only, the chat id hashed; [`context::report`] ranks what consumes context.
//!
//! Both read-back summaries answer JavaScript's values (key order, `String(key)` buckets) so the
//! `turn-logs` binary prints what `node server/decision-log.cjs` / `node server/context-log.cjs` print.
//! Nothing here calls a model or reads message text into a record. Unused by the front until the
//! native chat turn takes these writes over (inert crate).
pub mod context;
pub mod decision;
pub mod pretty;

use js_json::{wtf16, JValue};

/// `bucket[String(key)] = (bucket[String(key)] || 0) + 1` on a plain JS object, skipping
/// `undefined`/`null` keys as core's `bump` does. A `__proto__` key is dropped (in JS the
/// assignment hits the prototype setter and is ignored), so the counts match Node's output.
pub(crate) fn bump(bucket: &mut Vec<(String, JValue)>, key: &JValue) {
    if key.is_nullish() {
        return;
    }
    let Some(name) = prop_key(key) else { return };
    let next = match bucket.iter().find(|(k, _)| *k == name) {
        Some((_, JValue::Num(n))) => n + 1.0,
        _ => 1.0,
    };
    wtf16::set(bucket, &name, JValue::Num(next));
}

/// The property name a value becomes as an object key (`String(value)`), or `None` when JS would
/// not create an own property (`__proto__`) or `String()` throws (an object with its own
/// `toString`, which JS reports as a TypeError: core's summary would throw; this one skips).
pub(crate) fn prop_key(key: &JValue) -> Option<String> {
    let name = js_json::to_js_string(key).ok()?;
    (name != "__proto__").then_some(name)
}

/// The value a JSON line parses to, if it is an object Node could read fields from. Core reads
/// `e.kind` straight away, so a line holding `null` throws a TypeError there and ends the whole
/// summary (sbstndalton/noevia issue filed by council p12-2); a non-object line is skipped here
/// instead, like a line that is not JSON.
pub(crate) fn parse_line(raw: &str) -> Option<JValue> {
    match wtf16::parse(raw) {
        Ok(JValue::Null) | Err(_) => None,
        Ok(v) => Some(v),
    }
}
