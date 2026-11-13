//! The Automatic classification chat.cjs asks for (`classifyFastOrSmart`, slice P5b): noevia-core
//! `server/system-one-router.cjs` `classifyWithDetails`, `clearlySimple` and `ROUTE_LABELS`, and
//! `decision-wording.cjs` `normalizeWording`. It asks the decision service (feature
//! `systemOneRouting`) which role answers, and otherwise falls back to the auto router
//! ([`crate::router`]); either way it states its `routingDecision`.
//!
//! The flow, with the caller answering the seams ([`Router::start`]):
//! - [`Start::Fallback`]: run the auto router, then [`Router::fallback`] with its role.
//! - [`Start::Simple`]: a plainly simple ask goes Fast with no call.
//! - [`Start::Decide`]: send the request to the decision service (`decisions.decide`), then
//!   [`Router::accepted`] of its result; when that is `None`, run the auto router; then
//!   [`Router::decided`] with the role.
//!
//! The decision log (`log`, `recordDecision`) is text-free logging and not ported.

use std::sync::OnceLock;

use js_json::JValue;
use regex::Regex;

use crate::js;
use crate::roles::{Classification, Role};
use crate::router::{JS_SPACE, WORD_EDGE};

/// The decision backend in use (`getBackend() || candidate`).
#[derive(Debug, Clone, Copy)]
pub struct Backend<'a> {
    /// Its `id` (`'llama-logit'` for the configured endpoint).
    pub id: &'a JValue,
    /// Its `wording`.
    pub wording: &'a JValue,
}

/// What `classifyWithDetails` reads besides the message.
#[derive(Debug, Clone, Copy)]
pub struct Router<'a> {
    /// `roles()`: the workspace's roles (`null` when none).
    pub roles: &'a JValue,
    /// `enabled()`: `features.enabled('systemOneRouting')`.
    pub enabled: bool,
    /// The backend, `None` when none is configured.
    pub backend: Option<Backend<'a>>,
    /// `precheck` (index.cjs leaves it at its default, true).
    pub precheck: bool,
    /// `getDeadlineMs()` (the decision settings' `timeoutMs`).
    pub deadline_ms: &'a JValue,
    /// `decision-models.cjs` `DECISION_SERVICE_REPORTED_MODEL`, the model id the service reports.
    pub service_model: &'a str,
}

/// What [`Router::start`] asks of the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    /// No decision call: `fallbackReason` (`disabled`, `no-backend`, `missing-roles`).
    Fallback(&'static str),
    Simple(Classification),
    /// The request for `decisions.decide`.
    Decide(JValue),
}

/// `normalizeWording(value)`: `standard` or `compact`, the old names `laya` and `kev` mapped to them.
pub fn normalize_wording(value: &JValue) -> Option<&'static str> {
    match value.as_str()? {
        "standard" | "laya" => Some("standard"),
        "compact" | "kev" => Some("compact"),
        _ => None,
    }
}

/// `ROUTE_LABELS[wording]`: the Fast and Smart labels.
pub fn route_labels(wording: &str) -> (&'static str, &'static str) {
    if wording == "compact" {
        ("Small talk, a quick fact, or a simple short task: counting, lists, a joke, a short poem or a short story",
         "Explaining, comparing, planning, multi-step reasoning or maths, or long careful writing")
    } else {
        (
            "Greetings, thanks, or a one-line factual answer",
            "Explaining, comparing, planning, reasoning or writing more than a sentence",
        )
    }
}

const CODE_LABEL: &str = "Anything involving programming code, regex, errors or software";

struct Patterns {
    simple: Regex,
    not_simple: Regex,
    words: Regex,
}

