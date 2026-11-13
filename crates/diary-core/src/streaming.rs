//! The progress stream of one Diary exchange (port of `diary/agent/streaming.py`), without I/O.
//!
//! The stream opens with a fixed status frame, carries the exchange's events (never a model's
//! reasoning), then the diary result and `done`, or one error event that names no provider detail.
//! A comment frame keeps an idle connection open.
//!
//! D5: frames are `data: <JSON>\n\n` with the same JSON values as the Python frames, but serialised
//! compactly with sorted keys (Python writes `json.dumps` spacing, insertion order and `\u` escapes).
//! Every consumer parses the JSON; the shadow comparison compares parsed events.

use serde_json::{json, Map, Value};

use crate::llm::{LlmError, EMPTY_ANSWER_TEXT, LENGTH_CAP_TEXT};

/// The first frame, byte for byte as the Python service writes it.
pub const STATUS_FRAME: &str =
    "data: {\"type\":\"status\",\"text\":\"Opening diary context…\"}\n\n";
/// Sent when no event arrived for [`KEEP_ALIVE_AFTER_S`] seconds.
pub const KEEP_ALIVE_FRAME: &str = ": keep-alive\n\n";
pub const KEEP_ALIVE_AFTER_S: u64 = 5;
/// At most this many events wait for a slow reader (Python `queue.Queue(maxsize=256)`).
pub const QUEUE_LIMIT: usize = 256;
pub const HEADERS: [(&str, &str); 3] = [
    ("cache-control", "no-cache, no-transform"),
    ("content-type", "text/event-stream; charset=utf-8"),
    ("x-accel-buffering", "no"),
];
/// #1505: the companion's answer comes before any save, so a deadline passing while it was still
/// generating means nothing was written.
pub const MODEL_TIMEOUT_TEXT: &str =
    "The local model did not answer in time — nothing was saved. You can resend.";
pub const TEMPORAL_UNAVAILABLE_TEXT: &str =
    "Diary date filtering is unavailable. Nothing was saved; try again later.";
pub const FAILURE_TEXT: &str =
    "Diary processing failed. Saving may be incomplete; check the saved entry before sending again.";

/// How an exchange failed, as the stream tells it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Native temporal focus unavailable: no context was sent to a model.
    TemporalUnavailable,
    /// The exchange deadline passed; `nothing_saved` when no save had begun.
    Timeout {
        nothing_saved: bool,
    },
    LengthCap,
    EmptyAnswer,
    /// Anything else: never described to the user.
    Other,
}

impl From<&LlmError> for Failure {
    fn from(e: &LlmError) -> Self {
        match e {
            LlmError::LengthCap => Self::LengthCap,
            LlmError::EmptyAnswer => Self::EmptyAnswer,
            _ => Self::Other,
        }
    }
}

/// The error event for a failure (Python `exchange_stream.run`'s except branches).
pub fn error_event(failure: Failure) -> Value {
    match failure {
        Failure::TemporalUnavailable => {
            json!({"type": "error", "code": "contextUnavailable", "saved": false, "text": TEMPORAL_UNAVAILABLE_TEXT})
        }
        Failure::Timeout {
            nothing_saved: true,
        } => {
            json!({"type": "error", "code": "modelFailed", "reason": "chat_timeout", "saved": false, "text": MODEL_TIMEOUT_TEXT})
        }
        Failure::LengthCap => {
            json!({"type": "error", "code": "modelFailed", "reason": "length_cap", "saved": false, "text": LENGTH_CAP_TEXT})
        }
        Failure::EmptyAnswer => {
            json!({"type": "error", "code": "modelFailed", "reason": "empty_answer", "saved": false, "text": EMPTY_ANSWER_TEXT})
        }
        Failure::Timeout {
            nothing_saved: false,
        }
        | Failure::Other => json!({"type": "error", "text": FAILURE_TEXT}),
    }
}

/// #1505: a model's reasoning is never shown to the user, whatever produced it.
pub fn shown(event: &Value) -> bool {
    event.get("type") != Some(&json!("reasoning"))
}

/// `{"type": "diary", **result}`: a `type` key in the result replaces "diary", as in Python.
pub fn result_event(result: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), json!("diary"));
    out.extend(result.clone());
    Value::Object(out)
}

pub fn done_event() -> Value {
    json!({"type": "done"})
}

/// One event frame.
pub fn frame(event: &Value) -> String {
    format!("data: {event}\n\n")
}
