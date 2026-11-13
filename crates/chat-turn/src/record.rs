//! The turn record: the transcript entry a reply leaves when its client goes away, and the partial
//! file a reply keeps while it streams (noevia-core `server/chat-interrupted-turn.cjs`
//! `createTurnRecord`, and `server/chat-partial.cjs` `bounded()` and `finish({ keep })`).
//!
//! [`TurnRecord::observe`] follows the events the turn sends, as the client does;
//! [`TurnRecord::assistant_entry`] is the reply the client would have saved (unfinished tool calls
//! stopped, applied writes kept with a "changes saved" note, the language-neutral `Stopped`
//! placeholder when nothing is left); [`TurnRecord::partial_entry`] is the same reply marked
//! `interrupted`, keeping the thinking when no answer was written yet. [`bounded`] keeps the
//! newest [`PARTIAL_CAP_UNITS`] of `content` and `reasoning` and stamps `savedAt`; [`partial_file`]
//! is what a finished reply the transcript did not take leaves in `chat-partials/<sha>.json`.
//!
//! The behaviour is Node's, proven by `tests/fixtures/node-turn-record.v1.json` (recorded from the
//! Node functions by `tests/record_entries.cjs`) and by the whole turns of `node-turns.v1.json`.
//! Every length, cut and index Node takes in UTF-16 code units is taken in UTF-16 code units here;
//! keys are present exactly when `JSON.stringify` would write them.
//!
//! Named differences from Node (each tested):
//! - **Half pairs are dropped.** Where a JS cut (the 4000-unit tool result, the newest 64 Ki units
//!   of a partial field) falls inside a surrogate pair, JS keeps a lone surrogate; a Rust `String`
//!   cannot hold one, so the cut keeps whole characters only and is one unit shorter. The
//!   recording marks those cases (`loneDropped`) and stores Node's value without the lone unit.
//! - **A meta route that `String()` cannot convert** (an object with its own `toString` key):
//!   Node's `observe` throws out of the stream writer; here the event is ignored, which leaves the
//!   record exactly as Node's throw does (the throw comes before any field is set).
//! - **Sparse tool indexes cost nothing.** Node's tool list is a JS array: an `index` near 2^32
//!   makes its `filter`/`findIndex` walk billions of holes (the recording keeps indexes small for
//!   that reason). Here the slots are a map; indexes past the last array index (2^32 - 2), which
//!   JS stores as plain properties no later step reads, are ignored as they are in effect in Node.
//! - **Key order.** Entries are built in Node's key order, but `serde_json::Value` (without its
//!   `preserve_order` feature, which this workspace does not enable) holds keys sorted, so a file
//!   written from these values orders keys differently. Every reader parses the JSON.
//! - **No `Infinity`.** `count()` keeps finite numbers only; `serde_json` cannot hold a non-finite
//!   number, so there is no input for Node's `1e400` (JSON.parse: `Infinity`, dropped by count).

use std::collections::BTreeMap;

use js_json::{js_len, js_slice, number_to_string};
use serde_json::{Map, Value};

/// The model label of a reply that kept nothing (src/chat-labels.ts `STOPPED_SENDER`, #634).
pub const STOPPED_SENDER: &str = "Stopped";
/// UTF-16 units of a tool result the record keeps (src/components/ToolCalls.tsx).
pub const TOOL_RESULT_LIMIT: usize = 4000;
/// UTF-16 units of `content` and `reasoning` a partial keeps, the newest (chat-partial.cjs
/// `PARTIAL_CAP_CHARS`).
pub const PARTIAL_CAP_UNITS: usize = 64 * 1024;
/// The last index a JS array element can have; a larger index is a plain property.
const MAX_ARRAY_INDEX: f64 = 4_294_967_294.0;
/// How many declined tool names a declined pause keeps.
const DECLINED_CAP: usize = 8;
const COUNTS: [&str; 4] = [
    "promptTokens",
    "completionTokens",
    "totalTokens",
    "tokensPerSecond",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Pending,
    Done,
    Denied,
    Stopped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Pending => "pending",
            Status::Done => "done",
            Status::Denied => "denied",
            Status::Stopped => "stopped",
        }
    }
}

