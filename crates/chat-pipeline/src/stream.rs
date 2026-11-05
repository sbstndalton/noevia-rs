//! One round's engine stream read as core chat.cjs reads it (the loop at `for await (const chunk of
//! withIdleLimit(upstream.body ...))`): the bytes decoded as a streaming TextDecoder does, lines
//! split at `\n`, `data:` payloads parsed, and each event turned into the chat events the turn sends
//! (`reasoning`, `delta`, the accumulated `tool` slot per index, the `length` warning, an in-stream
//! provider `error`), with the round's state the loop keeps.
use crate::failure;
use crate::search::clean_search_args;
use crate::text::{as_str, get, spread, trim};
use js_json::{wtf16, JValue};

/// chat.cjs `MAX_TOOL_ARGS_UNITS`.
pub const MAX_TOOL_ARGS_UNITS: usize = 8 * 1024 * 1024;
const LENGTH_WARNING: &str = "The model reached its thinking/answer token budget. This reply may be incomplete; try Low thinking or a narrower question.";

/// What the loop does with one step of the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// `send(event)`.
    Send(JValue),
    /// `markFirstOutput()`: the first output of the round (and maybe of the exchange).
    FirstOutput,
    /// `reportRoundUsage(usage, roundTimings)`.
    Usage { usage: JValue, timings: JValue },
    /// The provider's error was sent; the response ends and the turn returns.
    End,
}

/// One tool call as the round assembles it (`toolCalls` slot).
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub id: JValue,
    pub id_made: bool,
    pub name: String,
    pub args: String,
    pub too_large: bool,
    pub name_cut: bool,
}

/// The round's state, as the loop's variables hold it.
pub struct Round {
    chatgpt_provider: bool,
    tool_offset: f64,
    /// The turn's TextDecoder (one per turn in chat.cjs): bytes of a character split across chunks.
    pub pending: Vec<u8>,
    buffer: String,
    /// `toolCalls`, by index in first-insertion order (a JS `Map`; keys by SameValueZero).
    pub tool_calls: Vec<(f64, Slot)>,
    pub saw_anything: bool,
    pub round_reasoning: String,
    pub round_has_content: bool,
    pub round_content: String,
    /// `shownText` (carried across rounds by the caller).
    pub shown_text: bool,
    pub round_timings: JValue,
    pub ended: bool,
}