fn patterns() -> Option<&'static Patterns> {
    static P: OnceLock<Option<Patterns>> = OnceLock::new();
    P.get_or_init(|| {
        let (b, s) = (WORD_EDGE, JS_SPACE);
        let re = |p: String| Regex::new(&p).ok();
        Some(Patterns {
            simple: re(format!(
                "^{s}*(please{s}+)?(can you{s}+|could you{s}+)?(count{b}|list{b}|spell{b}|tell me (a|another) (short )?joke{b}\
                 |(write|give me|make|compose) (me )?(a|an|another){s}+(short{s}+|quick{s}+|funny{s}+|little{s}+|tiny{s}+\
                 |[0-9]{{1,2}}[- ]line{s}+|[0-9]{{2,3}}[- ]word{s}+)*(poem|haiku|limerick|rhyme|joke|story|riddle){b})"
            ))?,
            not_simple: re(format!(
                "{b}(explain|why|how|compare|plan|analy[sz]e|reason|prove|derive|step[- ]by[- ]step|in detail|detailed|essay\
                 |code|function|script|program|regex|sql|debug|bug){b}|```"
            ))?,
            words: re(format!("{b}([0-9]{{2,5}})[- ]words?{b}"))?,
        })
    })
    .as_ref()
}

/// `clearlySimple(message)`: a short single-line ask for a plainly simple task (count, list, spell,
/// a joke, a short poem, a story of at most 400 words) that asks nothing harder.
pub fn clearly_simple(message: &str) -> bool {
    let Some(p) = patterns() else {
        return false;
    };
    let trimmed = js::trim(message);
    let lower = message.to_ascii_lowercase();
    if trimmed.is_empty()
        || js::len(message) > 160
        || trimmed.contains('\n')
        || !p.simple.is_match(&lower)
        || p.not_simple.is_match(&lower)
    {
        return false;
    }
    p.words
        .captures(&lower)
        .and_then(|c| c.get(1))
        .is_none_or(|n| n.as_str().parse::<u32>().is_ok_and(|n| n <= 400))
}

/// `CAUSE_RE` (`decision/index.cjs`): `/^[a-z][a-z0-9-]{0,39}$/`.
fn cause_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=40).contains(&b.len())
        && b.first().is_some_and(u8::is_ascii_lowercase)
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn finite(v: &JValue) -> Option<f64> {
    v.as_num().filter(|n| n.is_finite())
}

