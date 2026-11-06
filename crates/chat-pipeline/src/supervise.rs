//! Step supervision between tool rounds (core step-supervision.cjs, features.stepSupervision) and the
//! image probe a turn runs before it sends images to a model (core vision.cjs).
use crate::text::{as_str, get};
use js_json::{wtf16, JValue};
use std::future::Future;
use std::pin::pin;
use std::task::Poll;

/// step-supervision.cjs `VERIFY`.
pub const VERIFY: &str = "Check the preceding tool results against the user request before continuing. Identify missing evidence and uncertainty. Do not assume a failed tool succeeded.";
const CHOICES: [&str; 3] = ["continue", "verify", "escalate"];
const ESCALATE_MIN_SCORE: f64 = 0.6;
const ESCALATE_MARGIN: f64 = 0.3;
const EPSILON: f64 = 1e-9;

/// `confidentEscalation(scores)`.
pub fn confident_escalation(scores: &JValue) -> bool {
    if !matches!(scores, JValue::Obj(_) | JValue::Arr(_)) {
        return false;
    }
    let score = |id: &str| match get(scores, id) {
        JValue::Num(n) if n.is_finite() => Some(*n),
        _ => None,
    };
    let (Some(c), Some(v), Some(e)) = (score("continue"), score("verify"), score("escalate")) else {
        return false;
    };
    let runner_up = c.max(v);
    e >= ESCALATE_MIN_SCORE - EPSILON && e - runner_up >= ESCALATE_MARGIN - EPSILON
}

/// The decision service a supervisor asks, and its deadline.
#[allow(async_fn_in_trait)]
pub trait SuperviseIo {
    /// `enabled() && provider`: whether supervision runs at all.
    fn enabled(&self) -> bool;
    /// The turn's `signal.aborted`.
    fn aborted(&self) -> bool;
    /// `getDeadlineMs()` (or the configured `deadlineMs`).
    fn deadline_ms(&self) -> JValue;
    /// `provider.decide({ round, goal, outputs, choices })`: `Ok(None)` for a falsy result, `Err(cause)` when it threw.
    async fn decide(&self, input: &JValue) -> Result<Option<JValue>, String>;
    /// Completes when `ms` have passed (the deadline) or the turn was aborted.
    async fn deadline(&self, ms: u64);
    /// One `[system-one] supervise` log entry.
    fn log(&self, entry: JValue);
}

/// What `decide` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub action: &'static str,
    /// `existing` (the fallback) or `experimental`.
    pub source: &'static str,
}

const FALLBACK: Decision = Decision { action: "continue", source: "existing" };

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

fn tail_units(text: &str, n: usize) -> String {
    wtf16::slice(text, n)
}

/// Polls `a` and `b` together; the first ready wins (`a` first, as `Promise.race` settles the earlier one).
async fn race<A: Future, B: Future>(a: A, b: B) -> Result<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    std::future::poll_fn(move |cx| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        if let Poll::Ready(v) = b.as_mut().poll(cx) {
            return Poll::Ready(Err(v));
        }
        Poll::Pending
    })
    .await
}

/// `createStepSupervision(...).decide({ round, messages, signal, goal, wrote })`.
pub async fn decide(io: &impl SuperviseIo, round: u32, messages: &[JValue], goal: Option<&str>, wrote: bool) -> Decision {
    if !io.enabled() || io.aborted() || round >= 2 {
        return FALLBACK;
    }
    let mut outputs: Vec<JValue> = messages
        .iter()
        .filter(|m| matches!(as_str(get(m, "role")), Some("tool" | "assistant")))
        .map(|m| {
            let content = as_str(get(m, "content")).map(|c| tail_units(c, 1500)).unwrap_or_default();
            JValue::obj([("role", get(m, "role").clone()), ("content", JValue::Str(content))])
        })
        .collect();
    if outputs.len() > 8 {
        outputs.drain(..outputs.len() - 8);
    }
    let errored = |c: &str| c.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("error"));
    if outputs.iter().any(|m| as_str(get(m, "role")) == Some("tool") && as_str(get(m, "content")).is_some_and(errored)) {
        return FALLBACK;
    }
    let ms = match io.deadline_ms() {
        JValue::Num(n) if n.fract() == 0.0 && (1.0..=2000.0).contains(&n) => n as u64,
        _ => return FALLBACK,
    };
    let request = messages.iter().rev().find(|m| as_str(get(m, "role")) == Some("user"));
    let goal = match goal {
        Some(g) => tail_units(g, 1500),
        None => request.and_then(|r| as_str(get(r, "content"))).map(|c| tail_units(c, 1500)).unwrap_or_default(),
    };
    let input = JValue::obj([
        ("round", JValue::Num(f64::from(round))),
        ("goal", JValue::Str(goal)),
        ("outputs", JValue::Arr(outputs)),
        ("choices", JValue::Arr(CHOICES.iter().map(|c| s(c)).collect())),
    ]);
    let entry = |action: &str, fell: Option<&str>, cause: Option<&str>| {
        let mut e = vec![("round", JValue::Num(f64::from(round))), ("action", s(action)), ("fellBack", fell.map_or(JValue::Null, s))];
        if fell.is_some() {
            e.push(("cause", cause.map_or(JValue::Null, s)));
        }
        JValue::obj(e)
    };
    let result = race(io.decide(&input), io.deadline(ms)).await;
    let result = match result {
        Ok(Ok(r)) => r,
        Ok(Err(cause)) => {
            let cause = if io.aborted() { "aborted".to_owned() } else { cause };
            io.log(entry("continue", Some("error"), Some(&cause)));
            return FALLBACK;
        }
        Err(()) => None,
    };
    let action = result.as_ref().and_then(|r| {
        let JValue::Obj(items) = r else { return None };
        if items.iter().any(|(k, _)| k != "action" && k != "scores") {
            return None;
        }
        as_str(get(r, "action")).and_then(|a| CHOICES.iter().find(|c| **c == a).copied())
    });
    let Some(action) = action.filter(|_| !io.aborted()) else {
        let cause = if io.aborted() {
            "aborted"
        } else if result.as_ref().is_none_or(|r| !r.truthy()) {
            "deadline"
        } else {
            "invalid-result"
        };
        io.log(entry("continue", Some("no-decision"), Some(cause)));
        return FALLBACK;
    };
    if action == "escalate" && !wrote && !confident_escalation(get(result.as_ref().unwrap_or(&JValue::Null), "scores")) {
        io.log(entry("continue", Some("weak-escalate"), None));
        return FALLBACK;
    }
    io.log(entry(action, None, None));
    Decision { action, source: "experimental" }
}

