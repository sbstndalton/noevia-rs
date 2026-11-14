//! The Code sandbox's ACP bridge, native: a port of noevia-core `code-sandbox/pi-acp-bridge.cjs`.
//!
//! The bridge is an ACP agent on stdio (the supervisor spawns it like any harness). It runs one
//! RPC-mode harness process per session and translates:
//!
//! - ACP `initialize` / `session/new` / `session/prompt` / `session/cancel` -> harness `prompt` / `abort`;
//! - harness `message_update` text and thinking deltas -> `agent_message_chunk` / `agent_thought_chunk`;
//! - harness `tool_execution_start` / `_end` -> `tool_call` / `tool_call_update`;
//! - harness `extension_ui_request` `confirm` from noevia's gate -> `session/request_permission`.
//!
//! The bridge can only ever turn a question into "no" by itself, never into "yes": any other
//! dialog, a malformed payload, a closed client, a timed-out ask or an unknown answer is a refusal.
//!
//! This module is the protocol state machine, free of I/O: [`Bridge`] takes one framed line (or
//! one event) at a time, talks to the harness through [`Harness`], and queues what goes to the
//! client in [`Bridge::take_output`]. `main.rs` is the tokio shell around it.
//!
//! Nothing runs it yet. In the sandbox image (noevia-core `code-sandbox/Dockerfile`) it is built only
//! from its own source pin (`CODE_HARNESS_BRIDGE_RS_REF`, empty), installed at
//! `/usr/local/libexec/noevia/harness-bridge/code-harness-bridge`, and the selector starts it in place
//! of `node /srv/pi-acp-bridge.cjs` only with `CODE_HARNESS_BRIDGE_IMPL=rust` (default `js`).
//!
//! The JS reference's framing and `toolCallFor` are the shared `sandbox-bridge` crate, so this
//! bridge behaves as the JS one does with `SANDBOX_BRIDGE_IMPL=rust` (a failure of either is the
//! fixed [`RUST_FAILED`] session end).
//!
//! Behaviour kept from the reference on purpose:
//! - replies to ACP methods are sent after every line of the same input chunk was handled (the
//!   JS handlers are `async`, so their replies are promise reactions), while `Method not found` is
//!   sent at once; [`Bridge::end_chunk`] is that boundary;
//! - a refusal reason for the gate is kept per tool call id, at most 50, oldest first out, and is
//!   read once.
//!
//! Differences from the reference, each a reference bug (noevia#2762) or a choice named here:
//! - a client line or harness line that is JSON `null` is ignored (the reference throws on
//!   `null.id` / `null.type`, an uncaught exception that kills the bridge);
//! - a client method named after an `Object.prototype` member (`toString`, `constructor`,
//!   `__proto__`, ...) is `Method not found` (the reference calls the prototype function and dies
//!   on `.then` of its result);
//! - an ask timeout above 2^31-1 ms is clamped there (Node's `setTimeout` treats it as 1 ms, so a
//!   large `PI_BRIDGE_ASK_TIMEOUT_MS` refused every approval at once);
//! - a refusal reason is cut at 4000 UTF-16 code units without splitting a surrogate pair (the
//!   reference may send a lone surrogate).

use std::collections::{BTreeMap, VecDeque};

use serde_json::{json, Map, Value};