/// One tool call as the client shows it (one slot of Node's `tools` array).
#[derive(Debug, Clone)]
struct Slot {
    name: String,
    args: String,
    /// Only a `tool_result` sets it (and then always, possibly empty).
    result: Option<String>,
    status: Status,
    target: Option<String>,
    target_kind: Option<String>,
    applied: bool,
}

impl Slot {
    /// `{ name: str(ev.name) || 'tool', args: str(ev.args), status }`
    fn announced(ev: &Map<String, Value>, status: Status) -> Slot {
        Slot {
            name: name_or_tool(text_of(ev, "name")),
            args: text_of(ev, "args").to_owned(),
            result: None,
            status,
            target: None,
            target_kind: None,
            applied: false,
        }
    }

    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), self.name.clone().into());
        m.insert("args".into(), self.args.clone().into());
        if let Some(result) = &self.result {
            m.insert("result".into(), result.clone().into());
        }
        m.insert("status".into(), self.status.as_str().into());
        if let Some(target) = &self.target {
            m.insert("target".into(), target.clone().into());
        }
        if let Some(kind) = &self.target_kind {
            m.insert("targetKind".into(), kind.clone().into());
        }
        if self.applied {
            m.insert("applied".into(), true.into());
        }
        Value::Object(m)
    }
}

#[derive(Debug, Clone)]
enum Paused {
    Declined {
        applied: Value,
        declined: Vec<String>,
    },
    Sensitive(Value),
    Supervision(Value),
}

impl Paused {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        let (reason, applied) = match self {
            Paused::Declined { applied, .. } => ("declined", applied),
            Paused::Sensitive(applied) => ("sensitive", applied),
            Paused::Supervision(applied) => ("supervision", applied),
        };
        m.insert("reason".into(), reason.into());
        m.insert("applied".into(), applied.clone());
        if let Paused::Declined { declined, .. } = self {
            m.insert("declined".into(), declined.clone().into());
        }
        Value::Object(m)
    }
}

/// Follows one reply's stream events and builds the transcript entry the client would have saved
/// (`createTurnRecord`).
pub struct TurnRecord {
    clock: Box<dyn FnMut() -> i64 + Send>,
    started_at: i64,
    content: String,
    reasoning: String,
    narration: String,
    model: Option<String>,
    /// `Some` once a routed meta event set it; `Some(Value::Null)` when that event had it `null`.
    routing_decision: Option<Value>,
    route_target: Option<Value>,
    sources: Option<Vec<Value>>,
    stats: Option<Value>,
    paused: Option<Paused>,
    failed: bool,
    /// The JS array's elements by index (holes are absent keys) and its `length`.
    tools: BTreeMap<u32, Slot>,
    len: u64,
}

impl TurnRecord {
    /// A record whose clock is `clock` (Node's injected `now`): read once now, as the start, and
    /// again at every `usage` event for `stats.elapsedMs`.
    pub fn new(mut clock: impl FnMut() -> i64 + Send + 'static) -> Self {
        let started_at = clock();
        TurnRecord {
            clock: Box::new(clock),
            started_at,
            content: String::new(),
            reasoning: String::new(),
            narration: String::new(),
            model: None,
            routing_decision: None,
            route_target: None,
            sources: None,
            stats: None,
            paused: None,
            failed: false,
            tools: BTreeMap::new(),
            len: 0,
        }
    }

    /// A record whose clock stands still at `now_ms` (every `elapsedMs` is then 0).
    pub fn at(now_ms: i64) -> Self {
        Self::new(move || now_ms)
    }

