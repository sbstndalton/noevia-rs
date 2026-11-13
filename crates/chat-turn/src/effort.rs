//! The model request's reasoning effort and thinking cap: noevia-core `server/reasoning-effort.cjs`
//! (`modeFor`, `requestBody`, `forcedAnswerBody`, the request side of `requestWithEffort`) and the
//! numbers of `thinking-budget.cjs`.
//!
//! Only documented provider capabilities, or the local engine's Qwen3-family switch, get a real
//! effort parameter; any other effort is a system hint. A provider that rejects the parameter (or the
//! output budget) is remembered for the rest of the process, as Node's module-level sets do: here
//! the [`Rejected`] the executor keeps, keyed by provider id and model. While both executors run, the
//! two processes remember separately (one more warning at most).

use std::collections::HashSet;
use std::sync::OnceLock;

use js_json::JValue;
use regex::Regex;

use crate::js;

pub const HINT_LOW: &str = "Answer directly and concisely; skip step-by-step reasoning.";
pub const HINT_HIGH: &str = "Work this out carefully and check it before you reply. Then give the final answer only: do not show your working, a \"Reasoning:\" section, or corrections such as \"Wait, let me re-calculate\".";
/// thinking-budget.cjs FORCED_ANSWER.
pub const FORCED_ANSWER: &str = "Your thinking budget for this reply is used up. Write the final answer now, concisely and completely, without further reasoning. Do not call tools.";
pub const REJECTED_EFFORT_WARNING: &str =
    "This provider rejected reasoning effort. Using a best-effort hint instead.";
pub const REJECTED_BUDGET_WARNING: &str = "This provider rejected the output budget. The effort hint remains active with the provider’s default budget.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Real,
    Hint,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Real => "real",
            Mode::Hint => "hint",
        }
    }
}

/// What the request reads of the provider row (rust-chat-native.cjs `prepared.provider`).
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub id: String,
    /// reasoning-effort.cjs `localEngine(provider)`.
    pub local_engine: bool,
    /// The row's `capabilities`, or `undefined`.
    pub capabilities: JValue,
}

/// Providers that refused the effort parameter or the output budget (reasoning-effort.cjs
/// `rejected`, `rejectedBudget`).
#[derive(Debug, Default, Clone)]
pub struct Rejected {
    effort: HashSet<String>,
    budget: HashSet<String>,
}

fn key(provider: &Provider, model: &str) -> String {
    format!("{}\u{0}{model}", provider.id)
}

pub fn valid_effort(effort: &str) -> bool {
    matches!(effort, "default" | "low" | "high")
}

fn documented(provider: &Provider, model: &str) -> bool {
    let caps = &provider.capabilities;
    if !caps.is_object() || caps.get("reasoningEffortParam") != &JValue::Bool(true) {
        return false;
    }
    match caps.get("reasoningEffortModels") {
        JValue::Arr(models) => models.iter().any(|m| m.as_str() == Some(model)),
        _ => true,
    }
}

/// `max_completion_tokens` when the row says so, else `max_tokens`.
pub fn budget_field(provider: &Provider) -> &'static str {
    if provider.capabilities.get("tokenBudgetField").as_str() == Some("max_completion_tokens") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

fn qwen3() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|[/_])qwen3(?:\.5)?[-_]").ok())
        .as_ref()
}

/// The local engine's documented Qwen3 templates (machines are not native-executor turns yet).
pub fn native_thinking(provider: &Provider, model: &str) -> bool {
    provider.local_engine && qwen3().is_some_and(|re| re.is_match(model))
}

pub fn mode_for(provider: &Provider, model: &str, effort: &str, rejected: &Rejected) -> Mode {
    if effort == "default" {
        Mode::Off
    } else if (documented(provider, model) || native_thinking(provider, model))
        && !rejected.effort.contains(&key(provider, model))
    {
        Mode::Real
    } else {
        Mode::Hint
    }
}

fn hint(effort: &str) -> &'static str {
    if effort == "high" {
        HINT_HIGH
    } else {
        HINT_LOW
    }
}

/// `requestBody(body, effort, mode, provider)`.
pub fn request_body(body: &JValue, effort: &str, mode: Mode, provider: &Provider) -> JValue {
    let mut out = body.clone();
    match mode {
        Mode::Off => {}
        Mode::Real => {
            if native_thinking(provider, body.get("model").as_str().unwrap_or("")) {
                let mut kwargs = match body.get("chat_template_kwargs") {
                    o @ JValue::Obj(_) => o.clone(),
                    _ => js::obj(Vec::<(String, JValue)>::new()),
                };
                js::set(
                    &mut kwargs,
                    "enable_thinking",
                    JValue::Bool(effort == "high"),
                );
                js::set(&mut out, "chat_template_kwargs", kwargs);
            } else {
                js::set(&mut out, "reasoning_effort", js::s(effort));
            }
        }
        Mode::Hint => {
            let mut messages = vec![js::obj([
                ("role", js::s("system")),
                ("content", js::s(hint(effort))),
            ])];
            if let JValue::Arr(list) = body.get("messages") {
                messages.extend(list.iter().cloned());
            }
            js::set(&mut out, "messages", JValue::Arr(messages));
            if effort == "high" {
                let field = budget_field(provider);
                let value = match body.get(field) {
                    v if v.truthy() => v.clone(),
                    _ => match body.get("max_tokens") {
                        v if v.truthy() => v.clone(),
                        _ => js::n(8192.0),
                    },
                };
                js::set(&mut out, field, value);
            }
        }
    }
    out
}