/// `superviseNextStep(supervisor, input)`: the messages for the next round (a `verify` adds the check
/// request) and whether the turn pauses (`escalate`).
pub async fn next_step(io: Option<&impl SuperviseIo>, round: u32, messages: Vec<JValue>, goal: Option<&str>, wrote: bool) -> (Vec<JValue>, bool, Option<Decision>) {
    let Some(io) = io else {
        return (messages, false, None);
    };
    let d = decide(io, round, &messages, goal, wrote).await;
    let mut messages = messages;
    if d.action == "verify" {
        messages.push(JValue::obj([("role", s("user")), ("content", s(VERIFY))]));
    }
    (messages, d.action == "escalate", Some(d))
}

// ── vision.cjs ─────────────────────────────────────────────────────────────

/// One image probe's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub supported: bool,
    pub reason: Option<String>,
}

/// What the probe sends and reads.
#[allow(async_fn_in_trait)]
pub trait ProbeIo {
    fn now_ms(&self) -> f64;
    /// `admit(baseUrl, model)`; `Err(publicMessage)` when it threw.
    async fn admit(&self, base_url: &str, model: &str) -> Option<Result<(), Option<String>>>;
    /// `POST url` with `body`; `Ok((status, body text capped at 64 KiB))`, `Err` when it threw or timed out.
    async fn post(&self, url: &str, headers: &JValue, body: &str) -> Result<(u16, String), ()>;
}

const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// `createVisionProbe()`: answers cached per (base URL, headers, model) for 5 minutes (supported) or
/// 30 seconds (not), at most 128 entries.
#[derive(Default)]
pub struct VisionProbe {
    cache: std::sync::Mutex<Vec<(String, Probe, f64)>>,
}

impl VisionProbe {
    /// `probe(baseUrl, headers, model)`.
    pub async fn probe(&self, io: &impl ProbeIo, base_url: &str, headers: &JValue, model: &str) -> Probe {
        let key = wtf16::stringify(&JValue::Arr(vec![s(base_url), headers.clone(), s(model)])).unwrap_or_default();
        {
            let cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((_, r, until)) = cache.iter().find(|(k, _, _)| *k == key) {
                if *until > io.now_ms() {
                    return r.clone();
                }
            }
        }
        if let Some(Err(message)) = io.admit(base_url, model).await {
            return Probe {
                supported: false,
                reason: Some(message.filter(|m| !m.is_empty()).unwrap_or_else(|| "The model cannot be loaded right now, so the image probe did not run.".into())),
            };
        }
        let mut root = base_url.trim_end_matches('/');
        root = root.strip_suffix("/v1").unwrap_or(root);
        let url = format!("{root}/v1/chat/completions");
        let body = JValue::obj([
            ("model", s(model)),
            ("max_tokens", JValue::Num(1.0)),
            (
                "messages",
                JValue::Arr(vec![JValue::obj([
                    ("role", s("user")),
                    (
                        "content",
                        JValue::Arr(vec![
                            JValue::obj([("type", s("text")), ("text", s("ok"))]),
                            JValue::obj([("type", s("image_url")), ("image_url", JValue::obj([("url", JValue::Str(format!("data:image/png;base64,{PIXEL}")))]))]),
                        ]),
                    ),
                ])]),
            ),
        ]);
        let result = match io.post(&url, headers, &wtf16::stringify(&body).unwrap_or_default()).await {
            Ok((status, _)) if (200..300).contains(&status) => Probe { supported: true, reason: None },
            Ok((status, text)) => {
                let lower = text.to_ascii_lowercase();
                let reason = if lower.contains("mmproj") || lower.contains("projector") {
                    "The inference server needs a multimodal projector (mmproj) configured.".to_owned()
                } else {
                    format!("The inference server rejected the image probe (HTTP {status}). Check model configuration and server availability.")
                };
                Probe { supported: false, reason: Some(reason) }
            }
            Err(()) => Probe {
                supported: false,
                reason: Some("The image probe could not reach the inference server or timed out. Retry when the server is available.".into()),
            },
        };
        let until = io.now_ms() + if result.supported { 300_000.0 } else { 30_000.0 };
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // A Map's set keeps a key's place and takes the new value.
        match cache.iter_mut().find(|(k, _, _)| *k == key) {
            Some(slot) => {
                slot.1 = result.clone();
                slot.2 = until;
            }
            None => cache.push((key, result.clone(), until)),
        }
        if cache.len() > 128 {
            cache.remove(0);
        }
        result
    }
}