    /// Takes one event the turn sent. Anything that is not an object with a known string `type`
    /// is ignored.
    pub fn observe(&mut self, ev: &Value) {
        let Some(ev) = ev.as_object() else { return };
        let Some(kind) = ev.get("type").and_then(Value::as_str) else {
            return;
        };
        match kind {
            "meta" => self.meta(ev),
            "sources" => {
                if let Some(Value::Array(list)) = ev.get("sources") {
                    self.sources = Some(list.clone());
                }
            }
            "reasoning" => self.reasoning.push_str(text_of(ev, "text")),
            "preamble" => self.preamble(text_of(ev, "text")),
            "delta" => self.content.push_str(text_of(ev, "text")),
            "tool" => {
                let at = index_of(ev).unwrap_or(self.len as f64);
                self.put(at, Slot::announced(ev, Status::Running));
            }
            "tool_pending" => {
                let at = index_of(ev).unwrap_or(self.len.saturating_sub(1) as f64);
                let target = text_of(ev, "target");
                let kind = ev.get("targetKind").and_then(Value::as_str);
                let slot = Slot {
                    target: (!target.is_empty()).then(|| target.to_owned()),
                    target_kind: kind
                        .filter(|k| *k == "drive" || *k == "drive-new")
                        .map(str::to_owned),
                    ..Slot::announced(ev, Status::Pending)
                };
                self.put(at, slot);
            }
            "tool_result" => self.tool_result(ev),
            "paused" => self.paused = Some(paused_of(ev)),
            "usage" => {
                let now = (self.clock)();
                let mut stats = Map::new();
                for key in COUNTS {
                    // count(): finite numbers only; serde_json holds no other kind.
                    if let Some(n @ Value::Number(_)) = ev.get(key) {
                        stats.insert(key.into(), n.clone());
                    }
                }
                let elapsed = now.saturating_sub(self.started_at).max(0);
                stats.insert("elapsedMs".into(), elapsed.into());
                self.stats = Some(Value::Object(stats));
            }
            "error" => self.failed = true,
            _ => {}
        }
    }

    fn meta(&mut self, ev: &Map<String, Value>) {
        if let Some(route) = ev.get("route").filter(|v| truthy(v)) {
            // Node's template literal throws here (an own `toString`); nothing is set then.
            let Some(route) = js_string(route, 0) else {
                return;
            };
            self.model = Some(format!("Assistant · Auto ({route})"));
            self.routing_decision = ev.get("routingDecision").cloned();
        }
        let Some(routing) = ev.get("routing").and_then(Value::as_object) else {
            return;
        };
        let route = routing.get("route").and_then(Value::as_str);
        let Some(route) = route.filter(|r| *r == "local" || *r == "cloud") else {
            return;
        };
        let Some(reason) = routing.get("reason").and_then(Value::as_str) else {
            return;
        };
        let mut target = Map::new();
        target.insert("route".into(), route.into());
        target.insert("reason".into(), reason.into());
        if reason == "check-unavailable" {
            if let Some(detail) = routing
                .get("detail")
                .and_then(Value::as_str)
                .filter(|d| detail_code(d))
            {
                target.insert("detail".into(), detail.into());
            }
        }
        self.route_target = Some(Value::Object(target));
    }

    /// Narration before a tool call belongs with the thinking, not the answer.
    fn preamble(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // lastIndexOf on well-formed strings: a UTF-16 match of a whole string never starts or
        // ends inside a pair, so the last byte match is the same occurrence.
        if let Some(at) = self.content.rfind(text) {
            self.content.replace_range(at..at + text.len(), "");
        }
        let trimmed = js_trim(text);
        for field in [&mut self.reasoning, &mut self.narration] {
            if !field.is_empty() {
                field.push_str("\n\n");
            }
            field.push_str(trimmed);
        }
    }

    fn tool_result(&mut self, ev: &Map<String, Value>) {
        let at = match index_of(ev) {
            Some(at) => at,
            None => {
                // findIndex((t) => t && t.name === ev.name): only a string name can match.
                let wanted = ev.get("name").and_then(Value::as_str);
                let found = wanted.and_then(|w| {
                    self.tools
                        .iter()
                        .find(|(_, slot)| slot.name == w)
                        .map(|(i, _)| *i)
                });
                found.map_or(self.len as f64, f64::from)
            }
        };
        let previous = element(at).and_then(|i| self.tools.get(&i));
        let target = text_of(ev, "target");
        let target = if target.is_empty() {
            previous.and_then(|p| p.target.clone())
        } else {
            Some(target.to_owned())
        };
        let name = text_of(ev, "name");
        let name = if !name.is_empty() {
            name.to_owned()
        } else {
            previous.map_or_else(|| "tool".to_owned(), |p| name_or_tool(&p.name))
        };
        let status = if ev.get("declined") == Some(&Value::Bool(true)) {
            Status::Denied
        } else if ev.get("notRun") == Some(&Value::Bool(true)) {
            Status::Stopped
        } else {
            Status::Done
        };
        let slot = Slot {
            name,
            args: previous.map(|p| p.args.clone()).unwrap_or_default(),
            result: Some(js_slice(text_of(ev, "text"), TOOL_RESULT_LIMIT)),
            status,
            target,
            target_kind: previous.and_then(|p| p.target_kind.clone()),
            applied: ev.get("applied") == Some(&Value::Bool(true)),
        };
        self.put(at, slot);
    }