/// ACP protocol version this agent speaks.
pub const PROTOCOL_VERSION: u64 = 1;
/// The web gate's own wait for a human (`APPROVAL_TIMEOUT_MS`); our ask to the client has the same.
pub const DEFAULT_ASK_TIMEOUT_MS: u64 = 300_000;
/// Largest timer Node honours (`setTimeout` treats anything larger as 1 ms).
pub const MAX_TIMER_MS: u64 = 2_147_483_647;
/// Grace between SIGTERM and SIGKILL of the harness's process group.
pub const KILL_GRACE_MS: u64 = 5_000;
/// The fixed session end when the shared bridge rules fail.
pub const RUST_FAILED: &str = "the sandbox bridge (rust) failed; closing the session";
/// The session id of the one session a bridge serves.
pub const SESSION_ID: &str = "pi-1";
/// The harness's reviewed command-line surface, after `--extension <gate>` is spliced in.
const HARNESS_ARGS_HEAD: [&str; 7] = [
    "--mode",
    "rpc",
    "--offline",
    "--no-session",
    "--no-approve",
    "--no-context-files",
    "--no-extensions",
];
const HARNESS_ARGS_TAIL: [&str; 5] = [
    "--no-skills",
    "--no-prompt-templates",
    "--no-themes",
    "--tools",
    "read,bash,edit,write,grep,find,ls",
];
/// The gate extension noevia writes into the harness's agent dir, relative to the task HOME.
const GATE_EXTENSION: &str = ".pi/agent/extensions/noevia-gate.js";
const DIALOGS: [&str; 4] = ["confirm", "select", "input", "editor"];
const MAX_REFUSALS: usize = 50;
const MAX_REASON_UNITS: usize = 4000;

/// What the bridge needs from the harness process.
pub trait Harness {
    /// Start the harness in `cwd` (`None` = inherit) with `args`, in its own process group.
    /// `Err` carries the reference's error text (`spawn <command> ENOENT`).
    fn spawn(&mut self, cwd: Option<&str>, args: &[String]) -> Result<(), String>;
    /// Write one JSONL command to the harness's stdin (dropped when it is gone).
    fn write(&mut self, command: &Value);
    /// SIGTERM the harness's process group, SIGKILL after [`KILL_GRACE_MS`] if it is still there.
    fn kill(&mut self);
}

/// `piArgsFor(env)`: the reviewed harness arguments for a task whose private HOME is `home`.
pub fn harness_args(home: Option<&str>) -> Result<Vec<String>, String> {
    let home = home.unwrap_or("");
    if !home.starts_with('/') {
        return Err("The pi bridge needs the task private HOME.".to_owned());
    }
    let gate = posix_join(home, GATE_EXTENSION);
    let mut args: Vec<String> = HARNESS_ARGS_HEAD.iter().map(|s| (*s).to_owned()).collect();
    args.push("--extension".to_owned());
    args.push(gate);
    args.extend(HARNESS_ARGS_TAIL.iter().map(|s| (*s).to_owned()));
    Ok(args)
}

/// Node's `path.posix.join(a, b)` for an absolute `a`: join, then normalise `.`, `..` and repeated
/// slashes (a trailing slash is kept, as Node keeps it).
pub fn posix_join(a: &str, b: &str) -> String {
    let joined = format!("{a}/{b}");
    let trailing = joined.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = format!("/{}", parts.join("/"));
    if trailing && out != "/" {
        out.push('/');
    }
    out
}

/// `Number(env.PI_BRIDGE_ASK_TIMEOUT_MS) > 0 ? Number(...) : DEFAULT_ASK_TIMEOUT_MS`, with Node's
/// timer range applied (fractions truncated, at most [`MAX_TIMER_MS`]).
pub fn ask_timeout_ms(raw: Option<&str>) -> u64 {
    let Some(n) = raw.and_then(js_number) else {
        return DEFAULT_ASK_TIMEOUT_MS;
    };
    if n.is_nan() || n <= 0.0 {
        return DEFAULT_ASK_TIMEOUT_MS;
    }
    if n >= MAX_TIMER_MS as f64 {
        return MAX_TIMER_MS;
    }
    // setTimeout(0.5) fires after 1 ms (Node: delay < 1 becomes 1).
    (n.trunc() as u64).max(1)
}

