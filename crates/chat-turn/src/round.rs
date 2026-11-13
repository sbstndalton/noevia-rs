//! One model round's stream, read as chat.cjs reads it (lines 2066-2165): each `data:` payload is
//! parsed, and its reasoning, answer text, tool-call fragments, usage and timings become the reply's
//! events and the round's state. The exchange-wide parts (the first output's telemetry, the usage
//! totals across rounds, whether answer text was shown) live in [`Exchange`].
//!
//! Every value is handled as JavaScript handles it: a field is "there" when it is truthy, text is
//! joined with `+` (so a number becomes its string), and a frame whose processing JavaScript would
//! abort with a TypeError (a `null` tool call, say) stops at that point with what it already did.

use js_json::{wtf16, JValue};

use crate::cap::first_delta;
use crate::js;

/// chat.cjs MAX_TOOL_ARGS_UNITS: a name, id or arguments past this is not a real tool call.
pub const MAX_TOOL_ARGS_UNITS: usize = 8 * 1024 * 1024;
pub const LENGTH_WARNING: &str = "The model reached its thinking/answer token budget. This reply may be incomplete; try Low thinking or a narrower question.";

/// What one frame asks of the turn, in the order Node does it.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Send this event.
    Event(JValue),
    /// `recordUsage(workspace, model, reported)`.
    Usage(JValue),
    /// `prefill.recordSample(model, promptTokens, firstTokenMs)`.
    PrefillSample { prompt: f64, first_token_ms: f64 },
    /// The model's draft acceptance, recorded as evidence (local engine only).
    Mtp { drafted: f64, accepted: f64 },
}

/// One tool call as its fragments arrive (chat.cjs `slot`).
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// The id the model sent (any JSON value, kept as sent), or `call-<i>` when it sent none.
    pub id: JValue,
    pub id_made: bool,
    pub name: String,
    pub args: String,
    pub too_large: bool,
    pub name_cut: bool,
}

/// One round's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Round {
    /// Insertion-ordered, keyed by the call's `index` (a JS number, compared as a `Map` does).
    pub tool_calls: Vec<(f64, Slot)>,
    pub saw_anything: bool,
    pub has_content: bool,
    pub content: String,
    pub reasoning: String,
    timings: Option<Vec<(String, JValue)>>,
    pub first_token_ms: Option<f64>,
    pub started_ms: f64,
}

/// What a frame means for the turn beyond its effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// An error frame: the reply ends here (chat.cjs `send error; res.end(); return`).
    EndTurn,
}

/// The exchange-wide state the rounds share.
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    pub model: String,
    pub started_ms: f64,
    pub first_token_ms: Option<f64>,
    /// Answer text was shown to the client in this reply (#1547).
    pub shown_text: bool,
    /// Tool events of earlier rounds (`toolOffset`).
    pub tool_offset: f64,
    /// The local default provider or a machine: draft evidence is recorded (chat.cjs 1902).
    pub local: bool,
    usage: Usage,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Usage {
    prompt: f64,
    completion: f64,
    total: f64,
    has_prompt: bool,
    has_completion: bool,
    has_total: bool,
    predicted_seconds: f64,
    rate_incomplete: bool,
    drafted: f64,
    accepted: f64,
    has_mtp: bool,
}

