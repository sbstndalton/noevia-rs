//! Why did a model fail to load or serve during auto-tune? (sbstndalton/noevia#1004)
//!
//! Auto-tune stays a script: [`autotune-plan`] picks every step, and this crate only names the
//! outcome of a failed step in the planner's own words. When a fill-and-recall step fails, the
//! calibrator already knows *that* it failed and gives a cause. Some causes are measurements
//! (available memory fell below the floor, the prompt ran over the time limit, the marker was not
//! recalled) and are final. Others are guesses from how the failure looked: the engine refused
//! the load, the router marked the model failed, the stream reported an error, or the server went
//! away. For those the caller passes the **evidence** (HTTP status, the router's exit code, the
//! engine's error text) and [`verdict`] does three things, always the same way for the same input:
//!
//! 1. **Rules first.** A fixed, ordered list of patterns ([`classify`]) names the failure: out of
//!    memory, a chat template the engine cannot use, a context the engine will not serve, a time
//!    out, or a model file the engine cannot load. The first matching rule wins; out of memory is
//!    checked first because a refused load often says both ("failed to load model: failed to
//!    allocate buffer"). No match is [`Label::Unknown`].
//! 2. **Advice only on a tie.** Only when the rules say `unknown` may an advisory label from the
//!    decision service (Laya, which reads the same text) decide, and only at a confidence of at
//!    least [`MIN_ADVICE_PERMILLE`], and never `timeout` (noevia#1046): a time out re-runs the
//!    same setting, so only a rule may say it. A rule verdict is never overridden. The reply says
//!    [`Source::Advisor`] when the advice decided and records the advice either way.
//! 3. **Otherwise the calibrator's cause stands** ([`Source::Fallback`]), exactly what auto-tune
//!    did before this crate existed.
//!
//! `ask` in the reply tells the caller whether advice could change anything (the rules found
//! nothing, there is text to read, and no advice was given yet), so the decision service is asked
//! at most once per failed step and never when a rule or a measurement already decided.
//!
//! The reply's `reason` is one of a fixed set of sentences; nothing from the input (the engine's
//! text, the advice) is ever echoed. Input is untrusted JSON (see [`verdict_json`]); anything
//! malformed or over a cap is refused with a fixed error code. Nothing here panics.
//!
//! [`autotune-plan`]: ../autotune_plan/index.html

use serde_json::{json, Map, Value};

/// Requests longer than this are refused ([`VerdictError::TooLarge`]).
pub const MAX_INPUT_BYTES: usize = 32 * 1024;
/// The engine's text at most this many UTF-8 bytes (callers send a short excerpt).
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Advice at a lower confidence than this (in thousandths) is recorded but never used.
pub const MIN_ADVICE_PERMILLE: u16 = 600;

/// A step's outcome, in autotune-plan's names (`timeout` and `template` are noevia-core's: a load
/// that did not finish in time is retried once, then stops the run; so does a template error,
/// which no smaller context or more compact cache type can fix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    Oom,
    LoadFailed,
    Timeout,
    OverTime,
    RecallFailed,
    Template,
}

impl Outcome {
    pub const ALL: [Outcome; 6] = [
        Outcome::Oom,
        Outcome::LoadFailed,
        Outcome::Timeout,
        Outcome::OverTime,
        Outcome::RecallFailed,
        Outcome::Template,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Oom => "oom",
            Outcome::LoadFailed => "load_failed",
            Outcome::Timeout => "timeout",
            Outcome::OverTime => "over_time",
            Outcome::RecallFailed => "recall_failed",
            Outcome::Template => "template",
        }
    }
    fn sentence(self) -> &'static str {
        match self {
            Outcome::Oom => "The engine ran out of memory at this setting.",
            Outcome::LoadFailed => "The engine could not load the model at this setting.",
            Outcome::Timeout => "The engine did not finish in time at this setting.",
            Outcome::OverTime => "Filling the context took longer than the prompt time limit.",
            Outcome::RecallFailed => "The engine could not serve the full context at this setting.",
            Outcome::Template => "The engine could not use the model's chat template.",
        }
    }
}

