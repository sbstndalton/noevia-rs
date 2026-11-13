//! Journal events and the bounds on them (jobs.cjs `TYPES`, `MAX_*`, `valid job id`).

use serde_json::{Map, Value};

use crate::error::EngineError;

/// The journal vocabulary: jobs.cjs `TYPES` without the two server-only lifecycle authority types
/// (see [`AUTHORITY_TYPES`]). Plan, review and assistant-output events are stored as given.
pub const EVENT_TYPES: [&str; 23] = [
    "job.created",
    "job.started",
    "step.started",
    "step.completed",
    "progress",
    "approval.requested",
    "approval.decided",
    "tool.started",
    "tool.completed",
    "tool.uncertain",
    "artifact.created",
    "checkpoint.created",
    "job.completed",
    "job.failed",
    "job.cancelled",
    "job.interrupted",
    "plan.proposed",
    "plan.edited",
    "plan.skipped",
    "assistant.output",
    "review.requested",
    "review.completed",
    "review.failed",
];

/// Lifecycle authority events (#701). The public append path refuses them; only the pipeline may
/// write them, and this crate has no such path yet.
pub const AUTHORITY_TYPES: [&str; 2] = ["task.stage", "task.revision"];

/// The largest JSON encoding of one event's `data`.
pub const MAX_EVENT_DATA_BYTES: usize = 64 * 1024;

/// The most events one job journal may hold.
pub const MAX_EVENTS_PER_JOB: usize = 4096;

/// `^[0-9a-f-]{36}$`, the job id shape jobs.cjs accepts.
pub fn valid_job_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// One journal row: jobs.cjs `{ job, seq, type, at, data }`, without the hash-chain fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub job: String,
    pub seq: u64,
    pub kind: String,
    pub at: u64,
    pub data: Value,
}

/// Checks the type, the data shape and the data size of an event before it is written.
pub(crate) fn check_event(kind: &str, data: &Value) -> Result<(), EngineError> {
    if AUTHORITY_TYPES.contains(&kind) {
        return Err(EngineError::ServerOnly);
    }
    if !EVENT_TYPES.contains(&kind) {
        return Err(EngineError::UnknownType);
    }
    if !data.is_object() {
        return Err(EngineError::BadInput("event data must be an object"));
    }
    if encoded_len(data)? > MAX_EVENT_DATA_BYTES {
        return Err(EngineError::TooLarge("event data is over 64 KiB"));
    }
    Ok(())
}

/// The JSON byte length of `value`, or a refusal when it cannot be encoded.
pub(crate) fn encoded_len(value: &Value) -> Result<usize, EngineError> {
    serde_json::to_vec(value)
        .map(|v| v.len())
        .map_err(|_| EngineError::BadInput("value cannot be encoded as JSON"))
}

/// The object form of an event, in jobs.cjs key order.
pub(crate) fn to_object(event: &Event) -> Value {
    let mut map = Map::new();
    map.insert("job".into(), Value::String(event.job.clone()));
    map.insert("seq".into(), Value::from(event.seq));
    map.insert("type".into(), Value::String(event.kind.clone()));
    map.insert("at".into(), Value::from(event.at));
    map.insert("data".into(), event.data.clone());
    Value::Object(map)
}