/// JS `Map` key equality (SameValueZero).
fn same_key(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// `String(x)` of a number, as a template literal writes it.
pub fn number_text(x: f64) -> String {
    js_json::number_to_string(x)
}

/// `String(v)` for the values a provider's fragment can hold (text joined with `+`).
fn text_of(v: &JValue) -> String {
    match v {
        JValue::Str(s) => s.clone(),
        other => js_json::to_js_string(other).unwrap_or_default(),
    }
}

fn reported_count(v: &JValue) -> Option<f64> {
    if matches!(v, JValue::Null | JValue::Undefined) || v.as_str() == Some("") {
        return None;
    }
    let x = js::to_number(v);
    // Number.isSafeInteger(x) && x >= 0
    (x.is_finite() && x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_991.0 && x >= 0.0)
        .then_some(x)
}

fn reported_rate(v: &JValue) -> Option<f64> {
    let x = js::to_number(v);
    (x.is_finite() && x > 0.0).then_some(x)
}

fn seconds(ms: Option<f64>) -> JValue {
    ms.map_or(JValue::Null, |ms| js::n(ms / 1000.0))
}

fn opt(v: Option<f64>) -> JValue {
    v.map_or(JValue::Null, js::n)
}

impl Exchange {
    pub fn new(model: impl Into<String>, started_ms: f64, local: bool) -> Self {
        Exchange {
            model: model.into(),
            started_ms,
            first_token_ms: None,
            shown_text: false,
            tool_offset: 0.0,
            local,
            usage: Usage::default(),
        }
    }

    /// `markFirstOutput`.
    pub fn mark_first_output(&mut self, round: &mut Round, now_ms: f64, out: &mut Vec<Effect>) {
        if round.first_token_ms.is_none() {
            round.first_token_ms = Some((now_ms - round.started_ms).max(0.0));
        }
        if self.first_token_ms.is_some() {
            return;
        }
        let ms = (now_ms - self.started_ms).max(0.0);
        self.first_token_ms = Some(ms);
        out.push(Effect::Event(js::obj([
            ("type", js::s("telemetry")),
            ("phase", js::s("streaming")),
            ("model", js::s(self.model.clone())),
            ("timeToFirstToken", js::n(ms / 1000.0)),
        ])));
    }

    /// The exchange's first-token time in seconds, `null` while none came (the closing telemetry's value).
    pub fn time_to_first_token(&self) -> JValue {
        seconds(self.first_token_ms)
    }

    /// `reportRoundUsage(rawUsage, timings)`.
    pub fn report_usage(
        &mut self,
        round: &Round,
        raw: &JValue,
        timings: &JValue,
        out: &mut Vec<Effect>,
    ) {
        if !raw.is_object() {
            return;
        }
        let prompt = reported_count(raw.get("prompt_tokens"));
        let completion = reported_count(raw.get("completion_tokens"));
        let total = reported_count(raw.get("total_tokens"));
        let rate = reported_rate(timings.get("predicted_per_second"));
        let u = &mut self.usage;
        if let Some(p) = prompt {
            u.prompt += p;
            u.has_prompt = true;
        }
        if let Some(c) = completion {
            u.completion += c;
            u.has_completion = true;
            match rate {
                Some(r) if c > 0.0 => u.predicted_seconds += c / r,
                _ if c > 0.0 => u.rate_incomplete = true,
                _ => {}
            }
        }
        match (total, prompt, completion) {
            (Some(t), _, _) => {
                u.total += t;
                u.has_total = true;
            }
            (None, Some(p), Some(c)) => {
                u.total += p + c;
                u.has_total = true;
            }
            _ => {}
        }
        let drafted = reported_count(timings.get("draft_n"));
        let accepted = reported_count(timings.get("draft_n_accepted"));
        let mtp = match (drafted, accepted) {
            (Some(d), Some(a)) if d > 0.0 && a <= d => Some((d, a)),
            _ => None,
        };
        if let Some((d, a)) = mtp {
            u.drafted += d;
            u.accepted += a;
            u.has_mtp = true;
        }
        let aggregate = (u.has_completion
            && u.completion > 0.0
            && !u.rate_incomplete
            && u.predicted_seconds > 0.0)
            .then(|| u.completion / u.predicted_seconds);
        if prompt.is_some_and(|p| p > 0.0) || completion.is_some_and(|c| c > 0.0) {
            out.push(Effect::Usage(js::obj([
                ("promptTokens", opt(prompt)),
                ("completionTokens", opt(completion)),
                ("totalTokens", opt(total)),
                ("tokensPerSecond", opt(rate)),
            ])));
        }
        if let (true, Some((d, a))) = (self.local, mtp) {
            out.push(Effect::Mtp {
                drafted: d,
                accepted: a,
            });
        }
        if let (Some(ms), Some(p)) = (round.first_token_ms, prompt) {
            if ms > 0.0 && p > 0.0 {
                out.push(Effect::PrefillSample {
                    prompt: p,
                    first_token_ms: ms,
                });
            }
        }
        let u = &self.usage;
        out.push(Effect::Event(js::obj([
            ("type", js::s("usage")),
            ("phase", js::s("streaming")),
            ("model", js::s(self.model.clone())),
            (
                "promptTokens",
                if u.has_prompt {
                    js::n(u.prompt)
                } else {
                    JValue::Null
                },
            ),
            (
                "completionTokens",
                if u.has_completion {
                    js::n(u.completion)
                } else {
                    JValue::Null
                },
            ),
            (
                "totalTokens",
                if u.has_total {
                    js::n(u.total)
                } else {
                    JValue::Null
                },
            ),
            ("tokensPerSecond", opt(aggregate)),
            ("timeToFirstToken", seconds(self.first_token_ms)),
            (
                "drafted",
                if u.has_mtp {
                    js::n(u.drafted)
                } else {
                    JValue::Null
                },
            ),
            (
                "accepted",
                if u.has_mtp {
                    js::n(u.accepted)
                } else {
                    JValue::Null
                },
            ),
        ])));
    }

    /// One `data:` payload of the round. `clean_args` is search-query.cjs `cleanSearchArgs`.
    pub fn frame(
        &mut self,
        round: &mut Round,
        payload: &str,
        now_ms: f64,
        clean_args: &dyn Fn(&str, &str) -> String,
        error_text: &dyn Fn(&JValue, bool) -> String,
        out: &mut Vec<Effect>,
    ) -> Flow {
        // JSON.parse failing: a keep-alive or a partial line, skipped.
        let Ok(evt) = wtf16::parse(payload) else {
            return Flow::Continue;
        };
        // `evt.choices` on null throws; any other non-object reads as having nothing.
        if !evt.is_object() {
            return Flow::Continue;
        }
        let choice = match evt.get("choices") {
            JValue::Arr(c) => c.first().unwrap_or(&JValue::Undefined),
            JValue::Obj(_) => evt.get("choices").get("0"),
            _ => &JValue::Undefined,
        };
        if choice.get("finish_reason").as_str() == Some("length") {
            out.push(Effect::Event(js::obj([
                ("type", js::s("warning")),
                ("text", js::s(LENGTH_WARNING)),
            ])));
        }
        if evt.get("error").truthy() {
            let text = error_text(evt.get("error"), self.shown_text);
            out.push(Effect::Event(js::obj([
                ("type", js::s("error")),
                ("text", js::s(text)),
            ])));
            return Flow::EndTurn;
        }
        let found = first_delta(&evt);
        let delta = if found.truthy() {
            found.clone()
        } else {
            js::obj(Vec::<(String, JValue)>::new())
        };
        let fragments = match delta.get("tool_calls") {
            JValue::Arr(list) => Some(list.clone()),
            _ => None,
        };
        let meaningful = fragments.as_ref().is_some_and(|list| {
            list.iter().any(|tc| {
                tc.get("id").truthy()
                    || tc.get("function").get("name").truthy()
                    || tc.get("function").get("arguments").truthy()
            })
        });
        if delta.get("content").truthy()
            || delta.get("reasoning_content").truthy()
            || delta.get("reasoning").truthy()
            || meaningful
        {
            self.mark_first_output(round, now_ms, out);
        }
        if let JValue::Obj(t) = evt.get("timings") {
            let merged = round.timings.get_or_insert_with(Vec::new);
            for (k, v) in t {
                wtf16::set(merged, k, v.clone());
            }
        } else if let JValue::Arr(items) = evt.get("timings") {
            // `{ ...timings, ...array }` spreads its indexes as keys.
            let merged = round.timings.get_or_insert_with(Vec::new);
            for (i, v) in items.iter().enumerate() {
                wtf16::set(merged, &i.to_string(), v.clone());
            }
        }
        if evt.get("usage").truthy() {
            let timings = round.timings.clone().map_or(JValue::Undefined, JValue::Obj);
            let usage = evt.get("usage").clone();
            self.report_usage(round, &usage, &timings, out);
        }
        for key in ["reasoning_content", "reasoning"] {
            let v = delta.get(key);
            if v.truthy() {
                round.saw_anything = true;
                js::concat(&mut round.reasoning, &text_of(v));
                out.push(Effect::Event(js::obj([
                    ("type", js::s("reasoning")),
                    ("text", v.clone()),
                ])));
            }
        }
        let content = delta.get("content");
        if content.truthy() {
            round.saw_anything = true;
            round.has_content = true;
            self.shown_text = true;
            js::concat(&mut round.content, &text_of(content));
            out.push(Effect::Event(js::obj([
                ("type", js::s("delta")),
                ("text", content.clone()),
            ])));
        }
        if let Some(list) = fragments {
            round.saw_anything = true;
            for tc in &list {
                // `tc.index` of null or undefined throws: the rest of this frame is dropped.
                if matches!(tc, JValue::Null | JValue::Undefined) {
                    return Flow::Continue;
                }
                self.fragment(round, tc, clean_args, out);
            }
        }
        Flow::Continue
    }

    fn fragment(
        &mut self,
        round: &mut Round,
        tc: &JValue,
        clean_args: &dyn Fn(&str, &str) -> String,
        out: &mut Vec<Effect>,
    ) {
        let i = match tc.get("index") {
            JValue::Num(x) => *x,
            _ => 0.0,
        };
        let at = round.tool_calls.iter().position(|(k, _)| same_key(*k, i));
        let mut slot = match at {
            Some(p) => round.tool_calls.get(p).map(|(_, s)| s.clone()),
            None => None,
        }
        .unwrap_or_else(|| Slot {
            id: js::s(format!("call-{}", number_text(i))),
            id_made: true,
            name: String::new(),
            args: String::new(),
            too_large: false,
            name_cut: false,
        });
        let id = tc.get("id");
        if id.truthy() {
            if js::len(&text_of(id)) > MAX_TOOL_ARGS_UNITS {
                slot.too_large = true;
                slot.args.clear();
            } else {
                slot.id = id.clone();
                slot.id_made = false;
            }
        }
        let name = tc.get("function").get("name");
        if name.truthy() && !slot.name_cut {
            let piece = text_of(name);
            if js::len(&slot.name) + js::len(&piece) > MAX_TOOL_ARGS_UNITS {
                slot.too_large = true;
                slot.name_cut = true;
                slot.args.clear();
                slot.name = js::slice(&slot.name, 64);
            } else {
                js::concat(&mut slot.name, &piece);
            }
        }
        let args = tc.get("function").get("arguments");
        if args.truthy() && !slot.too_large {
            let piece = text_of(args);
            if js::len(&slot.args) + js::len(&piece) > MAX_TOOL_ARGS_UNITS {
                slot.too_large = true;
                slot.args.clear();
            } else {
                js::concat(&mut slot.args, &piece);
            }
        }
        let event = js::obj([
            ("type", js::s("tool")),
            ("index", js::n(self.tool_offset + i)),
            ("name", js::s(slot.name.clone())),
            ("args", js::s(clean_args(&slot.name, &slot.args))),
        ]);
        match at {
            Some(p) => {
                if let Some(entry) = round.tool_calls.get_mut(p) {
                    entry.1 = slot;
                }
            }
            None => round.tool_calls.push((i, slot)),
        }
        out.push(Effect::Event(event));
    }
}

impl Round {
    pub fn new(started_ms: f64) -> Self {
        Round {
            started_ms,
            ..Round::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(payloads: &[&str]) -> (Exchange, Round, Vec<Effect>) {
        let mut ex = Exchange::new("qa-model", 0.0, true);
        let mut round = Round::new(0.0);
        let mut out = Vec::new();
        for p in payloads {
            ex.frame(
                &mut round,
                p,
                0.0,
                &|_, a| a.to_string(),
                &|_, _| "QA error".into(),
                &mut out,
            );
        }
        (ex, round, out)
    }

    fn events(out: &[Effect]) -> Vec<String> {
        out.iter()
            .filter_map(|e| match e {
                Effect::Event(v) => Some(js::stringify(v)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_reasoning_and_the_first_output_telemetry() {
        let (ex, round, out) = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"think "}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"content":5}}]}"#,
        ]);
        assert_eq!(
            events(&out),
            [
                r#"{"type":"telemetry","phase":"streaming","model":"qa-model","timeToFirstToken":0}"#,
                r#"{"type":"reasoning","text":"think "}"#,
                r#"{"type":"delta","text":"Hi"}"#,
                r#"{"type":"delta","text":5}"#,
            ]
        );
        assert_eq!(round.content, "Hi5");
        assert!(round.has_content && ex.shown_text);
    }

    #[test]
    fn tool_fragments_build_one_slot_per_index() {
        let (_, round, out) = run(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"drive_"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{\"a\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}},null,{"index":1,"id":"x"}]}}]}"#,
        ]);
        assert_eq!(
            round.tool_calls.len(),
            1,
            "the null fragment stops the frame"
        );
        let slot = &round.tool_calls[0].1;
        assert_eq!(
            (slot.name.as_str(), slot.args.as_str()),
            ("drive_read", "{\"a\":1}")
        );
        assert_eq!(slot.id, js::s("call-0"));
        assert_eq!(
            events(&out).last().unwrap(),
            r#"{"type":"tool","index":0,"name":"drive_read","args":"{\"a\":1}"}"#
        );
    }

    #[test]
    fn an_error_frame_ends_the_turn_and_usage_adds_up() {
        let mut ex = Exchange::new("m", 0.0, false);
        let mut round = Round::new(0.0);
        let mut out = Vec::new();
        let f = ex.frame(&mut round, r#"{"choices":[],"usage":{"prompt_tokens":"10","completion_tokens":2},"timings":{"predicted_per_second":4}}"#, 0.0, &|_, a| a.into(), &|_, _| String::new(), &mut out);
        assert_eq!(f, Flow::Continue);
        assert!(out.iter().any(|e| matches!(e, Effect::Usage(_))));
        assert!(events(&out)[0].contains(
            r#""promptTokens":10,"completionTokens":2,"totalTokens":12,"tokensPerSecond":4"#
        ));
        let f = ex.frame(
            &mut round,
            r#"{"error":{"message":"x"}}"#,
            0.0,
            &|_, a| a.into(),
            &|_, partial| format!("failed partial={partial}"),
            &mut out,
        );
        assert_eq!(f, Flow::EndTurn);
        assert_eq!(
            events(&out).last().unwrap(),
            r#"{"type":"error","text":"failed partial=false"}"#
        );
    }
}