/// The calibrator's cause for a failed step (llamacpp-calibration.cjs `finish`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Oom,
    Load,
    Timeout,
    Time,
    Recall,
}

impl Cause {
    pub const ALL: [Cause; 5] = [
        Cause::Oom,
        Cause::Load,
        Cause::Timeout,
        Cause::Time,
        Cause::Recall,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Cause::Oom => "oom",
            Cause::Load => "load",
            Cause::Timeout => "timeout",
            Cause::Time => "time",
            Cause::Recall => "recall",
        }
    }
    fn from_name(s: &str) -> Option<Self> {
        Cause::ALL.into_iter().find(|c| c.name() == s)
    }
    /// What auto-tune made of this cause before #1004 (PLAN_CAUSE in llamacpp-full-autotune.cjs).
    pub fn outcome(self) -> Outcome {
        match self {
            Cause::Oom => Outcome::Oom,
            Cause::Load => Outcome::LoadFailed,
            Cause::Timeout => Outcome::Timeout,
            Cause::Time => Outcome::OverTime,
            Cause::Recall => Outcome::RecallFailed,
        }
    }
}

/// A rule's (or the advisor's) reading of the evidence. Never `over_time`: only a measurement
/// says that.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Label {
    Oom,
    LoadFailed,
    Timeout,
    RecallFailed,
    Template,
    Unknown,
}

impl Label {
    pub const ALL: [Label; 6] = [
        Label::Oom,
        Label::LoadFailed,
        Label::Timeout,
        Label::RecallFailed,
        Label::Template,
        Label::Unknown,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Label::Oom => "oom",
            Label::LoadFailed => "load_failed",
            Label::Timeout => "timeout",
            Label::RecallFailed => "recall_failed",
            Label::Template => "template",
            Label::Unknown => "unknown",
        }
    }
    pub fn from_name(s: &str) -> Option<Self> {
        Label::ALL.into_iter().find(|l| l.name() == s)
    }
    pub fn outcome(self) -> Option<Outcome> {
        Some(match self {
            Label::Oom => Outcome::Oom,
            Label::LoadFailed => Outcome::LoadFailed,
            Label::Timeout => Outcome::Timeout,
            Label::RecallFailed => Outcome::RecallFailed,
            Label::Template => Outcome::Template,
            Label::Unknown => return None,
        })
    }
}

/// Who decided the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// No evidence was given: the calibrator measured it (memory floor, time, recall).
    Measured,
    /// A fixed rule matched the evidence.
    Rule,
    /// No rule matched; the decision service's advice decided.
    Advisor,
    /// No rule matched and no usable advice: the calibrator's own cause.
    Fallback,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Measured => "measured",
            Source::Rule => "rule",
            Source::Advisor => "advisor",
            Source::Fallback => "fallback",
        }
    }
    fn suffix(self) -> &'static str {
        match self {
            Source::Measured => "",
            Source::Rule => " (from the engine's error)",
            Source::Advisor => {
                " (no rule matched the engine's error; the decision service's reading was used)"
            }
            Source::Fallback => " (no rule matched the engine's error)",
        }
    }
}

/// How a guessed failure looked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evidence {
    /// The engine's HTTP status, when one came back.
    pub status: Option<u16>,
    /// The router's exit code for the model's process, when it reported one.
    pub exit_code: Option<i32>,
    /// The engine's error text (an excerpt), possibly empty.
    pub text: String,
    /// The engine went away during the step (the calibrator's crash path, noevia#1046). A crash is
    /// never read as a time out, whatever its text says: the time-out rules are skipped.
    pub crash: bool,
}

/// The decision service's label for the same evidence, confidence in thousandths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advice {
    pub label: Label,
    pub permille: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub cause: Cause,
    pub evidence: Option<Evidence>,
    pub advice: Option<Advice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub outcome: Outcome,
    pub source: Source,
    /// The rules' label (`unknown` when nothing matched or no evidence was given).
    pub rule: Label,
    /// The matching rule's id.
    pub rule_id: Option<&'static str>,
    /// Whether asking the decision service could change the outcome.
    pub ask: bool,
    pub advice: Option<Advice>,
    pub advice_used: bool,
}

