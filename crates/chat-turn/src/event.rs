//! The events one chat turn streams to the client: the JSON objects noevia-core's `chat.cjs`
//! writes as SSE `data:` frames (`send({ type: ... })`, chat.cjs lines 654 to 2460).
//!
//! Only the shapes are here. Key order is not part of the contract (the client reads objects),
//! so [`Event::to_json`] builds the same keys in a different order; the replay test compares
//! parsed values. Optional keys are left out when Node leaves them out (`undefined` is dropped
//! by `JSON.stringify`).

use serde_json::{json, Map, Value};

/// The `telemetry` phases Node sends (chat.cjs 1413, 1811, 2451).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Streaming,
    Complete,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Streaming => "streaming",
            Phase::Complete => "complete",
        }
    }
}

/// One streamed event. Variants carry only the fields the draft relays; Node sends more keys on
/// some events (`sources`, `detail`, `notRun` and so on) that later steps add.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// First event of a reply (chat.cjs 1408).
    Meta {
        model: String,
        chat_id: Option<String>,
        route: Option<String>,
    },
    /// `timeToFirstToken` is left out while waiting, `null` or seconds once known (chat.cjs 2451).
    Telemetry {
        phase: Phase,
        model: String,
        time_to_first_token: TimeToFirstToken,
    },
    Status {
        text: String,
        id: Option<String>,
    },
    Delta {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Preamble {
        text: String,
    },
    Tool {
        index: usize,
        name: String,
        args: Value,
    },
    ToolResult {
        index: usize,
        name: String,
        text: String,
        not_run: bool,
    },
    Warning {
        text: String,
    },
    /// `code` is left out unless Node names one (chat.cjs 363, 1203, 1590).
    Error {
        text: String,
        code: Option<String>,
    },
    /// After `done`, once a reply's title is derived (chat.cjs 2470-2490).
    Title {
        chat_id: String,
        title: String,
    },
    /// Last event of a completed reply (chat.cjs 2460).
    Done {
        model: String,
    },
}

/// The three states of Node's `timeToFirstToken` key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeToFirstToken {
    /// Key absent.
    Absent,
    /// Key present, value `null`.
    Unknown,
    /// Key present, seconds.
    Seconds(f64),
}

impl Event {
    /// `done` and `error` end the reply; nothing but `title` may follow them.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Event::Done { .. } | Event::Error { .. })
    }

    /// The `type` string on the wire.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Meta { .. } => "meta",
            Event::Telemetry { .. } => "telemetry",
            Event::Status { .. } => "status",
            Event::Delta { .. } => "delta",
            Event::Reasoning { .. } => "reasoning",
            Event::Preamble { .. } => "preamble",
            Event::Tool { .. } => "tool",
            Event::ToolResult { .. } => "tool_result",
            Event::Warning { .. } => "warning",
            Event::Error { .. } => "error",
            Event::Title { .. } => "title",
            Event::Done { .. } => "done",
        }
    }

    /// The JSON object Node would send for this event.
    pub fn to_json(&self) -> Value {
        let mut obj = Map::new();
        obj.insert("type".into(), json!(self.kind()));
        match self {
            Event::Meta {
                model,
                chat_id,
                route,
            } => {
                obj.insert("model".into(), json!(model));
                put_opt(&mut obj, "chatId", chat_id.as_deref());
                put_opt(&mut obj, "route", route.as_deref());
            }
            Event::Telemetry {
                phase,
                model,
                time_to_first_token,
            } => {
                obj.insert("phase".into(), json!(phase.as_str()));
                obj.insert("model".into(), json!(model));
                match time_to_first_token {
                    TimeToFirstToken::Absent => {}
                    TimeToFirstToken::Unknown => {
                        obj.insert("timeToFirstToken".into(), Value::Null);
                    }
                    TimeToFirstToken::Seconds(s) => {
                        obj.insert("timeToFirstToken".into(), json!(s));
                    }
                }
            }
            Event::Status { text, id } => {
                obj.insert("text".into(), json!(text));
                put_opt(&mut obj, "id", id.as_deref());
            }
            Event::Delta { text }
            | Event::Reasoning { text }
            | Event::Preamble { text }
            | Event::Warning { text } => {
                obj.insert("text".into(), json!(text));
            }
            Event::Tool { index, name, args } => {
                obj.insert("index".into(), json!(index));
                obj.insert("name".into(), json!(name));
                obj.insert("args".into(), args.clone());
            }
            Event::ToolResult {
                index,
                name,
                text,
                not_run,
            } => {
                obj.insert("index".into(), json!(index));
                obj.insert("name".into(), json!(name));
                obj.insert("text".into(), json!(text));
                if *not_run {
                    obj.insert("notRun".into(), json!(true));
                }
            }
            Event::Error { text, code } => {
                obj.insert("text".into(), json!(text));
                put_opt(&mut obj, "code", code.as_deref());
            }
            Event::Title { chat_id, title } => {
                obj.insert("chatId".into(), json!(chat_id));
                obj.insert("title".into(), json!(title));
            }
            Event::Done { model } => {
                obj.insert("model".into(), json!(model));
            }
        }
        Value::Object(obj)
    }

    /// One SSE frame, as `res.write` sends it.
    pub fn frame(&self) -> String {
        format!("data: {}\n\n", self.to_json())
    }
}

fn put_opt(obj: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        obj.insert(key.into(), json!(v));
    }
}
