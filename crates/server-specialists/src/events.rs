//! A coworker's conversation as AG-UI events (sbstndalton/noevia#2446, slice 5, the replay form).
//!
//! [`translate`] is a pure function from the stored messages of the coworker's chat (the
//! transcript, `history-<chat>.json`, as the web client saves it) to [`ag_ui_core::event::Event`]s,
//! in order. [`Store::conversation`] reads those messages. Nothing here writes, and no lock is
//! held while the events are built or sent: the account's specialists lock is taken (and released)
//! only to find the coworker, the transcript is read with no lock at all (every write of it is a
//! rename).
//!
//! # Mapping
//!
//! The thread is the coworker's id. A message's id is its stored `id` (or `message-<position>`
//! for an entry saved without one, the position counted in the history as read, at most the cap). A tool call has no
//! stored id, so its id is `<message id>-call-<n>` for the n-th call of that message.
//!
//! | Stored | Events |
//! |---|---|
//! | user message | `TEXT_MESSAGE_START` (role user), `TEXT_MESSAGE_CONTENT`, `TEXT_MESSAGE_END`, inside the run of the reply that follows (every stream starts with `RUN_STARTED`); a user message with no reply yet gets its own run, finished with `{awaitingReply: true}` |
//! | assistant message | `RUN_STARTED` (run id = the message id) … one terminal event |
//! | its `reasoning` | `THINKING_START`, `THINKING_TEXT_MESSAGE_START`/`CONTENT`/`END`, `THINKING_END` |
//! | each of its `toolCalls` | `TOOL_CALL_START`, `TOOL_CALL_ARGS`, `TOOL_CALL_END`, then one of the three below |
//! | a call that ran | `TOOL_CALL_RESULT` (message id `<message id>-result-<n>`) |
//! | a `space_page_save` result `{pageId, spaceId, title, link}` | after its result, `CUSTOM` named [`PAGE_SAVED`] |
//! | a call declined, never run, or still waiting for approval | `INTERRUPT` (reason `declined`, `not_run`, `approval_pending`) instead of a result |
//! | its text | `TEXT_MESSAGE_START`/`CONTENT`/`END` |
//! | finished reply | `RUN_FINISHED`; `result` carries `{stopped: true}` or `{paused: {reason, applied, declined?}}` when the reply was stopped or paused |
//! | interrupted reply (cut off, kept partway) | `RUN_ERROR`, code `interrupted` |
//! | failed reply | `RUN_ERROR`, code `failed` |
//!
//! Variants kept by Regenerate or Edit (`answers`, `branches`) are not replayed: the conversation
//! is the one on screen. Timestamps are left out, so a replay is the same bytes every time.

use crate::{Error, Store};
use ag_ui_core::event::{
    BaseEvent, CustomEvent, Event, InterruptEvent, InterruptReason, RunErrorEvent,
    RunFinishedEvent, RunStartedEvent, TextMessageContentEvent, TextMessageEndEvent,
    TextMessageStartEvent, ThinkingEndEvent, ThinkingStartEvent, ThinkingTextMessageContentEvent,
    ThinkingTextMessageEndEvent, ThinkingTextMessageStartEvent, ToolCallArgsEvent,
    ToolCallEndEvent, ToolCallResultEvent, ToolCallStartEvent,
};
use ag_ui_core::types::{MessageId, Role, RunId, ThreadId, ToolCallId};
use js_json::JValue;
use serde_json::{json, Value};

/// The tool whose result carries a saved page.
pub const PAGE_SAVE_TOOL: &str = "space_page_save";
/// The custom event announcing a saved page: `{pageId, spaceId, title, link}`.
pub const PAGE_SAVED: &str = "noevia.space_page_saved";

/// `after` names no message of the conversation.
#[derive(Debug, PartialEq, Eq)]
pub struct UnknownAfter;

fn base() -> BaseEvent {
    BaseEvent {
        timestamp: None,
        raw_event: None,
    }
}

fn text_of(v: &JValue) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

/// The stored id of the message at `position`, or the stand-in for one saved without an id.
fn message_id(m: &JValue, position: usize) -> String {
    match m.get("id").as_str() {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => format!("message-{position}"),
    }
}

fn text_message(out: &mut Vec<Event>, id: &str, role: Role, text: &str) {
    out.push(Event::TextMessageStart(TextMessageStartEvent {
        base: base(),
        message_id: MessageId::new(id),
        role,
    }));
    out.push(Event::TextMessageContent(TextMessageContentEvent {
        base: base(),
        message_id: MessageId::new(id),
        delta: text.to_owned(),
    }));
    out.push(Event::TextMessageEnd(TextMessageEndEvent {
        base: base(),
        message_id: MessageId::new(id),
    }));
}