impl Verdict {
    /// A fixed sentence: the outcome, then who decided it.
    pub fn reason(&self) -> String {
        format!(
            "{}{}",
            self.outcome.sentence().trim_end_matches('.'),
            self.source.suffix()
        ) + "."
    }
}

/// One pattern rule: any needle matches (in the normalized text).
struct Rule {
    id: &'static str,
    label: Label,
    needles: &'static [&'static str],
}

/// Ordered: out of memory first (a refused load often names both), then the template, the
/// context, a time out, and last a model the engine cannot load at all.
const RULES: &[Rule] = &[
    Rule {
        id: "text_oom",
        label: Label::Oom,
        needles: &[
            "out of memory",
            "outofdevicememory",
            "outofhostmemory",
            "out of device memory",
            "out of host memory",
            "failed to allocate",
            "unable to allocate",
            "cannot allocate",
            "could not allocate",
            "memory allocation failed",
            "bad alloc",
            "insufficient memory",
            "not enough memory",
            "oom kill",
            "cudamalloc failed",
        ],
    },
    Rule {
        id: "text_template",
        label: Label::Template,
        needles: &[
            "chat template",
            "jinja",
            "failed to apply template",
            "failed to parse template",
            "template error",
            "unsupported template",
        ],
    },
    Rule {
        id: "text_context",
        label: Label::RecallFailed,
        needles: &[
            "exceeds the available context size",
            "exceed context size",
            "exceeds context size",
            "context size exceeded",
            "context shift is disabled",
            "prompt is too long",
            "input is too large",
        ],
    },
    Rule {
        id: "text_timeout",
        label: Label::Timeout,
        needles: &["timed out", "timeout", "deadline exceeded"],
    },
    Rule {
        id: "text_load",
        label: Label::LoadFailed,
        needles: &[
            "failed to load model",
            "error loading model",
            "unable to load model",
            "invalid magic",
            "unknown model architecture",
            "unsupported model architecture",
            "missing tensor",
            "wrong number of tensors",
            "unexpectedly reached end of file",
            "gguf init",
            "invalid gguf",
            "no such file",
        ],
    },
];

/// Lowercases ASCII, reads `_` and `-` as spaces and collapses whitespace, so
/// `ErrorOutOfDeviceMemory`, `out_of_memory` and `Out-Of-Memory` read alike.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for c in text.chars() {
        let c = match c {
            '_' | '-' => ' ',
            c if c.is_whitespace() => ' ',
            c => c.to_ascii_lowercase(),
        };
        if c == ' ' {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// The rules' label for the evidence and the matching rule's id.
pub fn classify(e: &Evidence) -> (Label, Option<&'static str>) {
    // 137 = 128 + SIGKILL: the kernel's OOM killer (or a cgroup limit) ended the process.
    if matches!(e.exit_code, Some(137) | Some(-9)) {
        return (Label::Oom, Some("exit_killed"));
    }
    let text = normalize(&e.text);
    for rule in RULES {
        if e.crash && rule.label == Label::Timeout {
            continue;
        }
        if rule.needles.iter().any(|n| text.contains(n)) {
            return (rule.label, Some(rule.id));
        }
    }
    if !e.crash && matches!(e.status, Some(408) | Some(504)) {
        return (Label::Timeout, Some("status_timeout"));
    }
    (Label::Unknown, None)
}

/// The step's outcome from the calibrator's cause, the evidence and any advice.
pub fn verdict(r: &Request) -> Verdict {
    let fallback = r.cause.outcome();
    let Some(e) = &r.evidence else {
        return Verdict {
            outcome: fallback,
            source: Source::Measured,
            rule: Label::Unknown,
            rule_id: None,
            ask: false,
            advice: r.advice,
            advice_used: false,
        };
    };
    let (rule, rule_id) = classify(e);
    if let Some(outcome) = rule.outcome() {
        return Verdict {
            outcome,
            source: Source::Rule,
            rule,
            rule_id,
            ask: false,
            advice: r.advice,
            advice_used: false,
        };
    }
    let usable = r
        .advice
        .filter(|a| a.permille >= MIN_ADVICE_PERMILLE && a.label != Label::Timeout)
        .and_then(|a| a.label.outcome());
    match usable {
        Some(outcome) => Verdict {
            outcome,
            source: Source::Advisor,
            rule,
            rule_id,
            ask: false,
            advice: r.advice,
            advice_used: true,
        },
        None => Verdict {
            outcome: fallback,
            source: Source::Fallback,
            rule,
            rule_id,
            ask: r.advice.is_none() && !e.text.trim().is_empty(),
            advice: r.advice,
            advice_used: false,
        },
    }
}

/// Why a request was refused. The code is all the caller sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerdictError {
    Input,
    TooLarge,
}

