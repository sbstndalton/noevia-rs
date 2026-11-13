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
//! Values are `js_json::JValue`: objects in Node's key order, numbers as JS numbers, written with
//! `js_json::wtf16::stringify` byte for byte as `JSON.stringify` writes them. Text is
//! `js_json::wtf16` text: a lone surrogate is held as one private-use character, every length, cut
//! and index is in UTF-16 code units, and joining two strings rejoins a lone high half and a lone
//! low half into the character they make, as JS concatenation does. So a cut inside a pair (the
//! 4000-unit tool result, the newest 64 Ki units of a partial field), a delta that splits an emoji,
//! or a narration that removes half of one gives exactly Node's string. Proven by
//! `tests/fixtures/node-turn-record.v1.json` (recorded from the Node functions by
//! `tests/record_entries.cjs`) and the whole turns of `node-turns.v1.json`, compared as bytes.
//!
//! Named differences from Node (each tested):
//! - **A meta route that `String()` cannot convert** (an object with its own `toString` key):
//!   Node's `observe` throws out of the stream writer; here the event is ignored, which leaves the
//!   record exactly as Node's throw does (the throw comes before any field is set).
//! - **Sparse tool indexes cost nothing.** Node's tool list is a JS array: an `index` near 2^32
//!   makes its `filter`/`findIndex` walk billions of holes (the turn hangs; the recording keeps
//!   indexes small for that reason). Here the slots are a map; indexes past the last array index
//!   (2^32 - 2), which JS stores as plain properties no later step reads, are ignored.
//! - **An object `content` with its own `length`** (never made by the record): `bounded` compares
//!   a number `length` with the cap as JS does (a boolean or `null` one is never over it, in JS
//!   either); a string, array or object one (JS: converted to a number first, and a TypeError for
//!   an own `toString`) never counts as over the cap.
//! - **No `Infinity` and no reserved characters in inputs.** `js_json::wtf16::parse` refuses a
//!   number outside the `f64` range (`JSON.parse`: `Infinity`, which `count()` drops) and text
//!   holding a character of U+10F800..=U+10FFFF (the range lone surrogates are held in), so such
//!   an event never reaches the record.

use std::collections::BTreeMap;

use js_json::{to_js_string, wtf16, JValue};

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
    fn announced(ev: &JValue, status: Status) -> Slot {
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

    /// Node's key order for all three makers: name, args, result, status, target, targetKind,
    /// applied.
    fn to_value(&self) -> JValue {
        let mut m = vec![
            ("name".to_owned(), str_value(&self.name)),
            ("args".to_owned(), str_value(&self.args)),
        ];
        if let Some(result) = &self.result {
            m.push(("result".into(), str_value(result)));
        }
        m.push(("status".into(), self.status.as_str().into()));
        if let Some(target) = &self.target {
            m.push(("target".into(), str_value(target)));
        }
        if let Some(kind) = &self.target_kind {
            m.push(("targetKind".into(), str_value(kind)));
        }
        if self.applied {
            m.push(("applied".into(), true.into()));
        }
        JValue::Obj(m)
    }
}

#[derive(Debug, Clone)]
enum Paused {
    Declined { applied: f64, declined: Vec<String> },
    Sensitive(f64),
    Supervision(f64),
}