/// `forcedAnswerBody(request, provider)`: tools dropped, the note last, thinking off on the local engine.
pub fn forced_answer_body(request: &JValue, provider: &Provider) -> JValue {
    let mut body = request.clone();
    js::remove(&mut body, "tools");
    js::remove(&mut body, "tool_choice");
    let messages = match request.get("messages") {
        JValue::Arr(list) => list.clone(),
        _ => Vec::new(),
    };
    let mut with_note = messages.clone();
    match messages.last() {
        Some(last)
            if last.get("role").as_str() == Some("user")
                && last.get("content").as_str().is_some() =>
        {
            let mut joined = last.clone();
            let content = format!(
                "{}\n\n{FORCED_ANSWER}",
                last.get("content").as_str().unwrap_or("")
            );
            js::set(&mut joined, "content", js::s(content));
            with_note.pop();
            with_note.push(joined);
        }
        _ => with_note.push(js::obj([
            ("role", js::s("user")),
            ("content", js::s(FORCED_ANSWER)),
        ])),
    }
    // `{ ...rest, stream: true, messages }`: both keys exist in a chat request, so they keep their places.
    js::set(&mut body, "stream", JValue::Bool(true));
    js::set(&mut body, "messages", JValue::Arr(with_note));
    if provider.local_engine {
        let mut kwargs = match request.get("chat_template_kwargs") {
            o @ JValue::Obj(_) => o.clone(),
            _ => js::obj(Vec::<(String, JValue)>::new()),
        };
        js::set(&mut kwargs, "enable_thinking", JValue::Bool(false));
        js::set(&mut body, "chat_template_kwargs", kwargs);
    }
    body
}

/// One request as `requestWithEffort` builds it, and what it decided.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub request: JValue,
    pub mode: Mode,
    /// The thinking cap the stream is wrapped with (none: not capped).
    pub thinking_cap: Option<u64>,
    /// The cap leaves no room to think and the request carries no tools: it asks for the answer at once.
    pub answer_directly: bool,
    explicit_budget: bool,
    body: JValue,
}

/// Builds the request for `body` (the turn's own fields: model, budget, messages, stream, tools...).
pub fn build(
    body: &JValue,
    provider: &Provider,
    model: &str,
    effort: &str,
    thinking_cap: Option<u64>,
    rejected: &Rejected,
) -> Built {
    let explicit_budget =
        body.get("max_tokens").truthy() || body.get("max_completion_tokens").truthy();
    let mut body = body.clone();
    if body.get("max_tokens").truthy() && budget_field(provider) == "max_completion_tokens" {
        let value = body.get("max_tokens").clone();
        js::set(&mut body, "max_completion_tokens", value);
        js::remove(&mut body, "max_tokens");
    }
    let streamed = body.get("stream") == &JValue::Bool(true);
    let no_room = thinking_cap == Some(0) && streamed;
    let has_tools = matches!(body.get("tools"), JValue::Arr(t) if !t.is_empty());
    let answer_directly = no_room && !has_tools;
    let thinking_cap = if no_room {
        answer_directly.then_some(0)
    } else {
        thinking_cap
    };
    let mut built = Built {
        request: JValue::Undefined,
        mode: mode_for(provider, model, effort, rejected),
        thinking_cap,
        answer_directly,
        explicit_budget,
        body,
    };
    built.request = prepare(&built, provider, effort, rejected, model);
    built
}

fn prepare(
    built: &Built,
    provider: &Provider,
    effort: &str,
    rejected: &Rejected,
    model: &str,
) -> JValue {
    let mut value = request_body(&built.body, effort, built.mode, provider);
    if built.mode == Mode::Hint
        && !built.explicit_budget
        && rejected.budget.contains(&key(provider, model))
    {
        js::remove(&mut value, "max_tokens");
        js::remove(&mut value, "max_completion_tokens");
    }
    if built.answer_directly {
        value = forced_answer_body(&value, provider);
    }
    value
}