/// `String(v)` (`+=` on a string).
fn plus(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `shownToolName`.
fn shown_tool_name(name: &str) -> String {
    wtf16::slice(name, 64)
}

fn same_key(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

impl Round {
    pub fn new(chatgpt_provider: bool, tool_offset: usize, shown_text: bool) -> Self {
        Self {
            chatgpt_provider,
            tool_offset: tool_offset as f64,
            pending: Vec::new(),
            buffer: String::new(),
            tool_calls: Vec::new(),
            saw_anything: false,
            round_reasoning: String::new(),
            round_has_content: false,
            round_content: String::new(),
            shown_text,
            round_timings: JValue::Undefined,
            ended: false,
        }
    }

    /// `decoder.decode(chunk, {stream: true})`: complete characters now, a split one when it completes;
    /// a byte sequence that can never be valid becomes U+FFFD.
    fn decode(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(good).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = bad.get(n..).unwrap_or(&[]);
                        }
                        None => {
                            rest = bad;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }

    /// One chunk of the engine's body.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Step> {
        let mut steps = Vec::new();
        if self.ended {
            return steps;
        }
        let text = self.decode(chunk);
        self.buffer.push_str(&text);
        while let Some(idx) = self.buffer.find('\n') {
            let line = trim(self.buffer.get(..idx).unwrap_or("")).to_owned();
            self.buffer = self.buffer.get(idx + 1..).unwrap_or("").to_owned();
            let Some(after) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = trim(after);
            if payload == "[DONE]" {
                continue;
            }
            let Ok(evt) = wtf16::parse(payload) else {
                continue;
            };
            if self.event(&evt, &mut steps).is_err() {
                // A TypeError inside the handler: the rest of this event is skipped (the loop's catch).
            }
            if self.ended {
                return steps;
            }
        }
        steps
    }

    /// One parsed event; `Err` where Node's handler throws a TypeError.
    fn event(&mut self, evt: &JValue, steps: &mut Vec<Step>) -> Result<(), ()> {
        if evt.is_nullish() {
            return Err(());
        }
        let choice0 = match get(evt, "choices") {
            JValue::Arr(a) => a.first().cloned().unwrap_or(JValue::Undefined),
            JValue::Obj(o) => o
                .iter()
                .find(|(k, _)| k == "0")
                .map_or(JValue::Undefined, |(_, v)| v.clone()),
            _ => JValue::Undefined,
        };
        if as_str(get(&choice0, "finish_reason")) == Some("length") {
            steps.push(Step::Send(obj(vec![
                ("type", s("warning")),
                ("text", s(LENGTH_WARNING)),
            ])));
        }
        let error = get(evt, "error");
        if error.truthy() {
            let text = match get(error, "message") {
                JValue::Str(m)
                    if self.chatgpt_provider && as_str(get(error, "source")) == Some("chatgpt") =>
                {
                    wtf16::slice(m, 300)
                }
                _ => failure::provider_error(error, None, self.shown_text),
            };
            steps.push(Step::Send(obj(vec![
                ("type", s("error")),
                ("text", JValue::Str(text)),
            ])));
            steps.push(Step::End);
            self.ended = true;
            return Ok(());
        }
        let d = get(&choice0, "delta");
        let delta = if d.truthy() {
            d.clone()
        } else {
            JValue::Obj(Vec::new())
        };
        let tool_calls = match get(&delta, "tool_calls") {
            JValue::Arr(a) => Some(a.clone()),
            _ => None,
        };
        let meaningful = tool_calls.as_ref().is_some_and(|tcs| {
            tcs.iter().any(|tc| {
                let f = get(tc, "function");
                get(tc, "id").truthy() || get(f, "name").truthy() || get(f, "arguments").truthy()
            })
        });
        if get(&delta, "content").truthy()
            || get(&delta, "reasoning_content").truthy()
            || get(&delta, "reasoning").truthy()
            || meaningful
        {
            steps.push(Step::FirstOutput);
        }
        let timings = get(evt, "timings");
        if timings.truthy() && matches!(timings, JValue::Obj(_) | JValue::Arr(_)) {
            let mut merged = spread(&self.round_timings);
            if let (JValue::Obj(m), JValue::Obj(add)) = (&mut merged, spread(timings)) {
                for (k, v) in add {
                    wtf16::set(m, &k, v);
                }
            }
            self.round_timings = merged;
        }
        let usage = get(evt, "usage");
        if usage.truthy() {
            steps.push(Step::Usage {
                usage: usage.clone(),
                timings: self.round_timings.clone(),
            });
        }
        for key in ["reasoning_content", "reasoning"] {
            let r = get(&delta, key);
            if r.truthy() {
                self.saw_anything = true;
                self.round_reasoning.push_str(&plus(r));
                steps.push(Step::Send(obj(vec![
                    ("type", s("reasoning")),
                    ("text", r.clone()),
                ])));
            }
        }
        let c = get(&delta, "content");
        if c.truthy() {
            self.saw_anything = true;
            self.round_has_content = true;
            self.shown_text = true;
            self.round_content.push_str(&plus(c));
            steps.push(Step::Send(obj(vec![
                ("type", s("delta")),
                ("text", c.clone()),
            ])));
        }
        if let Some(tcs) = tool_calls {
            self.saw_anything = true;
            for tc in &tcs {
                if tc.is_nullish() {
                    return Err(());
                }
                let i = match get(tc, "index") {
                    JValue::Num(n) => *n,
                    _ => 0.0,
                };
                let mut slot = self
                    .tool_calls
                    .iter()
                    .find(|(k, _)| same_key(*k, i))
                    .map(|(_, s)| s.clone())
                    .unwrap_or_else(|| Slot {
                        id: JValue::Str(format!("call-{}", js_json::number_to_string(i))),
                        id_made: true,
                        name: String::new(),
                        args: String::new(),
                        too_large: false,
                        name_cut: false,
                    });
                let id = get(tc, "id");
                if id.truthy() {
                    if wtf16::len(&plus(id)) > MAX_TOOL_ARGS_UNITS {
                        slot.too_large = true;
                        slot.args.clear();
                    } else {
                        slot.id = id.clone();
                        slot.id_made = false;
                    }
                }
                let f = get(tc, "function");
                let name = get(f, "name");
                if name.truthy() && !slot.name_cut {
                    let add = plus(name);
                    if wtf16::len(&slot.name) + wtf16::len(&add) > MAX_TOOL_ARGS_UNITS {
                        slot.too_large = true;
                        slot.name_cut = true;
                        slot.args.clear();
                        slot.name = shown_tool_name(&slot.name);
                    } else {
                        slot.name.push_str(&add);
                    }
                }
                let a = get(f, "arguments");
                if a.truthy() && !slot.too_large {
                    let add = plus(a);
                    if wtf16::len(&slot.args) + wtf16::len(&add) > MAX_TOOL_ARGS_UNITS {
                        slot.too_large = true;
                        slot.args.clear();
                    } else {
                        slot.args.push_str(&add);
                    }
                }
                match self.tool_calls.iter_mut().find(|(k, _)| same_key(*k, i)) {
                    Some(entry) => entry.1 = slot.clone(),
                    None => self
                        .tool_calls
                        .push((if i == 0.0 { 0.0 } else { i }, slot.clone())),
                }
                let args = clean_search_args(
                    &JValue::Str(slot.name.clone()),
                    &JValue::Str(slot.args.clone()),
                );
                steps.push(Step::Send(obj(vec![
                    ("type", s("tool")),
                    ("index", JValue::Num(self.tool_offset + i)),
                    ("name", JValue::Str(slot.name.clone())),
                    ("args", args),
                ])));
            }
        }
        Ok(())
    }
}

fn s(v: &str) -> JValue {
    JValue::Str(v.into())
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// A `Set` key with SameValueZero equality (objects never equal: each parsed object is its own).
fn id_key(v: &JValue) -> Option<String> {
    match v {
        JValue::Obj(_) | JValue::Arr(_) => None,
        JValue::Num(n) if *n == 0.0 => Some("n0".into()),
        JValue::Num(n) if n.is_nan() => Some("nNaN".into()),
        other => wtf16::stringify(other).map(|s| format!("v{s}")),
    }
}

impl Round {
    /// The round's one non-streaming retry when the stream showed nothing (`!sawAnything`): the
    /// reply body's message as events and tool slots, then its usage. `body` is the parsed reply.
    pub fn fallback(&mut self, body: &JValue) -> Vec<Step> {
        let mut steps = Vec::new();
        let choice0 = match get(body, "choices") {
            JValue::Arr(a) => a.first().cloned().unwrap_or(JValue::Undefined),
            _ => JValue::Undefined,
        };
        let msg = get(&choice0, "message").clone();
        let calls = match get(&msg, "tool_calls") {
            JValue::Arr(a) => Some(a.clone()),
            _ => None,
        };
        if get(&msg, "reasoning_content").truthy() || get(&msg, "content").truthy() || calls.as_ref().is_some_and(|c| !c.is_empty()) {
            steps.push(Step::FirstOutput);
        }
        let r = get(&msg, "reasoning_content");
        if r.truthy() {
            self.round_reasoning.push_str(&plus(r));
            steps.push(Step::Send(obj(vec![("type", s("reasoning")), ("text", r.clone())])));
        }
        let c = get(&msg, "content");
        if c.truthy() {
            self.round_has_content = true;
            self.shown_text = true;
            self.round_content.push_str(&plus(c));
            steps.push(Step::Send(obj(vec![("type", s("delta")), ("text", c.clone())])));
        }
        for tc in calls.unwrap_or_default() {
            let index = self.tool_calls.len() as f64;
            let f = get(&tc, "function");
            let or_empty = |v: &JValue| if v.truthy() { plus(v) } else { String::new() };
            let too_large = [get(&tc, "id"), get(f, "name"), get(f, "arguments")]
                .iter()
                .any(|v| wtf16::len(&or_empty(v)) > MAX_TOOL_ARGS_UNITS);
            let name = if too_large { shown_tool_name(&or_empty(get(f, "name"))) } else { or_empty(get(f, "name")) };
            let id = get(&tc, "id");
            let made = too_large || !id.truthy();
            let args = if too_large { String::new() } else { or_empty(get(f, "arguments")) };
            self.tool_calls.push((
                index,
                Slot {
                    id: if made { JValue::Str(format!("call-{}", js_json::number_to_string(index))) } else { id.clone() },
                    id_made: made,
                    name: name.clone(),
                    args: args.clone(),
                    too_large,
                    name_cut: false,
                },
            ));
            let shown = if too_large {
                JValue::Str(String::new())
            } else {
                clean_search_args(&JValue::Str(or_empty(get(f, "name"))), &JValue::Str(args))
            };
            steps.push(Step::Send(obj(vec![
                ("type", s("tool")),
                ("index", JValue::Num(self.tool_offset + index)),
                ("name", JValue::Str(name)),
                ("args", shown),
            ])));
        }
        steps.push(Step::Usage { usage: get(body, "usage").clone(), timings: get(body, "timings").clone() });
        self.saw_anything = true;
        steps
    }

    /// Before the calls run: each call's arguments as they will run (`{}` for an oversized call, a
    /// search query cleaned, #1523), every id unique (the first real occurrence kept, the others
    /// `call-<i>[-<k>]`), the round's text sent as `preamble`, and the assistant message that
    /// carries the calls.
    pub fn prepare_calls(&mut self) -> (Vec<Step>, JValue) {
        for (_, c) in &mut self.tool_calls {
            c.args = if c.too_large {
                "{}".into()
            } else {
                match clean_search_args(&JValue::Str(c.name.clone()), &JValue::Str(c.args.clone())) {
                    JValue::Str(a) => a,
                    other => plus(&other),
                }
            };
        }
        let mut used: std::collections::HashSet<String> =
            self.tool_calls.iter().filter(|(_, c)| !c.id_made).filter_map(|(_, c)| id_key(&c.id)).collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (i, c) in &mut self.tool_calls {
            let key = id_key(&c.id);
            if !c.id_made && !key.as_ref().is_some_and(|k| seen.contains(k)) {
                if let Some(k) = key {
                    seen.insert(k);
                }
                continue;
            }
            let base = format!("call-{}", js_json::number_to_string(*i));
            let mut id = base.clone();
            let mut k = 2;
            while used.contains(&format!("v{}", wtf16::stringify(&JValue::Str(id.clone())).unwrap_or_default())) {
                id = format!("{base}-{k}");
                k += 1;
            }
            used.insert(format!("v{}", wtf16::stringify(&JValue::Str(id.clone())).unwrap_or_default()));
            c.id = JValue::Str(id);
        }
        let mut steps = Vec::new();
        if !trim(&self.round_content).is_empty() {
            steps.push(Step::Send(obj(vec![("type", s("preamble")), ("text", JValue::Str(self.round_content.clone()))])));
            self.round_has_content = false;
        }
        let calls = self
            .tool_calls
            .iter()
            .map(|(_, c)| {
                obj(vec![
                    ("id", c.id.clone()),
                    ("type", s("function")),
                    ("function", obj(vec![("name", JValue::Str(c.name.clone())), ("arguments", JValue::Str(c.args.clone()))])),
                ])
            })
            .collect();
        let message = obj(vec![("role", s("assistant")), ("content", JValue::Null), ("tool_calls", JValue::Arr(calls))]);
        (steps, message)
    }
}