impl Paused {
    fn to_value(&self) -> JValue {
        let (reason, applied) = match self {
            Paused::Declined { applied, .. } => ("declined", applied),
            Paused::Sensitive(applied) => ("sensitive", applied),
            Paused::Supervision(applied) => ("supervision", applied),
        };
        let mut m = vec![
            ("reason".to_owned(), reason.into()),
            ("applied".to_owned(), JValue::Num(*applied)),
        ];
        if let Paused::Declined { declined, .. } = self {
            let names = declined.iter().map(|n| str_value(n)).collect::<Vec<_>>();
            m.push(("declined".into(), JValue::Arr(names)));
        }
        JValue::Obj(m)
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
    /// What the last routed meta event carried (`Undefined` when it had none).
    routing_decision: JValue,
    route_target: Option<JValue>,
    sources: Option<Vec<JValue>>,
    stats: Option<JValue>,
    paused: Option<Paused>,
    failed: bool,
    /// The JS array's elements by index (holes are absent keys) and its `length`.
    tools: BTreeMap<u32, Slot>,
    len: u64,
}

impl TurnRecord {
    /// A record whose clock is `clock` (Node's injected `now`, in ms): read once now, as the
    /// start, and again at every `usage` event for `stats.elapsedMs`.
    pub fn new(mut clock: impl FnMut() -> i64 + Send + 'static) -> Self {
        let started_at = clock();
        TurnRecord {
            clock: Box::new(clock),
            started_at,
            content: String::new(),
            reasoning: String::new(),
            narration: String::new(),
            model: None,
            routing_decision: JValue::Undefined,
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

    /// Takes one event the turn sent (as `js_json::wtf16::parse` reads it). Anything that is not
    /// an object with a known string `type` is ignored.
    pub fn observe(&mut self, ev: &JValue) {
        if !ev.is_object() {
            return;
        }
        let Some(kind) = ev.get("type").as_str() else {
            return;
        };
        match kind {
            "meta" => self.meta(ev),
            "sources" => {
                if let JValue::Arr(list) = ev.get("sources") {
                    self.sources = Some(list.clone());
                }
            }
            "reasoning" => concat(&mut self.reasoning, text_of(ev, "text")),
            "preamble" => self.preamble(text_of(ev, "text")),
            "delta" => concat(&mut self.content, text_of(ev, "text")),
            "tool" => {
                let at = index_of(ev).unwrap_or(self.len as f64);
                self.put(at, Slot::announced(ev, Status::Running));
            }
            "tool_pending" => {
                let at = index_of(ev).unwrap_or(self.len.saturating_sub(1) as f64);
                let target = text_of(ev, "target");
                let kind = ev.get("targetKind").as_str();
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
                let mut stats = Vec::new();
                for key in COUNTS {
                    // count(): finite numbers only; the others are undefined and not written.
                    if let JValue::Num(n) = ev.get(key) {
                        if n.is_finite() {
                            stats.push((key.to_owned(), JValue::Num(*n)));
                        }
                    }
                }
                let elapsed = now.saturating_sub(self.started_at).max(0);
                stats.push(("elapsedMs".into(), elapsed.into()));
                self.stats = Some(JValue::Obj(stats));
            }
            "error" => self.failed = true,
            _ => {}
        }
    }

    fn meta(&mut self, ev: &JValue) {
        let route = ev.get("route");
        if route.truthy() {
            // Node's template literal throws here (an own `toString`); nothing is set then.
            let Ok(route) = to_js_string(route) else {
                return;
            };
            let mut model = "Assistant · Auto (".to_owned();
            concat(&mut model, &route);
            model.push(')');
            self.model = Some(model);
            self.routing_decision = ev.get("routingDecision").clone();
        }
        let routing = ev.get("routing");
        if !routing.is_object() {
            return;
        }
        let route = routing.get("route").as_str();
        let Some(route) = route.filter(|r| *r == "local" || *r == "cloud") else {
            return;
        };
        let Some(reason) = routing.get("reason").as_str() else {
            return;
        };
        let mut target = vec![
            ("route".to_owned(), route.into()),
            ("reason".to_owned(), str_value(reason)),
        ];
        if reason == "check-unavailable" {
            if let Some(detail) = routing.get("detail").as_str().filter(|d| detail_code(d)) {
                target.push(("detail".into(), detail.into()));
            }
        }
        self.route_target = Some(JValue::Obj(target));
    }

    /// Narration before a tool call belongs with the thinking, not the answer.
    fn preamble(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        remove_last(&mut self.content, text);
        let trimmed = js_trim(text);
        for field in [&mut self.reasoning, &mut self.narration] {
            if !field.is_empty() {
                field.push_str("\n\n");
            }
            concat(field, trimmed);
        }
    }

    fn tool_result(&mut self, ev: &JValue) {
        let at = match index_of(ev) {
            Some(at) => at,
            None => {
                // findIndex((t) => t && t.name === ev.name): only a string name can match.
                let wanted = ev.get("name").as_str();
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
        let status = if ev.get("declined") == &JValue::Bool(true) {
            Status::Denied
        } else if ev.get("notRun") == &JValue::Bool(true) {
            Status::Stopped
        } else {
            Status::Done
        };
        let slot = Slot {
            name,
            args: previous.map(|p| p.args.clone()).unwrap_or_default(),
            result: Some(wtf16::slice(text_of(ev, "text"), TOOL_RESULT_LIMIT)),
            status,
            target,
            target_kind: previous.and_then(|p| p.target_kind.clone()),
            applied: ev.get("applied") == &JValue::Bool(true),
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
    pub fn assistant_entry(&self) -> Option<JValue> {
        let calls = self.settled();
        let applied = calls.iter().filter(|c| c.applied).count();
        let stopped_note = || {
            JValue::obj([
                ("reason", JValue::from("stopped")),
                ("applied", JValue::Num(applied as f64)),
            ])
        };
        let mut entry = vec![("role".to_owned(), JValue::from("assistant"))];
        if self.failed {
            // A failed reply is kept only as the record of the changes it saved (#658).
            if applied == 0 {
                return None;
            }
            entry.push(("content".into(), "".into()));
            let kept = calls
                .iter()
                .filter(|c| c.status != Status::Denied)
                .map(Slot::to_value)
                .collect::<Vec<_>>();
            entry.push(("toolCalls".into(), JValue::Arr(kept)));
            let note = match &self.paused {
                Some(p @ Paused::Declined { .. }) => p.to_value(),
                _ => stopped_note(),
            };
            entry.push(("paused".into(), note));
            return Some(JValue::Obj(entry));
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
        entry.push(("content".into(), str_value(if stopped { "" } else { text })));
        let model = if stopped {
            Some(STOPPED_SENDER)
        } else {
            self.model.as_deref()
        };
        if let Some(model) = model {
            entry.push(("model".into(), str_value(model)));
        }
        if self.routing_decision.truthy() && !stopped {
            entry.push(("routingDecision".into(), self.routing_decision.clone()));
        }
        if let Some(target) = &self.route_target {
            entry.push(("routeTarget".into(), target.clone()));
        }
        if !self.reasoning.is_empty() && !stopped {
            entry.push(("reasoning".into(), str_value(&self.reasoning)));
        }
        if !calls.is_empty() {
            let list = calls.iter().map(Slot::to_value).collect::<Vec<_>>();
            entry.push(("toolCalls".into(), JValue::Arr(list)));
        }
        if let Some(stats) = &self.stats {
            entry.push(("stats".into(), stats.clone()));
        }
        if let Some(sources) = self.sources.as_ref().filter(|s| !s.is_empty()) {
            if !stopped {
                entry.push(("sources".into(), JValue::Arr(sources.clone())));
            }
        }
        if let Some(note) = note {
            entry.push(("paused".into(), note));
        }
        Some(JValue::Obj(entry))
    }

    /// The reply so far marked `interrupted`, for the partial file and a stored turn whose client
    /// left (`partialEntry()`, #1507). Unlike [`TurnRecord::assistant_entry`] it keeps the thinking
    /// when no answer was written; a failed reply, or one with nothing at all, is `None`.
    pub fn partial_entry(&self) -> Option<JValue> {
        if self.failed {
            return None;
        }
        let base = self.assistant_entry()?;
        let stopped =
            base.get("model").as_str() == Some(STOPPED_SENDER) && !base.get("content").truthy();
        let mut entry = if stopped {
            let mut m = vec![
                ("role".to_owned(), JValue::from("assistant")),
                ("content".to_owned(), JValue::from("")),
                ("model".to_owned(), JValue::from(STOPPED_SENDER)),
            ];
            let calls = base.get("toolCalls");
            if calls.truthy() {
                m.push(("toolCalls".into(), calls.clone()));
            }
            m
        } else {
            match base {
                JValue::Obj(items) => items,
                _ => return None,
            }
        };
        if stopped && !js_trim(&self.reasoning).is_empty() {
            wtf16::set(&mut entry, "reasoning", str_value(&self.reasoning));
        }
        let entry_value = JValue::Obj(entry);
        let has_calls = matches!(entry_value.get("toolCalls"), JValue::Arr(c) if !c.is_empty());
        if !entry_value.get("content").truthy()
            && !entry_value.get("reasoning").truthy()
            && !has_calls
            && !entry_value.get("paused").truthy()
        {
            return None;
        }
        let JValue::Obj(mut entry) = entry_value else {
            return None;
        };
        wtf16::set(&mut entry, "interrupted", true.into());
        Some(JValue::Obj(entry))
    }
}

/// chat-partial.cjs `bounded(entry)`: `{ ...entry, content, savedAt }` with the newest
/// [`PARTIAL_CAP_UNITS`] of a string `content` (missing or `null`: `""`) and of a string
/// `reasoning`, `savedAt: now_ms`, and `truncated: true` when `content` was longer than the cap.
/// Other keys keep their place; a non-object entry contributes none (Node is only ever given an
/// entry object).
pub fn bounded(entry: &JValue, now_ms: i64) -> JValue {
    let mut out = match entry {
        JValue::Obj(items) => items.clone(),
        _ => Vec::new(),
    };
    let content = match entry.get("content") {
        JValue::Undefined | JValue::Null => JValue::from(""),
        JValue::Str(s) => JValue::Str(tail(s, PARTIAL_CAP_UNITS)),
        other => other.clone(),
    };
    wtf16::set(&mut out, "content", content);
    wtf16::set(&mut out, "savedAt", now_ms.into());
    if let JValue::Str(r) = entry.get("reasoning") {
        wtf16::set(
            &mut out,
            "reasoning",
            JValue::Str(tail(r, PARTIAL_CAP_UNITS)),
        );
    }
    // `entry.content && entry.content.length > CAP`: a string's or array's length, or an
    // object's own `length` property (see the module doc for which kinds of it are compared).
    let cap = PARTIAL_CAP_UNITS as f64;
    let long = match entry.get("content") {
        JValue::Str(s) => wtf16::len(s) > PARTIAL_CAP_UNITS,
        JValue::Arr(a) => a.len() > PARTIAL_CAP_UNITS,
        JValue::Obj(_) => matches!(entry.get("content").get("length"), JValue::Num(n) if *n > cap),
        _ => false,
    };
    if long {
        wtf16::set(&mut out, "truncated", true.into());
    }
    JValue::Obj(out)
}

/// What `finish({ keep })` writes for a reply the transcript did not take: [`bounded`], marked
/// `interrupted`, without the `running` marker a live save carries.
pub fn partial_file(entry: &JValue, now_ms: i64) -> JValue {
    let mut out = bounded(entry, now_ms);
    if let JValue::Obj(items) = &mut out {
        wtf16::set(items, "interrupted", true.into());
    }
    out
}

/// A `wtf16` string as UTF-16 code units.
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u16; 2];
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// UTF-16 code units as a `wtf16` string (pairs joined, lone surrogates mapped).
fn from_units(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{fffd}'))
        })
        .collect()
}

/// `a += b` for JS strings: a lone high surrogate ending `a` and a lone low one starting `b`
/// become the one character they are.
fn concat(a: &mut String, b: &str) {
    let mut rest = b.chars();
    let hi = a.chars().next_back().and_then(wtf16::lone_unit);
    let lo = rest.clone().next().and_then(wtf16::lone_unit);
    if let (Some(hi @ 0xd800..=0xdbff), Some(lo @ 0xdc00..=0xdfff)) = (hi, lo) {
        a.pop();
        a.push_str(&from_units(&[hi, lo]));
        rest.next();
        a.push_str(rest.as_str());
    } else {
        a.push_str(b);
    }
}

/// `content = content.slice(0, at) + content.slice(at + text.length)` for
/// `at = content.lastIndexOf(text)`, in UTF-16 units (a match may start or end inside a pair when
/// `text` holds a lone surrogate).
fn remove_last(content: &mut String, text: &str) {
    if !text.chars().any(|c| wtf16::lone_unit(c).is_some()) {
        // A well-formed needle only matches whole characters, so the last byte match is the
        // same occurrence.
        if let Some(at) = content.rfind(text) {
            content.replace_range(at..at + text.len(), "");
        }
        return;
    }
    let hay = units(content);
    let needle = units(text);
    if needle.is_empty() {
        return;
    }
    if let Some(at) = hay.windows(needle.len()).rposition(|w| w == needle) {
        let before = hay.get(..at).unwrap_or_default();
        let after = hay.get(at + needle.len()..).unwrap_or_default();
        *content = from_units(&[before, after].concat());
    }
}

/// `s.slice(-n)` in UTF-16 units (a pair cut in half keeps its lone low half, as in JS).
fn tail(s: &str, n: usize) -> String {
    if wtf16::len(s) <= n {
        return s.to_owned();
    }
    let all = units(s);
    from_units(all.get(all.len().saturating_sub(n)..).unwrap_or_default())
}

fn str_value(s: &str) -> JValue {
    JValue::Str(s.to_owned())
}

/// `str(ev[key])`: the string, or `""` for anything else.
fn text_of<'a>(ev: &'a JValue, key: &str) -> &'a str {
    ev.get(key).as_str().unwrap_or("")
}

fn name_or_tool(name: &str) -> String {
    if name.is_empty() { "tool" } else { name }.to_owned()
}

/// `Number.isInteger(ev.index) && ev.index >= 0 ? ev.index : …`
fn index_of(ev: &JValue) -> Option<f64> {
    let n = ev.get("index").as_num()?;
    (n.is_finite() && n.fract() == 0.0 && n >= 0.0).then_some(n)
}

/// The array element a non-negative integer index names, if it is one.
fn element(at: f64) -> Option<u32> {
    (0.0..=MAX_ARRAY_INDEX).contains(&at).then_some(at as u32)
}

fn paused_of(ev: &JValue) -> Paused {
    let applied = ev
        .get("applied")
        .as_num()
        .filter(|x| x.is_finite() && x.fract() == 0.0 && *x >= 0.0)
        .unwrap_or(0.0);
    match ev.get("reason").as_str() {
        Some("declined") => {
            let mut declined: Vec<String> = Vec::new();
            let list = match ev.get("declined") {
                JValue::Arr(items) => items.as_slice(),
                _ => &[],
            };
            for name in list
                .iter()
                .filter_map(JValue::as_str)
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

    fn ev(text: &str) -> JValue {
        wtf16::parse(text).unwrap()
    }

    #[test]
    fn halves_of_a_pair_are_held_and_rejoined() {
        let mut r = TurnRecord::at(0);
        r.observe(&ev(r#"{"type":"delta","text":"QA \ud83d"}"#));
        r.observe(&ev(r#"{"type":"delta","text":"\ude00 😀"}"#));
        // lastIndexOf of a lone high half finds it inside the last pair.
        r.observe(&ev(r#"{"type":"preamble","text":"\ud83d"}"#));
        let entry = wtf16::stringify(&r.assistant_entry().unwrap()).unwrap();
        assert_eq!(
            entry,
            r#"{"role":"assistant","content":"QA 😀 \ude00","reasoning":"\ud83d"}"#
        );
        assert_eq!(
            tail("😀😀", 1),
            wtf16::lone_char(0xde00).unwrap().to_string()
        );
        assert_eq!(tail("x😀", 2), "😀");
    }

    #[test]
    fn indexes_far_past_the_array_cost_nothing() {
        let mut r = TurnRecord::at(0);
        r.observe(&ev(
            r#"{"type":"tool","index":4294967294,"name":"qa_last"}"#,
        ));
        r.observe(&ev(
            r#"{"type":"tool","index":4294967295,"name":"qa_property"}"#,
        ));
        r.observe(&ev(r#"{"type":"tool","index":1e300,"name":"qa_property"}"#));
        let calls = r.assistant_entry().unwrap().get("toolCalls").clone();
        assert!(matches!(&calls, JValue::Arr(c) if c.len() == 1));
        // length is now 2^32 - 1: an append lands on a property, as in JS.
        r.observe(&ev(r#"{"type":"tool","name":"qa_append"}"#));
        assert_eq!(r.assistant_entry().unwrap().get("toolCalls"), &calls);
    }
}