impl VerdictError {
    pub fn code(self) -> &'static str {
        match self {
            VerdictError::Input => "input",
            VerdictError::TooLarge => "too_large",
        }
    }
}

fn only_keys(o: &Map<String, Value>, keys: &[&str]) -> Result<(), VerdictError> {
    if o.keys().all(|k| keys.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(VerdictError::Input)
    }
}

fn int_field(
    o: &Map<String, Value>,
    key: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, VerdictError> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_i64() {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(VerdictError::Input),
        },
    }
}

/// Parses a request:
///
/// ```json
/// {"cause":"load",
///  "evidence":{"status":500,"exitCode":null,"text":"…","crash":false} | null,
///  "advice":{"label":"oom","confidence":0.82} | null}
/// ```
///
/// `cause` is the calibrator's (`oom`, `load`, `timeout`, `time`, `recall`); `evidence` is given
/// only for a guessed cause; `confidence` is a number from 0 to 1. Unknown keys are refused.
pub fn parse(text: &str) -> Result<Request, VerdictError> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(VerdictError::TooLarge);
    }
    let v: Value = serde_json::from_str(text).map_err(|_| VerdictError::Input)?;
    let o = v.as_object().ok_or(VerdictError::Input)?;
    only_keys(o, &["cause", "evidence", "advice"])?;
    let cause = o
        .get("cause")
        .and_then(Value::as_str)
        .and_then(Cause::from_name)
        .ok_or(VerdictError::Input)?;
    let evidence = match o.get("evidence") {
        None | Some(Value::Null) => None,
        Some(Value::Object(e)) => {
            only_keys(e, &["status", "exitCode", "text", "crash"])?;
            let crash = match e.get("crash") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(VerdictError::Input),
            };
            let status = int_field(e, "status", 0, 999)?.map(|n| n as u16);
            let exit_code = int_field(e, "exitCode", -1024, 1024)?.map(|n| n as i32);
            let text = match e.get("text") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) if s.len() > MAX_TEXT_BYTES => {
                    return Err(VerdictError::TooLarge)
                }
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(VerdictError::Input),
            };
            Some(Evidence {
                status,
                exit_code,
                text,
                crash,
            })
        }
        Some(_) => return Err(VerdictError::Input),
    };
    let advice = match o.get("advice") {
        None | Some(Value::Null) => None,
        Some(Value::Object(a)) => {
            only_keys(a, &["label", "confidence"])?;
            let label = a
                .get("label")
                .and_then(Value::as_str)
                .and_then(Label::from_name)
                .ok_or(VerdictError::Input)?;
            let c = a
                .get("confidence")
                .and_then(Value::as_f64)
                .filter(|c| c.is_finite() && (0.0..=1.0).contains(c))
                .ok_or(VerdictError::Input)?;
            // 0 ≤ c ≤ 1, so the rounded product is 0..=1000.
            let permille = (c * 1000.0).round() as u16;
            Some(Advice { label, permille })
        }
        Some(_) => return Err(VerdictError::Input),
    };
    Ok(Request {
        cause,
        evidence,
        advice,
    })
}

/// The reply: `{"outcome","source","rule","ruleId","ask","advice","adviceUsed","reason"}`, where
/// `advice` is `{"label","permille"}` or null.
pub fn to_json(v: &Verdict) -> String {
    let advice = match v.advice {
        Some(a) => json!({"label": a.label.name(), "permille": a.permille}),
        None => Value::Null,
    };
    json!({
        "outcome": v.outcome.name(),
        "source": v.source.name(),
        "rule": v.rule.name(),
        "ruleId": v.rule_id,
        "ask": v.ask,
        "advice": advice,
        "adviceUsed": v.advice_used,
        "reason": v.reason(),
    })
    .to_string()
}

