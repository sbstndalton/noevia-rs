//! The reply a turn keeps when its client went away (core chat-interrupted-turn.cjs
//! `createTurnRecord`, #679, #1507): the turn follows its own stream events and builds the
//! transcript entry the client would have saved (partial text, unfinished tool calls stopped,
//! applied writes kept with their note). The append itself is the store's
//! (crates/server-projects transcript `append_if_last`).
use crate::text::{as_str, get, trim};
use js_json::{wtf16, JValue};
use std::collections::BTreeMap;

/// src/chat-labels.ts `STOPPED_SENDER`.
pub const STOPPED_SENDER: &str = "Stopped";
/// src/components/ToolCalls.tsx `TOOL_RESULT_LIMIT`.
const TOOL_RESULT_LIMIT: usize = 4000;
/// The largest JavaScript array index.
const MAX_INDEX: f64 = 4_294_967_294.0;

fn str_of(v: &JValue) -> String {
    as_str(v).unwrap_or("").to_owned()
}

fn count(v: &JValue) -> JValue {
    match v {
        JValue::Num(n) if n.is_finite() => JValue::Num(*n),
        _ => JValue::Undefined,
    }
}

fn tool_name_ok(n: &str) -> bool {
    (1..=80).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

fn detail_code_ok(d: &str) -> bool {
    let b = d.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b.first().is_some_and(u8::is_ascii_lowercase)
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `Number.isInteger(v) && v >= 0`.
fn index(v: &JValue) -> Option<f64> {
    match v {
        JValue::Num(n) if n.is_finite() && n.fract() == 0.0 && *n >= 0.0 => Some(*n),
        _ => None,
    }
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().filter(|(_, v)| !matches!(v, JValue::Undefined)).map(|(k, v)| (k.to_owned(), v)).collect())
}

/// The tool calls as the JS array holds them: elements up to the largest array index (holes are
/// absent), and larger "indices" as plain properties that no array method sees.
#[derive(Default)]
struct Tools {
    elements: BTreeMap<u64, JValue>,
    properties: BTreeMap<String, JValue>,
}

impl Tools {
    fn len(&self) -> f64 {
        self.elements.keys().next_back().map_or(0.0, |k| (*k + 1) as f64)
    }
    fn get(&self, at: f64) -> Option<&JValue> {
        if at <= MAX_INDEX {
            self.elements.get(&(at as u64))
        } else {
            self.properties.get(&js_json::number_to_string(at))
        }
    }
    fn set(&mut self, at: f64, v: JValue) {
        if at <= MAX_INDEX {
            self.elements.insert(at as u64, v);
        } else {
            self.properties.insert(js_json::number_to_string(at), v);
        }
    }
    fn calls(&self) -> Vec<JValue> {
        self.elements.values().cloned().collect()
    }
}

/// One reply's record, fed every event the turn sends.
pub struct TurnRecord {
    started_at: f64,
    content: String,
    reasoning: String,
    narration: String,
    model: JValue,
    routing_decision: JValue,
    route_target: Option<JValue>,
    sources: Option<Vec<JValue>>,
    stats: Option<JValue>,
    paused: Option<JValue>,
    failed: bool,
    tools: Tools,
}

impl TurnRecord {
    /// `createTurnRecord({now})`, at `now` (ms).
    pub fn new(now: f64) -> Self {
        Self {
            started_at: now,
            content: String::new(),
            reasoning: String::new(),
            narration: String::new(),
            model: JValue::Undefined,
            routing_decision: JValue::Undefined,
            route_target: None,
            sources: None,
            stats: None,
            paused: None,
            failed: false,
            tools: Tools::default(),
        }
    }

    /// `observe(ev)`; `now` is read only by a `usage` event.
    pub fn observe(&mut self, ev: &JValue, now: impl FnOnce() -> f64) {
        if !matches!(ev, JValue::Obj(_) | JValue::Arr(_)) {
            return;
        }
        match as_str(get(ev, "type")).unwrap_or("") {
            "meta" => {
                let route = get(ev, "route");
                if route.truthy() {
                    let r = js_json::to_js_string(route).unwrap_or_default();
                    self.model = JValue::Str(format!("Assistant · Auto ({r})"));
                    self.routing_decision = get(ev, "routingDecision").clone();
                }
                let routing = get(ev, "routing");
                let rr = as_str(get(routing, "route"));
                if routing.truthy() && matches!(rr, Some("local" | "cloud")) {
                    if let Some(reason) = as_str(get(routing, "reason")) {
                        let cause = (reason == "check-unavailable")
                            .then(|| as_str(get(routing, "detail")).filter(|d| detail_code_ok(d)))
                            .flatten();
                        let mut t = vec![
                            ("route", JValue::Str(rr.unwrap_or_default().into())),
                            ("reason", JValue::Str(reason.into())),
                        ];
                        if let Some(c) = cause {
                            t.push(("detail", JValue::Str(c.into())));
                        }
                        self.route_target = Some(obj(t));
                    }
                }
            }
            "sources" => {
                if let JValue::Arr(a) = get(ev, "sources") {
                    self.sources = Some(a.clone());
                }
            }
            "reasoning" => self.reasoning.push_str(&str_of(get(ev, "text"))),
            "preamble" => {
                let text = str_of(get(ev, "text"));
                if text.is_empty() {
                    return;
                }
                if let Some(at) = self.content.rfind(&text) {
                    self.content.replace_range(at..at + text.len(), "");
                }
                let t = trim(&text);
                if !self.reasoning.is_empty() {
                    self.reasoning.push_str("\n\n");
                }
                self.reasoning.push_str(t);
                if !self.narration.is_empty() {
                    self.narration.push_str("\n\n");
                }
                self.narration.push_str(t);
            }
            "delta" => self.content.push_str(&str_of(get(ev, "text"))),
            "tool" => {
                let at = index(get(ev, "index")).unwrap_or(self.tools.len());
                let name = str_of(get(ev, "name"));
                self.tools.set(
                    at,
                    obj(vec![
                        ("name", JValue::Str(if name.is_empty() { "tool".into() } else { name })),
                        ("args", JValue::Str(str_of(get(ev, "args")))),
                        ("status", JValue::Str("running".into())),
                    ]),
                );
            }
            "tool_pending" => {
                let at = index(get(ev, "index")).unwrap_or((self.tools.len() - 1.0).max(0.0));
                let name = str_of(get(ev, "name"));
                let mut t = vec![
                    ("name", JValue::Str(if name.is_empty() { "tool".into() } else { name })),
                    ("args", JValue::Str(str_of(get(ev, "args")))),
                    ("status", JValue::Str("pending".into())),
                ];
                if !str_of(get(ev, "target")).is_empty() {
                    t.push(("target", get(ev, "target").clone()));
                }
                if matches!(as_str(get(ev, "targetKind")), Some("drive" | "drive-new")) {
                    t.push(("targetKind", get(ev, "targetKind").clone()));
                }
                self.tools.set(at, obj(t));
            }
            "tool_result" => {
                let name_ev = get(ev, "name");
                let at = match index(get(ev, "index")) {
                    Some(i) => i,
                    None => {
                        let found = as_str(name_ev).and_then(|n| {
                            self.tools.elements.iter().find(|(_, t)| as_str(get(t, "name")) == Some(n)).map(|(k, _)| *k as f64)
                        });
                        found.unwrap_or(self.tools.len())
                    }
                };
                let previous = self.tools.get(at).cloned();
                let prev = previous.as_ref();
                let target = {
                    let t = str_of(get(ev, "target"));
                    if t.is_empty() {
                        prev.map(|p| get(p, "target").clone()).unwrap_or(JValue::Undefined)
                    } else {
                        JValue::Str(t)
                    }
                };
                let name = {
                    let n = str_of(name_ev);
                    if !n.is_empty() {
                        JValue::Str(n)
                    } else {
                        match prev.map(|p| get(p, "name")) {
                            Some(v) if v.truthy() => v.clone(),
                            _ => JValue::Str("tool".into()),
                        }
                    }
                };
                let status = if get(ev, "declined") == &JValue::Bool(true) {
                    "denied"
                } else if get(ev, "notRun") == &JValue::Bool(true) {
                    "stopped"
                } else {
                    "done"
                };
                let mut t = vec![
                    ("name", name),
                    ("args", prev.map(|p| get(p, "args").clone()).unwrap_or(JValue::Str(String::new()))),
                    ("result", JValue::Str(wtf16::slice(&str_of(get(ev, "text")), TOOL_RESULT_LIMIT))),
                    ("status", JValue::Str(status.into())),
                ];
                if target.truthy() {
                    t.push(("target", target));
                }
                if let Some(k) = prev.map(|p| get(p, "targetKind")).filter(|k| k.truthy()) {
                    t.push(("targetKind", k.clone()));
                }
                if get(ev, "applied") == &JValue::Bool(true) {
                    t.push(("applied", JValue::Bool(true)));
                }
                self.tools.set(at, obj(t));
            }
            "paused" => {
                let applied = index(get(ev, "applied")).unwrap_or(0.0);
                let reason = as_str(get(ev, "reason"));
                self.paused = Some(match reason {
                    Some("declined") => {
                        let mut names: Vec<String> = Vec::new();
                        if let JValue::Arr(list) = get(ev, "declined") {
                            for n in list.iter().filter_map(as_str).filter(|n| tool_name_ok(n)) {
                                if !names.iter().any(|x| x == n) {
                                    names.push(n.to_owned());
                                }
                            }
                        }
                        names.truncate(8);
                        obj(vec![
                            ("reason", JValue::Str("declined".into())),
                            ("applied", JValue::Num(applied)),
                            ("declined", JValue::Arr(names.into_iter().map(JValue::Str).collect())),
                        ])
                    }
                    Some("sensitive-tool-result") => obj(vec![("reason", JValue::Str("sensitive".into())), ("applied", JValue::Num(applied))]),
                    _ => obj(vec![("reason", JValue::Str("supervision".into())), ("applied", JValue::Num(applied))]),
                });
            }
            "usage" => {
                let elapsed = (now() - self.started_at).max(0.0);
                self.stats = Some(obj(vec![
                    ("promptTokens", count(get(ev, "promptTokens"))),
                    ("completionTokens", count(get(ev, "completionTokens"))),
                    ("totalTokens", count(get(ev, "totalTokens"))),
                    ("tokensPerSecond", count(get(ev, "tokensPerSecond"))),
                    ("elapsedMs", JValue::Num(elapsed)),
                ]));
            }
            "error" => self.failed = true,
            _ => {}
        }
    }

    fn settled_calls(&self) -> Vec<JValue> {
        self.tools
            .calls()
            .into_iter()
            .filter(JValue::truthy)
            .map(|c| match as_str(get(&c, "status")) {
                Some("done" | "denied") => c,
                _ => {
                    let mut c = c;
                    crate::text::set(&mut c, "status", JValue::Str("stopped".into()));
                    c
                }
            })
            .collect()
    }

    /// `assistantEntry()`: the reply as it stood when the client went away, or `None`.
    pub fn assistant_entry(&self) -> Option<JValue> {
        let calls = self.settled_calls();
        let applied = calls.iter().filter(|c| get(c, "applied") == &JValue::Bool(true)).count() as f64;
        if self.failed {
            if applied == 0.0 {
                return None;
            }
            let paused = match &self.paused {
                Some(p) if as_str(get(p, "reason")) == Some("declined") => p.clone(),
                _ => obj(vec![("reason", JValue::Str("stopped".into())), ("applied", JValue::Num(applied))]),
            };
            return Some(obj(vec![
                ("role", JValue::Str("assistant".into())),
                ("content", JValue::Str(String::new())),
                ("toolCalls", JValue::Arr(calls.into_iter().filter(|c| as_str(get(c, "status")) != Some("denied")).collect())),
                ("paused", paused),
            ]));
        }
        let text = if trim(&self.content).is_empty() {
            self.narration.clone()
        } else {
            self.content.trim_start_matches(js_space).to_owned()
        };
        let note = self
            .paused
            .clone()
            .or_else(|| (applied > 0.0).then(|| obj(vec![("reason", JValue::Str("stopped".into())), ("applied", JValue::Num(applied))])));
        let stopped = trim(&text).is_empty() && note.is_none();
        let mut e = vec![
            ("role", JValue::Str("assistant".into())),
            ("content", JValue::Str(if stopped { String::new() } else { text })),
            ("model", if stopped { JValue::Str(STOPPED_SENDER.into()) } else { self.model.clone() }),
        ];
        if self.routing_decision.truthy() && !stopped {
            e.push(("routingDecision", self.routing_decision.clone()));
        }
        if let Some(t) = &self.route_target {
            e.push(("routeTarget", t.clone()));
        }
        if !self.reasoning.is_empty() && !stopped {
            e.push(("reasoning", JValue::Str(self.reasoning.clone())));
        }
        if !calls.is_empty() {
            e.push(("toolCalls", JValue::Arr(calls)));
        }
        if let Some(s) = &self.stats {
            e.push(("stats", s.clone()));
        }
        if let Some(s) = self.sources.as_ref().filter(|s| !s.is_empty() && !stopped) {
            e.push(("sources", JValue::Arr(s.clone())));
        }
        if let Some(n) = note {
            e.push(("paused", n));
        }
        Some(obj(e))
    }

    /// `partialEntry()` (#1507): the reply so far, marked interrupted; keeps the thinking when no
    /// answer was written yet. A failed reply, or nothing at all, is `None`.
    pub fn partial_entry(&self) -> Option<JValue> {
        if self.failed {
            return None;
        }
        let base = self.assistant_entry()?;
        let stopped = as_str(get(&base, "model")) == Some(STOPPED_SENDER) && !get(&base, "content").truthy();
        let mut entry = if stopped {
            let mut e = vec![
                ("role", JValue::Str("assistant".into())),
                ("content", JValue::Str(String::new())),
                ("model", get(&base, "model").clone()),
            ];
            if get(&base, "toolCalls").truthy() {
                e.push(("toolCalls", get(&base, "toolCalls").clone()));
            }
            obj(e)
        } else {
            base
        };
        if stopped && !trim(&self.reasoning).is_empty() {
            crate::text::set(&mut entry, "reasoning", JValue::Str(self.reasoning.clone()));
        }
        let calls = matches!(get(&entry, "toolCalls"), JValue::Arr(a) if !a.is_empty());
        if !get(&entry, "content").truthy() && !get(&entry, "reasoning").truthy() && !calls && !get(&entry, "paused").truthy() {
            return None;
        }
        crate::text::set(&mut entry, "interrupted", JValue::Bool(true));
        Some(entry)
    }
}
