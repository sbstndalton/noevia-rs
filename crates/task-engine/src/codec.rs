//! The JSON Lines form of a journal: one `{ job, seq, type, at, data }` object per line.
//!
//! Unlike jobs.cjs, a torn last line is refused, not dropped, and a row with a hash chain
//! (`hash` or `previous`) is refused: this slice does not verify chains, so it fails closed.

use serde_json::Value;

use crate::error::EngineError;
use crate::event::{to_object, Event, MAX_EVENTS_PER_JOB};

/// One line per event, each ending in `\n`.
pub fn to_jsonl(events: &[Event]) -> String {
    let mut out = String::new();
    for event in events {
        // A `Value` of plain strings and numbers always encodes; the fallback keeps this total.
        let line = serde_json::to_string(&to_object(event)).unwrap_or_default();
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parses journal rows. Blank lines are skipped. Any other malformed row refuses the whole journal.
pub fn parse_jsonl(text: &str) -> Result<Vec<Event>, EngineError> {
    let mut rows = Vec::new();
    for line in text.split('\n').filter(|l| !l.is_empty()) {
        if rows.len() >= MAX_EVENTS_PER_JOB {
            return Err(EngineError::Limit("job has too many events"));
        }
        let value: Value =
            serde_json::from_str(line).map_err(|_| EngineError::BadJournal("a row is not JSON"))?;
        rows.push(row_to_event(&value)?);
    }
    Ok(rows)
}

fn row_to_event(value: &Value) -> Result<Event, EngineError> {
    let obj = value
        .as_object()
        .ok_or(EngineError::BadJournal("a row is not an object"))?;
    if obj.contains_key("hash") || obj.contains_key("previous") {
        return Err(EngineError::BadJournal(
            "hash-chained journals are not supported yet",
        ));
    }
    let text = |key: &str| obj.get(key).and_then(Value::as_str).map(str::to_string);
    let number = |key: &str| obj.get(key).and_then(Value::as_u64);
    let data = obj
        .get("data")
        .filter(|d| d.is_object())
        .cloned()
        .ok_or(EngineError::BadJournal("a row has no data object"))?;
    Ok(Event {
        job: text("job").ok_or(EngineError::BadJournal("a row has no job id"))?,
        seq: number("seq").ok_or(EngineError::BadJournal("a row has no seq"))?,
        kind: text("type").ok_or(EngineError::BadJournal("a row has no type"))?,
        at: number("at").ok_or(EngineError::BadJournal("a row has no time"))?,
        data,
    })
}