/// JS `Number(string)` for the forms an environment value takes; `None` = NaN.
pub fn js_number(raw: &str) -> Option<f64> {
    let s = raw.trim_matches(sandbox_bridge::frame::js_space);
    if s.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = s.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            let mut value = 0f64;
            for c in digits.chars() {
                value = value * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0));
            }
            return Some(value);
        }
    }
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned == "Infinity" {
        return Some(if s.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    // StrDecimalLiteral: digits, one dot, an exponent; nothing Rust's parser also takes (inf, nan).
    let ok = !unsigned.is_empty()
        && unsigned
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && unsigned.chars().any(|c| c.is_ascii_digit());
    if !ok {
        return None;
    }
    s.parse::<f64>().ok()
}

/// JS truthiness of a parsed JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(v)` of a parsed JSON value (`Err` where JS throws a TypeError: an object whose own
/// `toString` is not callable, as every JSON one is).
fn js_string(v: &Value) -> Result<String, ()> {
    if let Value::String(s) = v {
        return Ok(s.clone());
    }
    let text = serde_json::to_string(v).map_err(|_| ())?;
    let parsed = sandbox_bridge::json::parse(&text).ok_or(())?;
    let units = sandbox_bridge::js::to_string16(&parsed).map_err(|_| ())?;
    Ok(String::from_utf16_lossy(&units))
}

/// JS `String(v ?? '')`.
fn js_string_or_empty(v: Option<&Value>) -> Result<String, ()> {
    match v {
        None | Some(Value::Null) => Ok(String::new()),
        Some(v) => js_string(v),
    }
}

/// JS `String.prototype.trim()` is empty.
fn blank(s: &str) -> bool {
    s.trim_matches(sandbox_bridge::frame::js_space).is_empty()
}

/// `text.slice(0, 4000)` in UTF-16 code units, never splitting a surrogate pair.
fn cut_reason(text: &str) -> String {
    let mut units = 0usize;
    let mut out = String::new();
    for c in text.chars() {
        units += c.len_utf16();
        if units > MAX_REASON_UNITS {
            break;
        }
        out.push(c);
    }
    out
}

/// `refusalReason(outcome, error)`: noevia's own words for a refusal, bounded; empty when none.
pub fn refusal_reason(outcome: Option<&Value>, error: Option<&Value>) -> String {
    let said = match outcome {
        Some(o @ (Value::Object(_) | Value::Array(_))) => {
            o.get("_meta").and_then(|m| m.get("noevia"))
        }
        _ => None,
    };
    let reason = said.and_then(|s| s.get("reason")).and_then(Value::as_str);
    if let Some(r) = reason.filter(|r| !blank(r)) {
        return cut_reason(r);
    }
    let message = error.and_then(|e| e.get("message")).and_then(Value::as_str);
    match message.filter(|m| !blank(m)) {
        Some(m) => cut_reason(m),
        None => String::new(),
    }
}

/// `toolCallOf(payload)` through the shared rules; `None` = the bridge rules failed.
fn tool_call(payload: &Value) -> Option<Map<String, Value>> {
    let text = serde_json::to_string(payload).ok()?;
    let out = sandbox_bridge::tool_call::tool_call_json(&text).ok()?;
    match serde_json::from_str::<Value>(&out).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// How a `session/prompt` call stands.
#[derive(Debug, Clone, PartialEq)]
enum Ack {
    Waiting,
    Accepted,
    Refused,
}

#[derive(Debug, Clone)]
struct PromptCall {
    /// The client's request id (absent for a notification-style call).
    reply_id: Option<Value>,
    ack: Ack,
    /// The turn's stop reason, once the turn is over.
    finished: Option<&'static str>,
    replied: bool,
}

#[derive(Debug, Clone)]
struct Turn {
    call: usize,
    cancelled: bool,
}

/// One question to the client still waiting for its answer.
#[derive(Debug, Clone)]
struct Ask {
    ui_id: Option<Value>,
    tool_call_id: String,
}

/// Work the JS reference does in promise reactions: run at the end of the input chunk.
#[derive(Debug, Clone)]
enum Deferred {
    Reply(Option<Value>, Result<Value, (i64, String)>),
    Answer {
        ask: Ask,
        result: Option<Value>,
        error: Option<Value>,
    },
    Prompt(usize),
}

/// The bridge's state for one stdio connection.
#[derive(Debug)]
pub struct Bridge {
    home: Option<String>,
    ask_timeout_ms: u64,
    next_ask: u64,
    waiting: BTreeMap<String, Ask>,
    session: bool,
    harness_running: bool,
    turn: Option<Turn>,
    calls: Vec<PromptCall>,
    harness_seq: u64,
    harness_pending: BTreeMap<String, usize>,
    closed: bool,
    closing: bool,
    close_output: bool,
    refusals: Vec<(String, String)>,
    deferred: VecDeque<Deferred>,
    output: Vec<Value>,
    logs: Vec<String>,
    timers: Vec<(String, u64)>,
}

impl Bridge {
    /// A bridge for a task whose HOME is `home`, asking the client with `ask_timeout_ms`.
    pub fn new(home: Option<String>, ask_timeout_ms: u64) -> Self {
        Self {
            home,
            ask_timeout_ms,
            next_ask: 1,
            waiting: BTreeMap::new(),
            session: false,
            harness_running: false,
            turn: None,
            calls: Vec::new(),
            harness_seq: 0,
            harness_pending: BTreeMap::new(),
            closed: false,
            closing: false,
            close_output: false,
            refusals: Vec::new(),
            deferred: VecDeque::new(),
            output: Vec::new(),
            logs: Vec::new(),
            timers: Vec::new(),
        }
    }

    /// Messages for the client, in order (each without its trailing newline).
    pub fn take_output(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.output)
    }

    /// Lines for the bridge's stderr log.
    pub fn take_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.logs)
    }

    /// Asks that need a timer: `(ask id, milliseconds)`; call [`Bridge::ask_timed_out`] when one fires.
    pub fn take_timers(&mut self) -> Vec<(String, u64)> {
        std::mem::take(&mut self.timers)
    }

    /// The session is over: stop reading the client and end the output once it is written.
    pub fn output_closed(&self) -> bool {
        self.close_output
    }

    /// The session ended (nothing more is read from either side).
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// A harness process is running (not yet reported gone).
    pub fn harness_running(&self) -> bool {
        self.harness_running
    }

    fn send(&mut self, mut message: Map<String, Value>) {
        if self.close_output {
            return;
        }
        message.insert("jsonrpc".to_owned(), Value::from("2.0"));
        self.output.push(Value::Object(message));
    }

    fn reply(&mut self, id: Option<Value>, outcome: Result<Value, (i64, String)>) {
        let mut m = Map::new();
        if let Some(id) = id {
            m.insert("id".to_owned(), id);
        }
        match outcome {
            Ok(result) => {
                m.insert("result".to_owned(), result);
            }
            Err((code, message)) => {
                m.insert(
                    "error".to_owned(),
                    json!({ "code": code, "message": message }),
                );
            }
        }
        self.send(m);
    }

    fn notify(&mut self, update: Value) {
        let mut m = Map::new();
        m.insert("method".to_owned(), Value::from("session/update"));
        let session = if self.session {
            Value::from(SESSION_ID)
        } else {
            Value::Null
        };
        m.insert(
            "params".to_owned(),
            json!({ "sessionId": session, "update": update }),
        );
        self.send(m);
    }

    fn to_harness(&mut self, h: &mut dyn Harness, command: Value) {
        if self.harness_running {
            h.write(&command);
        }
    }

    /// One complete line from the client (already framed and valid JSON).
    pub fn client_line(&mut self, h: &mut dyn Harness, line: &str) {
        if self.closed {
            return;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Value::Object(message) = message else {
            return; // a JSON null, number, string or array has no id or method
        };
        let id = message.get("id").cloned();
        let method = message.get("method");
        if id.is_some() && method.is_none() {
            let Some(Value::String(key)) = &id else {
                return;
            };
            // Answered now: a second reply under the same id, or one after the timeout, is dropped.
            if let Some(ask) = self.waiting.remove(key) {
                self.deferred.push_back(Deferred::Answer {
                    ask,
                    result: message.get("result").cloned(),
                    error: message.get("error").cloned(),
                });
            }
            return;
        }
        let name = match method {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match name {
            "session/cancel" => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.cancelled = true;
                }
                self.to_harness(h, json!({ "type": "abort" }));
            }
            "initialize" => {
                let result = json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "agentCapabilities": { "loadSession": false },
                    "agentInfo": { "name": "pi (noevia bridge)", "version": "1" },
                    "authMethods": [],
                });
                self.deferred.push_back(Deferred::Reply(id, Ok(result)));
            }
            "session/new" => {
                let outcome = self.session_new(h, &params);
                self.deferred.push_back(Deferred::Reply(id, outcome));
            }
            "session/prompt" => self.session_prompt(h, id, &params),
            _ => {
                if let Some(id) = id {
                    self.reply(Some(id), Err((-32601, "Method not found".to_owned())));
                }
            }
        }
    }

    fn session_new(&mut self, h: &mut dyn Harness, params: &Value) -> Result<Value, (i64, String)> {
        if self.session {
            return Err((-32602, "One session per bridge".to_owned()));
        }
        let cwd = params.get("cwd").and_then(Value::as_str).map(str::to_owned);
        let args = harness_args(self.home.as_deref()).map_err(|e| (-32603, e))?;
        let spawned = h.spawn(cwd.as_deref(), &args);
        self.session = true;
        match spawned {
            Ok(()) => self.harness_running = true,
            // The reference learns of a failed spawn on the next tick, before the reply goes out.
            Err(why) => self.end_session(h, &format!("pi failed: {why}"), false),
        }
        Ok(json!({ "sessionId": SESSION_ID }))
    }

    fn session_prompt(&mut self, h: &mut dyn Harness, id: Option<Value>, params: &Value) {
        let same =
            params.get("sessionId").and_then(Value::as_str) == Some(SESSION_ID) && self.session;
        if !self.harness_running || !same {
            self.deferred.push_back(Deferred::Reply(
                id,
                Err((-32602, "No such session".to_owned())),
            ));
            return;
        }
        if self.turn.is_some() {
            self.deferred.push_back(Deferred::Reply(
                id,
                Err((-32602, "A prompt is already running".to_owned())),
            ));
            return;
        }
        let mut texts = Vec::new();
        if let Some(Value::Array(blocks)) = params.get("prompt") {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                // Array.prototype.join: undefined and null are '', the rest String().
                match js_string_or_empty(block.get("text")) {
                    Ok(t) => texts.push(t),
                    Err(()) => {
                        let e = (
                            -32603,
                            "Cannot convert object to primitive value".to_owned(),
                        );
                        self.deferred.push_back(Deferred::Reply(id, Err(e)));
                        return;
                    }
                }
            }
        }
        let call = self.calls.len();
        self.calls.push(PromptCall {
            reply_id: id,
            ack: Ack::Waiting,
            finished: None,
            replied: false,
        });
        self.turn = Some(Turn {
            call,
            cancelled: false,
        });
        self.harness_seq += 1;
        let key = format!("p{}", self.harness_seq);
        self.harness_pending.insert(key.clone(), call);
        let text = texts.join("\n");
        self.to_harness(h, json!({ "id": key, "type": "prompt", "message": text }));
    }

    /// One complete line from the harness.
    pub fn harness_line(&mut self, h: &mut dyn Harness, line: &str) {
        if self.closed {
            return;
        }
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "response" {
            if let Some(Value::String(key)) = event.get("id") {
                if let Some(call) = self.harness_pending.remove(key) {
                    let refused = event.get("success") == Some(&Value::Bool(false));
                    self.ack(call, !refused);
                    return;
                }
            }
        }
        match kind {
            "extension_ui_request" => self.ui_request(h, &event),
            "message_update" => {
                let inner = event.get("assistantMessageEvent");
                let which = inner.and_then(|e| e.get("type")).and_then(Value::as_str);
                let update = match which {
                    Some("text_delta") => "agent_message_chunk",
                    Some("thinking_delta") => "agent_thought_chunk",
                    _ => return,
                };
                let Ok(text) = js_string_or_empty(inner.and_then(|e| e.get("delta"))) else {
                    return self.end_session(h, RUST_FAILED, true);
                };
                self.notify(
                    json!({ "sessionUpdate": update, "content": { "type": "text", "text": text } }),
                );
            }
            "tool_execution_start" => {
                let mut payload = Map::new();
                for (from, to) in [
                    ("toolCallId", "toolCallId"),
                    ("toolName", "toolName"),
                    ("args", "input"),
                ] {
                    if let Some(v) = event.get(from) {
                        payload.insert(to.to_owned(), v.clone());
                    }
                }
                let Some(call) = tool_call(&Value::Object(payload)) else {
                    return self.end_session(h, RUST_FAILED, true);
                };
                let mut update = Map::new();
                update.insert("sessionUpdate".to_owned(), Value::from("tool_call"));
                update.extend(call);
                update.insert("status".to_owned(), Value::from("in_progress"));
                self.notify(Value::Object(update));
            }
            "tool_execution_end" => {
                let id = match event.get("toolCallId") {
                    Some(v) if truthy(v) => js_string(v),
                    _ => Ok(String::new()),
                };
                let Ok(id) = id else {
                    return self.end_session(h, RUST_FAILED, true);
                };
                let failed = event.get("isError").is_some_and(truthy);
                let status = if failed { "failed" } else { "completed" };
                self.notify(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status }));
            }
            "agent_settled" => {
                if let Some(turn) = self.turn.take() {
                    let reason = if turn.cancelled {
                        "cancelled"
                    } else {
                        "end_turn"
                    };
                    self.finish(turn.call, reason);
                }
            }
            _ => {}
        }
    }

    fn ack(&mut self, call: usize, accepted: bool) {
        let Some(c) = self.calls.get_mut(call) else {
            return;
        };
        c.ack = if accepted {
            Ack::Accepted
        } else {
            Ack::Refused
        };
        if !accepted && self.turn.as_ref().is_some_and(|t| t.call == call) {
            self.turn = None;
        }
        self.deferred.push_back(Deferred::Prompt(call));
    }

    fn finish(&mut self, call: usize, reason: &'static str) {
        if let Some(c) = self.calls.get_mut(call) {
            if c.finished.is_none() {
                c.finished = Some(reason);
            }
        }
        self.deferred.push_back(Deferred::Prompt(call));
    }

    /// `{ type: 'extension_ui_response', id, [field]: value }`; an absent `id` stays absent.
    fn ui_answer(&mut self, h: &mut dyn Harness, id: Option<&Value>, field: &str, value: Value) {
        let mut m = Map::new();
        m.insert("type".to_owned(), Value::from("extension_ui_response"));
        if let Some(id) = id {
            m.insert("id".to_owned(), id.clone());
        }
        m.insert(field.to_owned(), value);
        self.to_harness(h, Value::Object(m));
    }

    fn refuse_ui(&mut self, h: &mut dyn Harness, id: Option<&Value>, method: &str) {
        if method == "confirm" {
            self.ui_answer(h, id, "confirmed", Value::Bool(false));
        } else {
            self.ui_answer(h, id, "cancelled", Value::Bool(true));
        }
    }

    fn ui_request(&mut self, h: &mut dyn Harness, request: &Map<String, Value>) {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        if !DIALOGS.contains(&method) {
            return; // notify, setStatus, ...: fire and forget
        }
        let id = request.get("id").cloned();
        if method == "input" {
            return self.refusal_reason_request(h, id.as_ref(), request);
        }
        if method != "confirm" {
            return self.refuse_ui(h, id.as_ref(), method);
        }
        let Ok(text) = js_string_or_empty(request.get("message")) else {
            return self.end_session(h, RUST_FAILED, true);
        };
        let payload = match serde_json::from_str::<Value>(&text) {
            Ok(v) if sandbox_bridge::json::validate(&text) => v,
            _ => return self.refuse_ui(h, id.as_ref(), method),
        };
        let wanted = payload.get("noevia").and_then(Value::as_str) == Some("tool_call")
            && payload.get("toolName").is_some_and(truthy);
        if !wanted {
            return self.refuse_ui(h, id.as_ref(), method);
        }
        let Some(call) = tool_call(&payload) else {
            return self.end_session(h, RUST_FAILED, true);
        };
        let tool_call_id = call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let key = format!("b{}", self.next_ask);
        self.next_ask += 1;
        self.waiting.insert(
            key.clone(),
            Ask {
                ui_id: id,
                tool_call_id,
            },
        );
        self.timers.push((key.clone(), self.ask_timeout_ms));
        let mut m = Map::new();
        m.insert("id".to_owned(), Value::from(key));
        m.insert(
            "method".to_owned(),
            Value::from("session/request_permission"),
        );
        let session = if self.session {
            Value::from(SESSION_ID)
        } else {
            Value::Null
        };
        m.insert(
            "params".to_owned(),
            json!({
                "sessionId": session,
                "toolCall": Value::Object(call),
                "options": [
                    { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "reject_once", "name": "Decline", "kind": "reject_once" },
                ],
            }),
        );
        self.send(m);
    }

    fn refusal_reason_request(
        &mut self,
        h: &mut dyn Harness,
        id: Option<&Value>,
        request: &Map<String, Value>,
    ) {
        let Ok(text) = js_string_or_empty(request.get("placeholder")) else {
            return self.end_session(h, RUST_FAILED, true);
        };
        let asked = match serde_json::from_str::<Value>(&text) {
            Ok(v) if sandbox_bridge::json::validate(&text) => v,
            _ => return self.refuse_ui(h, id, "input"),
        };
        if asked.get("noevia").and_then(Value::as_str) != Some("refusal_reason") {
            return self.refuse_ui(h, id, "input");
        }
        let mut probe = Map::new();
        for k in ["toolCallId", "toolName"] {
            if let Some(v) = asked.get(k) {
                probe.insert(k.to_owned(), v.clone());
            }
        }
        let Some(call) = tool_call(&Value::Object(probe)) else {
            return self.end_session(h, RUST_FAILED, true);
        };
        let key = call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let found = self.refusals.iter().position(|(k, _)| *k == key);
        let reason = found
            .map(|i| self.refusals.remove(i).1)
            .filter(|r| !r.is_empty());
        match reason {
            Some(value) => self.ui_answer(h, id, "value", Value::from(value)),
            None => self.refuse_ui(h, id, "input"),
        }
    }

    /// The timer of ask `id` fired: the question is refused (a later reply is dropped).
    pub fn ask_timed_out(&mut self, h: &mut dyn Harness, id: &str) {
        if let Some(ask) = self.waiting.remove(id) {
            self.answer(h, ask, None, None);
        }
    }

    fn answer(
        &mut self,
        h: &mut dyn Harness,
        ask: Ask,
        result: Option<Value>,
        error: Option<Value>,
    ) {
        let outcome = result.as_ref().and_then(|r| r.get("outcome"));
        let confirmed = outcome
            .and_then(|o| o.get("outcome"))
            .and_then(Value::as_str)
            == Some("selected")
            && outcome
                .and_then(|o| o.get("optionId"))
                .and_then(Value::as_str)
                == Some("allow_once");
        if !confirmed {
            let reason = refusal_reason(outcome, error.as_ref());
            if !reason.is_empty() {
                if self.refusals.len() >= MAX_REFUSALS {
                    self.refusals.remove(0);
                }
                match self
                    .refusals
                    .iter()
                    .position(|(k, _)| *k == ask.tool_call_id)
                {
                    Some(i) => {
                        if let Some(entry) = self.refusals.get_mut(i) {
                            entry.1 = reason;
                        }
                    }
                    None => self.refusals.push((ask.tool_call_id.clone(), reason)),
                }
            }
        }
        self.ui_answer(h, ask.ui_id.as_ref(), "confirmed", Value::Bool(confirmed));
    }

    /// The end of one input chunk: run the replies and continuations the reference runs as
    /// promise reactions, in order.
    pub fn end_chunk(&mut self, h: &mut dyn Harness) {
        while let Some(work) = self.deferred.pop_front() {
            match work {
                Deferred::Reply(id, outcome) => self.reply(id, outcome),
                Deferred::Answer { ask, result, error } => {
                    // A JSON-RPC error means no result (`message.error ? null : message.result`).
                    let result = if error.as_ref().is_some_and(truthy) {
                        None
                    } else {
                        result
                    };
                    let error = error.filter(truthy);
                    self.answer(h, ask, result, error);
                }
                Deferred::Prompt(call) => self.settle_prompt(call),
            }
        }
        if self.closing {
            self.close_output = true;
        }
    }

    fn settle_prompt(&mut self, call: usize) {
        let Some(c) = self.calls.get(call) else {
            return;
        };
        if c.replied {
            return;
        }
        let stop = match (&c.ack, c.finished) {
            (Ack::Refused, _) => "refusal",
            (Ack::Accepted, Some(reason)) => reason,
            _ => return,
        };
        let id = c.reply_id.clone();
        if let Some(c) = self.calls.get_mut(call) {
            c.replied = true;
        }
        self.reply(id, Ok(json!({ "stopReason": stop })));
    }

    /// Refuse what is still waiting: the running turn, every unacknowledged harness command and
    /// every question to the client.
    fn refuse_all_pending(&mut self, _h: &mut dyn Harness) {
        let pending = std::mem::take(&mut self.harness_pending);
        for call in pending.into_values() {
            if let Some(c) = self.calls.get_mut(call) {
                c.ack = Ack::Refused;
            }
            self.deferred.push_back(Deferred::Prompt(call));
        }
        if let Some(turn) = self.turn.take() {
            self.finish(turn.call, "refusal");
        }
        self.refuse_questions();
    }

    /// Every question to the client still waiting is answered "no" (in the reaction order).
    fn refuse_questions(&mut self) {
        let waiting = std::mem::take(&mut self.waiting);
        for (_, ask) in waiting {
            self.deferred.push_back(Deferred::Answer {
                ask,
                result: None,
                error: None,
            });
        }
    }

    fn end_session(&mut self, h: &mut dyn Harness, reason: &str, error: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.logs.push(reason.to_owned());
        if error {
            let mut m = Map::new();
            m.insert("id".to_owned(), Value::Null);
            m.insert(
                "error".to_owned(),
                json!({ "code": -32000, "message": reason }),
            );
            self.send(m);
        }
        self.refuse_all_pending(h);
        if self.harness_running {
            h.kill();
        }
        // The output closes at the end of this chunk, after the replies just queued are written
        // (the reference's setImmediate runs after its promise reactions).
        self.closing = true;
    }

    /// The client's line exceeded the line buffer.
    pub fn client_overflow(&mut self, h: &mut dyn Harness, size: usize) {
        let reason = format!(
            "the client's message exceeded the {size}-byte line buffer; closing the session"
        );
        self.end_session(h, &reason, true);
    }

    /// The harness's line exceeded the line buffer.
    pub fn harness_overflow(&mut self, h: &mut dyn Harness, size: usize) {
        let reason =
            format!("pi's output exceeded the {size}-byte line buffer; closing the session");
        self.end_session(h, &reason, true);
    }

    /// The client closed its stream: refuse its open questions and stop the harness.
    pub fn client_end(&mut self, h: &mut dyn Harness) {
        self.refuse_questions();
        if self.harness_running {
            h.kill();
        }
    }

    /// The harness is gone (`why` = `exited (SIGTERM)`, `exited (0)`, `failed: ...`).
    pub fn harness_gone(&mut self, h: &mut dyn Harness, why: &str) {
        self.harness_running = false;
        self.end_session(h, &format!("pi {why}"), false);
    }
}