    /// `tools[at] = slot`: an array element below 2^32 - 1 (growing `length`), else a property
    /// nothing reads again.
    fn put(&mut self, at: f64, slot: Slot) {
        if let Some(i) = element(at) {
            self.tools.insert(i, slot);
            self.len = self.len.max(u64::from(i) + 1);
        }
    }

    /// The tool calls with nothing left running or waiting (settleToolCalls in the client).
    fn settled(&self) -> Vec<Slot> {
        self.tools
            .values()
            .map(|c| match c.status {
                Status::Done | Status::Denied => c.clone(),
                _ => Slot {
                    status: Status::Stopped,
                    ..c.clone()
                },
            })
            .collect()
    }

    /// The reply as it stood when the client went away, or `None` when there is nothing to keep
    /// (`assistantEntry()`).
    pub fn assistant_entry(&self) -> Option<Value> {
        let calls = self.settled();
        let applied = calls.iter().filter(|c| c.applied).count();
        let stopped_note = || {
            let mut m = Map::new();
            m.insert("reason".into(), "stopped".into());
            m.insert("applied".into(), applied.into());
            Value::Object(m)
        };
        let mut entry = Map::new();
        entry.insert("role".into(), "assistant".into());
        if self.failed {
            // A failed reply is kept only as the record of the changes it saved (#658).
            if applied == 0 {
                return None;
            }
            entry.insert("content".into(), "".into());
            let kept: Vec<Value> = calls
                .iter()
                .filter(|c| c.status != Status::Denied)
                .map(Slot::to_value)
                .collect();
            entry.insert("toolCalls".into(), kept.into());
            let note = match &self.paused {
                Some(p @ Paused::Declined { .. }) => p.to_value(),
                _ => stopped_note(),
            };
            entry.insert("paused".into(), note);
            return Some(Value::Object(entry));
        }
        let text = if js_trim(&self.content).is_empty() {
            self.narration.as_str()
        } else {
            js_trim_start(&self.content)
        };
        let note = match &self.paused {
            Some(p) => Some(p.to_value()),
            None => (applied > 0).then(stopped_note),
        };
        // No answer and no note: the Stopped placeholder (#634); its unfinished thinking goes.
        let stopped = js_trim(text).is_empty() && note.is_none();
        entry.insert("content".into(), if stopped { "" } else { text }.into());
        let model = if stopped {
            Some(STOPPED_SENDER.to_owned())
        } else {
            self.model.clone()
        };
        if let Some(model) = model {
            entry.insert("model".into(), model.into());
        }
        if let Some(decision) = self.routing_decision.as_ref().filter(|d| truthy(d)) {
            if !stopped {
                entry.insert("routingDecision".into(), decision.clone());
            }
        }
        if let Some(target) = &self.route_target {
            entry.insert("routeTarget".into(), target.clone());
        }
        if !self.reasoning.is_empty() && !stopped {
            entry.insert("reasoning".into(), self.reasoning.clone().into());
        }
        if !calls.is_empty() {
            let list: Vec<Value> = calls.iter().map(Slot::to_value).collect();
            entry.insert("toolCalls".into(), list.into());
        }
        if let Some(stats) = &self.stats {
            entry.insert("stats".into(), stats.clone());
        }
        if let Some(sources) = self.sources.as_ref().filter(|s| !s.is_empty()) {
            if !stopped {
                entry.insert("sources".into(), sources.clone().into());
            }
        }
        if let Some(note) = note {
            entry.insert("paused".into(), note);
        }
        Some(Value::Object(entry))
    }

