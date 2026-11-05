//! What a turn reports of its engine's usage (core chat.cjs `markFirstOutput`, `reportRoundUsage`):
//! the exchange's cumulative token counts and rate across tool rounds, the `telemetry` and `usage`
//! events, and the records a round hands on (usage per account, speculative-decoding evidence, the
//! prefill sample). Times are milliseconds from the caller's clock.
use crate::text::{get, js_number};
use js_json::JValue;

#[derive(Default, Clone, Copy)]
struct Totals {
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

/// The records one usage report hands on.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `send(...)`: the `usage` event.
    pub event: JValue,
    /// `recordUsage(workspace, model, reported)` when the round counted tokens.
    pub record_usage: Option<JValue>,
    /// `modelManager.recordEvidence(model, {category: 'mtp_acceptance', ... value})` (local provider only).
    pub mtp_value: Option<JValue>,
    /// `prefill.recordSample(model, prompt, roundFirstTokenMs)`.
    pub prefill_sample: Option<(f64, f64)>,
}

/// One exchange (a visible reply, possibly several engine requests).
pub struct Exchange {
    model: JValue,
    started_at: f64,
    round_started_at: f64,
    round_first_token_ms: Option<f64>,
    first_token_ms: Option<f64>,
    totals: Totals,
}

fn count(v: &JValue) -> Option<f64> {
    if matches!(v, JValue::Null | JValue::Undefined) || v.as_str() == Some("") {
        return None;
    }
    let n = js_number(v);
    (n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && n >= 0.0).then_some(n)
}

fn rate(v: &JValue) -> Option<f64> {
    let n = js_number(v);
    (n.is_finite() && n > 0.0).then_some(n)
}

fn num(v: Option<f64>) -> JValue {
    v.map_or(JValue::Null, JValue::Num)
}

/// `Math.round(x)`.
fn js_round(x: f64) -> f64 {
    let r = x.floor();
    if x - r >= 0.5 {
        r + 1.0
    } else {
        r
    }
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

impl Exchange {
    /// `exchangeStartedAt` is `started_at`; `model` is the turn's model as the events name it.
    pub fn new(model: JValue, started_at: f64) -> Self {
        Self { model, started_at, round_started_at: 0.0, round_first_token_ms: None, first_token_ms: None, totals: Totals::default() }
    }

    /// A round's engine request starts (`roundStartedAt = Date.now(); roundFirstTokenMs = null`).
    pub fn round(&mut self, now: f64) {
        self.round_started_at = now;
        self.round_first_token_ms = None;
    }

    /// The exchange's time to first token, in ms, once seen.
    pub fn first_token_ms(&self) -> Option<f64> {
        self.first_token_ms
    }

    /// `markFirstOutput()`: the `telemetry` event the first output of the exchange sends.
    pub fn first_output(&mut self, now: f64) -> Option<JValue> {
        if self.round_first_token_ms.is_none() {
            self.round_first_token_ms = Some((now - self.round_started_at).max(0.0));
        }
        if self.first_token_ms.is_some() {
            return None;
        }
        let ms = (now - self.started_at).max(0.0);
        self.first_token_ms = Some(ms);
        Some(obj(vec![
            ("type", JValue::Str("telemetry".into())),
            ("phase", JValue::Str("streaming".into())),
            ("model", self.model.clone()),
            ("timeToFirstToken", JValue::Num(ms / 1000.0)),
        ]))
    }

    /// `reportRoundUsage(rawUsage, timings)`; `None` when the usage is not an object.
    pub fn report(&mut self, raw: &JValue, timings: &JValue) -> Option<Report> {
        if !matches!(raw, JValue::Obj(_) | JValue::Arr(_)) {
            return None;
        }
        let prompt = count(get(raw, "prompt_tokens"));
        let completion = count(get(raw, "completion_tokens"));
        let total = count(get(raw, "total_tokens"));
        let r = rate(get(timings, "predicted_per_second"));
        let t = &mut self.totals;
        if let Some(p) = prompt {
            t.prompt += p;
            t.has_prompt = true;
        }
        if let Some(c) = completion {
            t.completion += c;
            t.has_completion = true;
            match r {
                Some(rate) if c > 0.0 => t.predicted_seconds += c / rate,
                _ if c > 0.0 => t.rate_incomplete = true,
                _ => {}
            }
        }
        if let Some(x) = total {
            t.total += x;
            t.has_total = true;
        } else if let (Some(p), Some(c)) = (prompt, completion) {
            t.total += p + c;
            t.has_total = true;
        }
        let drafted = count(get(timings, "draft_n"));
        let accepted = count(get(timings, "draft_n_accepted"));
        let mtp = match (drafted, accepted) {
            (Some(d), Some(a)) if d > 0.0 && a <= d => Some((d, a)),
            _ => None,
        };
        if let Some((d, a)) = mtp {
            t.drafted += d;
            t.accepted += a;
            t.has_mtp = true;
        }
        let aggregate = (t.has_completion && t.completion > 0.0 && !t.rate_incomplete && t.predicted_seconds > 0.0)
            .then(|| t.completion / t.predicted_seconds);
        let reported = obj(vec![
            ("promptTokens", num(prompt)),
            ("completionTokens", num(completion)),
            ("totalTokens", num(total)),
            ("tokensPerSecond", num(r)),
        ]);
        let counted = prompt.is_some_and(|p| p > 0.0) || completion.is_some_and(|c| c > 0.0);
        let mtp_value = mtp.map(|(d, a)| {
            obj(vec![
                ("rate", JValue::Num(js_round((a / d) * 100.0) / 100.0)),
                ("drafted", JValue::Num(d)),
                ("accepted", JValue::Num(a)),
            ])
        });
        let prefill_sample = match (self.round_first_token_ms, prompt) {
            (Some(ms), Some(p)) if ms > 0.0 && p > 0.0 => Some((p, ms)),
            _ => None,
        };
        let t = self.totals;
        let event = obj(vec![
            ("type", JValue::Str("usage".into())),
            ("phase", JValue::Str("streaming".into())),
            ("model", self.model.clone()),
            ("promptTokens", if t.has_prompt { JValue::Num(t.prompt) } else { JValue::Null }),
            ("completionTokens", if t.has_completion { JValue::Num(t.completion) } else { JValue::Null }),
            ("totalTokens", if t.has_total { JValue::Num(t.total) } else { JValue::Null }),
            ("tokensPerSecond", num(aggregate)),
            ("timeToFirstToken", self.first_token_ms.map_or(JValue::Null, |ms| JValue::Num(ms / 1000.0))),
            ("drafted", if t.has_mtp { JValue::Num(t.drafted) } else { JValue::Null }),
            ("accepted", if t.has_mtp { JValue::Num(t.accepted) } else { JValue::Null }),
        ]);
        Some(Report { event, record_usage: counted.then_some(reported), mtp_value, prefill_sample })
    }
}