fn thinking(out: &mut Vec<Event>, text: &str) {
    out.push(Event::ThinkingStart(ThinkingStartEvent {
        base: base(),
        title: None,
    }));
    out.push(Event::ThinkingTextMessageStart(
        ThinkingTextMessageStartEvent { base: base() },
    ));
    out.push(Event::ThinkingTextMessageContent(
        ThinkingTextMessageContentEvent {
            base: base(),
            delta: text.to_owned(),
        },
    ));
    out.push(Event::ThinkingTextMessageEnd(ThinkingTextMessageEndEvent {
        base: base(),
    }));
    out.push(Event::ThinkingEnd(ThinkingEndEvent { base: base() }));
}

/// How a stored call ended, from its `status`, `notRun` and `result`.
enum Outcome<'a> {
    Result(&'a str),
    Interrupt(InterruptReason),
}

fn outcome(call: &JValue) -> Outcome<'_> {
    let status = call.get("status").as_str().unwrap_or("");
    if status == "denied" {
        return Outcome::Interrupt(InterruptReason::Declined);
    }
    if call.get("notRun").as_bool() == Some(true) {
        return Outcome::Interrupt(InterruptReason::NotRun);
    }
    match status {
        "pending" => Outcome::Interrupt(InterruptReason::ApprovalPending),
        "running" | "stopped" => Outcome::Interrupt(InterruptReason::NotRun),
        _ => match call.get("result").as_str() {
            Some(result) => Outcome::Result(result),
            None if status == "done" => Outcome::Result(""),
            None => Outcome::Interrupt(InterruptReason::NotRun),
        },
    }
}

fn interrupt_message(reason: InterruptReason) -> &'static str {
    match reason {
        InterruptReason::ApprovalPending => "This call is waiting for approval.",
        InterruptReason::Declined => "This call was declined.",
        InterruptReason::NotRun => "This call did not run.",
    }
}

/// `{pageId, spaceId, title, link}` from a `space_page_save` result, when it is one.
fn saved_page(result: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(result).ok()?;
    let field = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(json!({
        "pageId": field("pageId")?,
        "spaceId": field("spaceId")?,
        "title": v.get("title").and_then(Value::as_str)?,
        "link": field("link")?,
    }))
}

fn tool_calls(out: &mut Vec<Event>, message: &str, calls: &[JValue]) {
    for (n, call) in calls.iter().enumerate() {
        let id = ToolCallId::new(format!("{message}-call-{n}"));
        let name = call.get("name").as_str().unwrap_or("").to_owned();
        out.push(Event::ToolCallStart(ToolCallStartEvent {
            base: base(),
            tool_call_id: id.clone(),
            tool_call_name: name.clone(),
            parent_message_id: Some(MessageId::new(message)),
        }));
        if let Some(args) = text_of(call.get("args")) {
            out.push(Event::ToolCallArgs(ToolCallArgsEvent {
                base: base(),
                tool_call_id: id.clone(),
                delta: args.to_owned(),
            }));
        }
        out.push(Event::ToolCallEnd(ToolCallEndEvent {
            base: base(),
            tool_call_id: id.clone(),
        }));
        match outcome(call) {
            Outcome::Result(result) => {
                out.push(Event::ToolCallResult(ToolCallResultEvent {
                    base: base(),
                    message_id: MessageId::new(format!("{message}-result-{n}")),
                    tool_call_id: id,
                    content: result.to_owned(),
                    role: Role::Tool,
                }));
                if name == PAGE_SAVE_TOOL {
                    if let Some(value) = saved_page(result) {
                        out.push(Event::Custom(CustomEvent {
                            base: base(),
                            name: PAGE_SAVED.to_owned(),
                            value,
                        }));
                    }
                }
            }
            Outcome::Interrupt(reason) => out.push(Event::Interrupt(InterruptEvent {
                base: base(),
                interrupt_id: format!("{message}-interrupt-{n}"),
                tool_call_id: id,
                tool_call_name: name,
                reason,
                message: Some(interrupt_message(reason).to_owned()),
            })),
        }
    }
}