impl Router<'_> {
    fn options(&self) -> Vec<JValue> {
        let wording = self
            .backend
            .filter(|_| self.enabled)
            .and_then(|b| normalize_wording(b.wording))
            .unwrap_or("standard");
        let (fast, smart) = route_labels(wording);
        let option = |id: &str, label: &str| {
            JValue::obj([("id", JValue::from(id)), ("label", JValue::from(label))])
        };
        let mut options = vec![option("fast", fast), option("smart", smart)];
        if self.roles.get("code").truthy() {
            options.push(option("code", CODE_LABEL));
        }
        options
    }

    /// `offered`: the options when Fast and Smart are both saved, else none.
    fn offered(&self) -> JValue {
        let both = self.roles.get("fast").truthy() && self.roles.get("smart").truthy();
        JValue::Arr(if both { self.options() } else { Vec::new() })
    }

    fn decision(&self, fields: [(&str, JValue); 10]) -> JValue {
        let mut items = vec![("offered".to_string(), self.offered())];
        items.extend(fields.into_iter().map(|(k, v)| (k.to_string(), v)));
        JValue::Obj(items)
    }

    /// The first step of `classifyWithDetails(message)`.
    pub fn start(&self, message: &str) -> Start {
        let routing = self.enabled && self.backend.is_some();
        let offered = matches!(self.offered(), JValue::Arr(o) if !o.is_empty());
        if !routing || !offered {
            return Start::Fallback(if !self.enabled {
                "disabled"
            } else if !routing {
                "no-backend"
            } else {
                "missing-roles"
            });
        }
        if self.precheck && clearly_simple(message) {
            let s = |v: &str| JValue::from(v);
            return Start::Simple(Classification {
                role: Role::Fast,
                decision: self.decision([
                    ("scores", JValue::Obj(Vec::new())),
                    ("selectedRole", s("fast")),
                    ("effectiveRole", s("fast")),
                    ("backend", s("legacy")),
                    ("model", JValue::Null),
                    ("calibrated", JValue::Bool(false)),
                    ("latencyMs", JValue::Num(0.0)),
                    ("status", s("accepted")),
                    ("fallbackReason", JValue::Null),
                    ("fallbackCause", JValue::Null),
                ]),
            });
        }
        Start::Decide(JValue::obj([
            ("kind", JValue::from("choice")),
            ("purpose", JValue::from("model.route")),
            (
                "question",
                JValue::from("Which configured model role should answer this user message?"),
            ),
            (
                "context",
                JValue::obj([
                    ("cloud", JValue::from("forbidden")),
                    ("stateText", JValue::Str(js::slice(message, 1000))),
                ]),
            ),
            ("options", JValue::Arr(self.options())),
            (
                "constraints",
                JValue::obj([("deadlineMs", self.deadline_ms.clone())]),
            ),
            (
                "fallback",
                JValue::obj([
                    ("selected", JValue::Null),
                    ("scores", JValue::Obj(Vec::new())),
                ]),
            ),
        ]))
    }

    /// The classification after [`Start::Fallback`], `role` being the auto router's.
    pub fn fallback(&self, reason: &str, role: Role) -> Classification {
        let s = |v: &str| JValue::from(v);
        Classification {
            role,
            decision: self.decision([
                ("scores", JValue::Obj(Vec::new())),
                ("selectedRole", JValue::Null),
                ("effectiveRole", s(role.as_str())),
                ("backend", s("legacy")),
                ("model", JValue::Null),
                ("calibrated", JValue::Bool(false)),
                ("latencyMs", JValue::Null),
                ("status", s("fallback")),
                ("fallbackReason", s(reason)),
                ("fallbackCause", JValue::Null),
            ]),
        }
    }

    /// The role the decision service chose, when its result is accepted: not its own fallback, and one
    /// of the options. `result` is `decide`'s result (always an object there).
    pub fn accepted(&self, result: &JValue) -> Option<Role> {
        if result.get("source").as_str() == Some("fallback") {
            return None;
        }
        let selected = result.get("selected").as_str()?;
        self.options()
            .iter()
            .any(|o| o.get("id").as_str() == Some(selected))
            .then(|| Role::of(selected))
            .flatten()
    }

    /// The classification after [`Start::Decide`]: `role` is [`Router::accepted`]'s, else the auto
    /// router's.
    pub fn decided(&self, result: &JValue, role: Role) -> Classification {
        let accepted = self.accepted(result).is_some();
        let meta = result.get("metadata");
        let s = |v: &str| JValue::from(v);
        let scores = if accepted {
            self.options()
                .iter()
                .filter_map(|o| {
                    let id = o.get("id").as_str()?;
                    finite(result.get("scores").get(id)).map(|n| (id.to_string(), JValue::Num(n)))
                })
                .collect()
        } else {
            Vec::new()
        };
        let cause = match meta.get("cause").as_str() {
            Some(c) if !accepted && cause_ok(c) => s(c),
            _ => JValue::Null,
        };
        let reason = match meta.get("fellBack").as_str() {
            _ if accepted => JValue::Null,
            Some(r @ ("deadline" | "no-backend-answered" | "low-confidence")) => s(r),
            _ => s("rejected"),
        };
        let backend = match self.backend {
            Some(b) if accepted && b.id.as_str() == Some("llama-logit") => "llama-logit",
            _ if accepted => "decision-service",
            _ => "legacy",
        };
        let model = if accepted && meta.get("model").as_str() == Some(self.service_model) {
            s(self.service_model)
        } else {
            JValue::Null
        };
        Classification {
            role,
            decision: self.decision([
                ("scores", JValue::Obj(scores)),
                (
                    "selectedRole",
                    if accepted {
                        result.get("selected").clone()
                    } else {
                        JValue::Null
                    },
                ),
                ("effectiveRole", s(role.as_str())),
                ("backend", s(backend)),
                ("model", model),
                ("calibrated", JValue::Bool(false)),
                (
                    "latencyMs",
                    finite(meta.get("latencyMs")).map_or(JValue::Null, JValue::Num),
                ),
                ("status", s(if accepted { "accepted" } else { "fallback" })),
                ("fallbackReason", reason),
                ("fallbackCause", cause),
            ]),
        }
    }
}