    /// The reply so far marked `interrupted`, for the partial file and a stored turn whose client
    /// left (`partialEntry()`, #1507). Unlike [`TurnRecord::assistant_entry`] it keeps the thinking
    /// when no answer was written; a failed reply, or one with nothing at all, is `None`.
    pub fn partial_entry(&self) -> Option<Value> {
        if self.failed {
            return None;
        }
        let Some(Value::Object(base)) = self.assistant_entry() else {
            return None;
        };
        let content_empty = base.get("content").is_none_or(|c| !truthy(c));
        let stopped =
            base.get("model").and_then(Value::as_str) == Some(STOPPED_SENDER) && content_empty;
        let mut entry = if stopped {
            let mut m = Map::new();
            m.insert("role".into(), "assistant".into());
            m.insert("content".into(), "".into());
            m.insert("model".into(), STOPPED_SENDER.into());
            if let Some(calls) = base.get("toolCalls") {
                m.insert("toolCalls".into(), calls.clone());
            }
            m
        } else {
            base
        };
        if stopped && !js_trim(&self.reasoning).is_empty() {
            entry.insert("reasoning".into(), self.reasoning.clone().into());
        }
        let has = |key: &str| entry.get(key).is_some_and(truthy);
        let has_calls = entry
            .get("toolCalls")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty());
        if !has("content") && !has("reasoning") && !has_calls && !has("paused") {
            return None;
        }
        entry.insert("interrupted".into(), true.into());
        Some(Value::Object(entry))
    }
}

/// chat-partial.cjs `bounded(entry)`: the entry with the newest [`PARTIAL_CAP_UNITS`] of its
/// `content` (missing or `null`: `""`) and string `reasoning`, `savedAt: now_ms`, and
/// `truncated: true` when a string `content` was longer than the cap. Other keys are kept; a
/// non-object entry contributes none (Node is only ever given an entry object).
pub fn bounded(entry: &Value, now_ms: i64) -> Value {
    let mut out = entry.as_object().cloned().unwrap_or_default();
    let content = match entry.get("content") {
        None | Some(Value::Null) => Value::String(String::new()),
        Some(Value::String(s)) => Value::String(js_tail(s, PARTIAL_CAP_UNITS).to_owned()),
        Some(other) => other.clone(),
    };
    out.insert("content".into(), content);
    out.insert("savedAt".into(), now_ms.into());
    if let Some(Value::String(r)) = entry.get("reasoning") {
        out.insert(
            "reasoning".into(),
            js_tail(r, PARTIAL_CAP_UNITS).to_owned().into(),
        );
    }
    // `entry.content && entry.content.length > CAP`: of the JSON kinds only a string or an array
    // has a length.
    let long = match entry.get("content") {
        Some(Value::String(s)) => js_len(s) > PARTIAL_CAP_UNITS,
        Some(Value::Array(a)) => a.len() > PARTIAL_CAP_UNITS,
        _ => false,
    };
    if long {
        out.insert("truncated".into(), true.into());
    }
    Value::Object(out)
}

/// What `finish({ keep })` writes for a reply the transcript did not take: [`bounded`], marked
/// `interrupted`, without the `running` marker a live save carries.
pub fn partial_file(entry: &Value, now_ms: i64) -> Value {
    let mut out = bounded(entry, now_ms);
    if let Value::Object(m) = &mut out {
        m.insert("interrupted".into(), true.into());
    }
    out
}

/// `s.slice(-n)` by UTF-16 units, keeping whole characters: a pair cut in half loses its low half
/// too (JS keeps it, lone), so the result is then `n - 1` units.
fn js_tail(s: &str, n: usize) -> &str {
    let total = js_len(s);
    if total <= n {
        return s;
    }
    let skip = total - n;
    let mut units = 0;
    for (at, ch) in s.char_indices() {
        if units >= skip {
            return s.get(at..).unwrap_or("");
        }
        units += ch.len_utf16();
    }
    ""
}

/// `str(ev[key])`: the string, or `""` for anything else.
fn text_of<'a>(ev: &'a Map<String, Value>, key: &str) -> &'a str {
    ev.get(key).and_then(Value::as_str).unwrap_or("")
}

fn name_or_tool(name: &str) -> String {
    if name.is_empty() { "tool" } else { name }.to_owned()
}

/// `Number.isInteger(ev.index) && ev.index >= 0 ? ev.index : …`
fn index_of(ev: &Map<String, Value>) -> Option<f64> {
    let n = ev.get("index").and_then(Value::as_f64)?;
    (n.is_finite() && n.fract() == 0.0 && n >= 0.0).then_some(n)
}