/// What a 4xx answer to the first request asks for: the same request once more, with this warning.
/// `None`: the answer stands.
pub fn retry_after_refusal(
    built: &mut Built,
    detail: &str,
    provider: &Provider,
    model: &str,
    effort: &str,
    rejected: &mut Rejected,
) -> Option<&'static str> {
    let lower = detail.to_ascii_lowercase();
    if built.mode == Mode::Real
        && [
            "reasoning_effort",
            "chat_template_kwargs",
            "enable_thinking",
        ]
        .iter()
        .any(|w| lower.contains(w))
    {
        rejected.effort.insert(key(provider, model));
        built.mode = Mode::Hint;
        built.request = prepare(built, provider, effort, rejected, model);
        return Some(REJECTED_EFFORT_WARNING);
    }
    if built.mode == Mode::Hint
        && !built.explicit_budget
        && effort == "high"
        && (lower.contains("max_tokens") || lower.contains("max_completion_tokens"))
    {
        rejected.budget.insert(key(provider, model));
        built.request = prepare(built, provider, effort, rejected, model);
        return Some(REJECTED_BUDGET_WARNING);
    }
    None
}

/// The `meta` event `requestWithEffort` reports once its answer is in.
pub fn meta_event(built: &Built, effort: &str) -> JValue {
    let budget = match built.request.get("max_completion_tokens") {
        v if v.truthy() => v.clone(),
        _ => built.request.get("max_tokens").clone(),
    };
    let mut meta = js::obj([
        ("type", js::s("meta")),
        ("reasoning", js::s(built.mode.as_str())),
        ("reasoningEffort", js::s(effort)),
        ("outputBudget", budget),
    ]);
    if let Some(cap) = built.thinking_cap {
        #[allow(clippy::cast_precision_loss)]
        js::set(&mut meta, "thinkingCap", js::n(cap as f64));
    }
    meta
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn local() -> Provider {
        Provider {
            id: "default".into(),
            local_engine: true,
            capabilities: JValue::Undefined,
        }
    }

    #[test]
    fn the_modes_follow_the_row_and_the_model() {
        let r = Rejected::default();
        assert_eq!(mode_for(&local(), "qa-model", "default", &r), Mode::Off);
        assert_eq!(mode_for(&local(), "qa-model", "low", &r), Mode::Hint);
        assert_eq!(mode_for(&local(), "org/Qwen3-8B", "low", &r), Mode::Real);
        assert_eq!(mode_for(&local(), "qwen3.5_9b", "high", &r), Mode::Real);
        assert_eq!(mode_for(&local(), "xqwen3-8b", "high", &r), Mode::Hint);
        let documented = Provider {
            capabilities: js_json::parse(
                r#"{"reasoningEffortParam":true,"reasoningEffortModels":["a"]}"#,
            )
            .unwrap(),
            local_engine: false,
            ..local()
        };
        assert_eq!(mode_for(&documented, "a", "low", &r), Mode::Real);
        assert_eq!(mode_for(&documented, "b", "low", &r), Mode::Hint);
    }

    #[test]
    fn a_refused_effort_falls_back_to_the_hint_for_good() {
        let mut r = Rejected::default();
        let p = local();
        let body = js_json::parse(r#"{"model":"Qwen3-8B","max_tokens":100,"messages":[{"role":"user","content":"q"}],"stream":true}"#).unwrap();
        let mut built = build(&body, &p, "Qwen3-8B", "high", None, &r);
        assert_eq!(built.mode, Mode::Real);
        assert_eq!(
            js::stringify(&built.request),
            r#"{"model":"Qwen3-8B","max_tokens":100,"messages":[{"role":"user","content":"q"}],"stream":true,"chat_template_kwargs":{"enable_thinking":true}}"#
        );
        let warning = retry_after_refusal(
            &mut built,
            "unknown field enable_thinking",
            &p,
            "Qwen3-8B",
            "high",
            &mut r,
        );
        assert_eq!(warning, Some(REJECTED_EFFORT_WARNING));
        assert_eq!(built.mode, Mode::Hint);
        assert_eq!(
            build(&body, &p, "Qwen3-8B", "high", None, &r).mode,
            Mode::Hint
        );
        assert_eq!(
            retry_after_refusal(&mut built, "other", &p, "Qwen3-8B", "high", &mut r),
            None
        );
    }

    #[test]
    fn no_room_to_think_asks_for_the_answer_at_once_unless_tools_ride_along() {
        let r = Rejected::default();
        let body = js_json::parse(r#"{"model":"m","max_tokens":10,"messages":[{"role":"user","content":"q"}],"stream":true}"#).unwrap();
        let built = build(&body, &local(), "m", "default", Some(0), &r);
        assert!(built.answer_directly);
        assert_eq!(built.thinking_cap, Some(0));
        assert!(js::stringify(&built.request).contains(FORCED_ANSWER));
        let mut with_tools = body.clone();
        js::set(
            &mut with_tools,
            "tools",
            JValue::Arr(vec![js::obj([("type", js::s("function"))])]),
        );
        let built = build(&with_tools, &local(), "m", "default", Some(0), &r);
        assert!(!built.answer_directly);
        assert_eq!(built.thinking_cap, None);
    }
}