/// The JSON entry point (dav-parse.wasm's `load_verdict`): status 0 with the reply, or 1 with
/// `{"error":"input"|"too_large"}`.
pub fn verdict_json(text: &str) -> (u32, String) {
    match parse(text) {
        Ok(r) => (0, to_json(&verdict(&r))),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn ev(text: &str) -> Evidence {
        Evidence {
            text: text.to_owned(),
            ..Evidence::default()
        }
    }

    fn label(text: &str) -> Label {
        classify(&ev(text)).0
    }

    #[test]
    fn rules_name_engine_failures() {
        for t in [
            "ggml_vulkan: Device memory allocation of size 4294967296 failed. vk::Device::allocateMemory: ErrorOutOfDeviceMemory",
            "CUDA error: out of memory",
            "llama_init_from_model: failed to allocate compute buffers",
            "terminate called after throwing an instance of 'std::bad_alloc'",
            "Out-Of-Memory",
        ] {
            assert_eq!(label(t), Label::Oom, "{t}");
        }
        assert_eq!(
            label("error loading model: failed to load model: failed to allocate buffer"),
            Label::Oom
        );
        assert_eq!(
            label("common_chat_templates_init: failed to parse chat template (Jinja)"),
            Label::Template
        );
        assert_eq!(label("unsupported chat_template"), Label::Template);
        assert_eq!(
            label("the request exceeds the available context size, try increasing it"),
            Label::RecallFailed
        );
        assert_eq!(label("Loading the test profile timed out."), Label::Timeout);
        assert_eq!(
            label("gguf_init_from_file_impl: invalid magic characters"),
            Label::LoadFailed
        );
        assert_eq!(
            label("llama_model_load: error loading model: unknown model architecture: 'x'"),
            Label::LoadFailed
        );
        assert_eq!(
            label("The model server failed during this step."),
            Label::Unknown
        );
        assert_eq!(label(""), Label::Unknown);
    }

    #[test]
    fn exit_codes_and_statuses() {
        let killed = Evidence {
            exit_code: Some(137),
            text: "chat template".into(),
            ..Evidence::default()
        };
        assert_eq!(classify(&killed), (Label::Oom, Some("exit_killed")));
        let gateway = Evidence {
            status: Some(504),
            ..Evidence::default()
        };
        assert_eq!(classify(&gateway), (Label::Timeout, Some("status_timeout")));
        let other = Evidence {
            status: Some(500),
            exit_code: Some(1),
            ..Evidence::default()
        };
        assert_eq!(classify(&other).0, Label::Unknown);
    }

    fn req(cause: Cause, text: Option<&str>, advice: Option<(Label, u16)>) -> Request {
        Request {
            cause,
            evidence: text.map(ev),
            advice: advice.map(|(label, permille)| Advice { label, permille }),
        }
    }

    #[test]
    fn measured_causes_stand() {
        let v = verdict(&req(Cause::Time, None, Some((Label::Oom, 1000))));
        assert_eq!(
            (v.outcome, v.source, v.ask, v.advice_used),
            (Outcome::OverTime, Source::Measured, false, false)
        );
    }

    #[test]
    fn a_rule_beats_any_advice() {
        let v = verdict(&req(
            Cause::Load,
            Some("out of memory"),
            Some((Label::Template, 1000)),
        ));
        assert_eq!(
            (v.outcome, v.source, v.advice_used),
            (Outcome::Oom, Source::Rule, false)
        );
        assert!(!v.ask);
    }

    #[test]
    fn advice_breaks_only_an_unknown() {
        let v = verdict(&req(Cause::Oom, Some("engine said something odd"), None));
        assert_eq!(
            (v.outcome, v.source, v.ask),
            (Outcome::Oom, Source::Fallback, true)
        );
        let v = verdict(&req(
            Cause::Oom,
            Some("engine said something odd"),
            Some((Label::Template, 820)),
        ));
        assert_eq!(
            (v.outcome, v.source, v.ask, v.advice_used),
            (Outcome::Template, Source::Advisor, false, true)
        );
        let v = verdict(&req(
            Cause::Oom,
            Some("engine said something odd"),
            Some((Label::Template, 599)),
        ));
        assert_eq!(
            (v.outcome, v.source, v.ask, v.advice_used),
            (Outcome::Oom, Source::Fallback, false, false)
        );
        let v = verdict(&req(Cause::Load, Some("odd"), Some((Label::Unknown, 1000))));
        assert_eq!(
            (v.outcome, v.source),
            (Outcome::LoadFailed, Source::Fallback)
        );
        // Nothing to read: no point asking.
        let v = verdict(&req(Cause::Load, Some("  "), None));
        assert!(!v.ask);
    }

    #[test]
    fn a_crash_is_never_a_time_out() {
        let crash = Evidence {
            status: Some(504),
            text: "Loading timed out; deadline exceeded".into(),
            crash: true,
            ..Evidence::default()
        };
        assert_eq!(classify(&crash), (Label::Unknown, None));
        let r = Request {
            cause: Cause::Oom,
            evidence: Some(crash),
            advice: None,
        };
        assert_eq!(verdict(&r).outcome, Outcome::Oom);
        // Other rules still read a crash's text.
        let oom = Evidence {
            text: "timed out after: out of memory".into(),
            crash: true,
            ..Evidence::default()
        };
        assert_eq!(classify(&oom).0, Label::Oom);
    }

    #[test]
    fn advice_never_says_timeout() {
        let v = verdict(&req(Cause::Oom, Some("odd"), Some((Label::Timeout, 900))));
        assert_eq!(
            (v.outcome, v.source, v.advice_used),
            (Outcome::Oom, Source::Fallback, false)
        );
    }

    #[test]
    fn json_shapes() {
        let (s, r) = verdict_json(
            r#"{"cause":"load","evidence":{"status":500,"exitCode":null,"text":"SECRET-ish odd text"},"advice":{"label":"oom","confidence":0.8125}}"#,
        );
        assert_eq!(s, 0);
        assert_eq!(
            r,
            r#"{"advice":{"label":"oom","permille":813},"adviceUsed":true,"ask":false,"outcome":"oom","reason":"The engine ran out of memory at this setting (no rule matched the engine's error; the decision service's reading was used).","rule":"unknown","ruleId":null,"source":"advisor"}"#
        );
        assert!(!r.contains("SECRET"));
        let (s, r) = verdict_json(r#"{"cause":"recall"}"#);
        assert_eq!(s, 0);
        assert!(r.contains(
            r#""reason":"The engine could not serve the full context at this setting.""#
        ));
        for bad in [
            "{",
            "[]",
            r#"{"cause":"x"}"#,
            r#"{"cause":"load","extra":1}"#,
            r#"{"cause":"load","evidence":{"text":1}}"#,
            r#"{"cause":"load","evidence":{"status":1000}}"#,
            r#"{"cause":"load","evidence":{"exitCode":1.5}}"#,
            r#"{"cause":"load","evidence":{"other":""}}"#,
            r#"{"cause":"load","advice":{"label":"oom","confidence":1.01}}"#,
            r#"{"cause":"load","advice":{"label":"over_time","confidence":0.9}}"#,
            r#"{"cause":"load","advice":{"label":"oom"}}"#,
            r#"{"cause":"load","evidence":[]}"#,
            r#"{"cause":"load","evidence":{"crash":1}}"#,
        ] {
            assert_eq!(
                verdict_json(bad),
                (1, r#"{"error":"input"}"#.to_owned()),
                "{bad}"
            );
        }
        let long = format!(
            r#"{{"cause":"load","evidence":{{"text":"{}"}}}}"#,
            "a".repeat(MAX_TEXT_BYTES + 1)
        );
        assert_eq!(verdict_json(&long).1, r#"{"error":"too_large"}"#);
        let huge = " ".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(verdict_json(&huge).1, r#"{"error":"too_large"}"#);
    }
}