/// What a finished reply says about how it ended, for `RUN_FINISHED`'s `result`.
fn finish_result(m: &JValue) -> Option<Value> {
    let mut out = serde_json::Map::new();
    if m.get("stopped").as_bool() == Some(true) {
        out.insert("stopped".into(), json!(true));
    }
    let paused = m.get("paused");
    if paused.is_object() {
        let mut p = serde_json::Map::new();
        p.insert(
            "reason".into(),
            json!(paused.get("reason").as_str().unwrap_or("")),
        );
        let applied = paused.get("applied").as_num().unwrap_or(0.0);
        p.insert("applied".into(), json!(applied.max(0.0) as u64));
        if let JValue::Arr(names) = paused.get("declined") {
            let names: Vec<&str> = names.iter().filter_map(JValue::as_str).collect();
            p.insert("declined".into(), json!(names));
        }
        out.insert("paused".into(), Value::Object(p));
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

fn run_started(out: &mut Vec<Event>, thread: &str, id: &str) {
    out.push(Event::RunStarted(RunStartedEvent {
        base: base(),
        thread_id: ThreadId::new(thread),
        run_id: RunId::new(id),
    }));
}

fn assistant(out: &mut Vec<Event>, thread: &str, id: &str, m: &JValue) {
    if let Some(reasoning) = text_of(m.get("reasoning")) {
        thinking(out, reasoning);
    }
    if let JValue::Arr(calls) = m.get("toolCalls") {
        tool_calls(out, id, calls);
    }
    let failed = m.get("error").as_bool() == Some(true);
    let failure = text_of(m.get("failure"));
    let content = text_of(m.get("content"));
    // An older failed reply keeps its failure sentence in `content`, so that is not message text.
    if let Some(text) = content.filter(|_| !failed || failure.is_some()) {
        text_message(out, id, Role::Assistant, text);
    }
    if failed {
        out.push(Event::RunError(RunErrorEvent {
            base: base(),
            message: failure
                .or(content)
                .unwrap_or("The reply failed.")
                .to_owned(),
            code: Some("failed".to_owned()),
        }));
    } else if m.get("interrupted").as_bool() == Some(true) {
        out.push(Event::RunError(RunErrorEvent {
            base: base(),
            message: "The reply was cut off before it finished.".to_owned(),
            code: Some("interrupted".to_owned()),
        }));
    } else {
        out.push(Event::RunFinished(RunFinishedEvent {
            base: base(),
            thread_id: ThreadId::new(thread),
            run_id: RunId::new(id),
            result: finish_result(m),
        }));
    }
}

/// The events of `history` (the coworker's stored messages, oldest first), after the message
/// whose id is `after` when given (that message is not replayed). [`UnknownAfter`] when no message
/// has that id, so a cursor from a history that has since changed is never taken for "from the
/// start".
pub fn translate(
    thread_id: &str,
    history: &[JValue],
    after: Option<&str>,
) -> Result<Vec<Event>, UnknownAfter> {
    let from = match after {
        None => 0,
        Some(cursor) => {
            history
                .iter()
                .enumerate()
                .position(|(i, m)| message_id(m, i) == cursor)
                .ok_or(UnknownAfter)?
                + 1
        }
    };
    let mut out = Vec::new();
    // User turns wait here for the run of the reply that follows, so every run starts the stream
    // or follows a finished one, as the protocol requires.
    let mut pending: Vec<(String, &str)> = Vec::new();
    for (i, m) in history.iter().enumerate().skip(from) {
        let id = message_id(m, i);
        match m.get("role").as_str() {
            Some("user") => {
                if let Some(text) = text_of(m.get("content")) {
                    pending.push((id, text));
                }
            }
            Some("assistant") => {
                run_started(&mut out, thread_id, &id);
                for (uid, text) in pending.drain(..) {
                    text_message(&mut out, &uid, Role::User, text);
                }
                assistant(&mut out, thread_id, &id, m);
            }
            _ => {}
        }
    }
    if let Some((last, _)) = pending.last() {
        let last = last.clone();
        run_started(&mut out, thread_id, &last);
        for (uid, text) in pending.drain(..) {
            text_message(&mut out, &uid, Role::User, text);
        }
        out.push(Event::RunFinished(RunFinishedEvent {
            base: base(),
            thread_id: ThreadId::new(thread_id),
            run_id: RunId::new(last),
            result: Some(json!({"awaitingReply": true})),
        }));
    }
    Ok(out)
}

/// One event as a Server-Sent Events frame: `data: <json>` and a blank line.
pub fn frame(event: &Event) -> Option<String> {
    serde_json::to_string(event)
        .ok()
        .map(|json| format!("data: {json}\n\n"))
}

impl Store {
    /// The stored messages of coworker `id`'s conversation, at most the history cap (the newest
    /// when there are more). [`Error::NotFound`] for an unknown id, which includes another
    /// account's: the store is the signed-in account's own directory. A coworker without a backing
    /// conversation, or whose chat has no transcript yet, has no messages.
    pub fn conversation(&self, id: &str) -> Result<Vec<JValue>, Error> {
        let (_, specialist) = self.get_specialist(id)?;
        let (Some(backing), Some(chat)) = (self.backing(), specialist.chat_id.as_deref()) else {
            return Ok(Vec::new());
        };
        let current = server_projects::transcript::read(&backing.data_dir, &backing.user_id, chat)
            .map_err(|e| Error::Io(std::io::Error::other(format!("transcript: {e:?}"))))?;
        let JValue::Arr(mut messages) = current.history else {
            return Ok(Vec::new());
        };
        let cap = server_projects::transcript::HISTORY_CAP;
        if messages.len() > cap {
            messages.drain(..messages.len() - cap);
        }
        Ok(messages)
    }
}