/// The array element a non-negative integer index names, if it is one.
fn element(at: f64) -> Option<u32> {
    (0.0..=MAX_ARRAY_INDEX).contains(&at).then_some(at as u32)
}

fn paused_of(ev: &Map<String, Value>) -> Paused {
    let applied = match ev.get("applied") {
        Some(n @ Value::Number(_)) if n.as_f64().is_some_and(|x| x.fract() == 0.0 && x >= 0.0) => {
            n.clone()
        }
        _ => Value::from(0),
    };
    match ev.get("reason").and_then(Value::as_str) {
        Some("declined") => {
            let mut declined: Vec<String> = Vec::new();
            for name in ev
                .get("declined")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|n| tool_name(n))
            {
                if !declined.iter().any(|d| d == name) {
                    declined.push(name.to_owned());
                }
            }
            declined.truncate(DECLINED_CAP);
            Paused::Declined { applied, declined }
        }
        Some("sensitive-tool-result") => Paused::Sensitive(applied),
        _ => Paused::Supervision(applied),
    }
}

/// `/^[\w.-]{1,80}$/` (no `u` flag: `\w` is ASCII; `$` is the end of input).
fn tool_name(s: &str) -> bool {
    (1..=80).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// `/^[a-z][a-z0-9-]{0,39}$/`
fn detail_code(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && s.len() <= 40
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `!!value` for JSON data.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `String(value)` as a template literal applies it; `None` where JS throws a TypeError (an object
/// with an own `toString`, which JSON data cannot make callable, or nesting V8 gives up on).
fn js_string(v: &Value, depth: usize) -> Option<String> {
    if depth > 1000 {
        return None;
    }
    Some(match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Object(m) => {
            if m.contains_key("toString") {
                return None;
            }
            "[object Object]".into()
        }
        Value::Array(items) => {
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if !item.is_null() {
                    out.push_str(&js_string(item, depth + 1)?);
                }
            }
            out
        }
    })
}

/// JS `WhiteSpace` and `LineTerminator` (what `trim` removes): unlike `char::is_whitespace`, it
/// takes U+FEFF and leaves U+0085.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_space)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_cut_inside_a_pair_keeps_whole_characters() {
        let result = format!("{}😀tail", "a".repeat(TOOL_RESULT_LIMIT - 1));
        let mut r = TurnRecord::at(0);
        r.observe(&json!({ "type": "tool_result", "index": 0, "name": "qa_read", "text": result }));
        let entry = r.assistant_entry().unwrap();
        let kept = entry["toolCalls"][0]["result"].as_str().unwrap();
        assert_eq!(js_len(kept), TOOL_RESULT_LIMIT - 1);
        assert!(kept.bytes().all(|b| b == b'a'));

        let content = format!("😀{}", "b".repeat(PARTIAL_CAP_UNITS - 1));
        let out = bounded(&json!({ "content": content, "reasoning": content }), 7);
        assert_eq!(out["content"], "b".repeat(PARTIAL_CAP_UNITS - 1));
        assert_eq!(out["reasoning"], "b".repeat(PARTIAL_CAP_UNITS - 1));
        assert_eq!(out["truncated"], true);
        assert_eq!(out["savedAt"], 7);
        assert_eq!(js_tail("😀😀", 1), "");
        assert_eq!(js_tail("😀😀", 2), "😀");
        assert_eq!(js_tail("x😀", 2), "😀");
    }

    #[test]
    fn indexes_far_past_the_array_cost_nothing() {
        let mut r = TurnRecord::at(0);
        r.observe(&json!({ "type": "tool", "index": 4_294_967_294_u64, "name": "qa_last" }));
        r.observe(&json!({ "type": "tool", "index": 4_294_967_295_u64, "name": "qa_property" }));
        r.observe(&json!({ "type": "tool", "index": 1e300, "name": "qa_property" }));
        let calls = r.assistant_entry().unwrap()["toolCalls"].clone();
        assert_eq!(calls.as_array().unwrap().len(), 1);
        assert_eq!(calls[0]["name"], "qa_last");
        // length is now 2^32 - 1: an append lands on a property, as in JS.
        r.observe(&json!({ "type": "tool", "name": "qa_append" }));
        assert_eq!(
            r.assistant_entry().unwrap()["toolCalls"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
