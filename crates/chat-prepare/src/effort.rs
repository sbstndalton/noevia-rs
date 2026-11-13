//! A turn's reasoning effort and the Fast route's thinking switch: noevia-core
//! `server/reasoning-effort.cjs` `validEffort`, `resolveEffort`, `effortForRoute`,
//! `fastRouteThinkingOff` and `localEngine`, the parts chat.cjs reads before the model request
//! (slice P5a; [`crate::route`] puts them together as chat.cjs does). The request side of the
//! module (`modeFor`, `requestBody`, the hints and the thinking cap) is `chat-turn`'s `effort`;
//! its `valid_effort` is the same rule over a Rust string, and is copied here over JS values (the
//! two crates do not depend on each other).
//!
//! Replayed byte for byte on Node's output (`tests/fixtures/node-route.v1.json`, written by
//! `tests/record_route.cjs`).

use js_json::JValue;

/// One of `reasoning-effort.cjs` `EFFORTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Default,
    Low,
    High,
}

impl Effort {
    /// The effort as chat.cjs holds it (`'default'`, `'low'`, `'high'`).
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Default => "default",
            Effort::Low => "low",
            Effort::High => "high",
        }
    }

    /// `validEffort(value) ? value : null`: `EFFORTS.includes(value)`, so only these exact strings
    /// (no case folding, no trimming, never a number or an object).
    pub fn of(value: &JValue) -> Option<Effort> {
        match value.as_str()? {
            "default" => Some(Effort::Default),
            "low" => Some(Effort::Low),
            "high" => Some(Effort::High),
            _ => None,
        }
    }
}

/// `validEffort(value)`.
pub fn valid_effort(value: &JValue) -> bool {
    Effort::of(value).is_some()
}

/// `resolveEffort(project, globalDefault)`: the project's own effort, else the account default
/// (the settings row `reasoning_effort_default`'s value; `undefined` without the row), else
/// `'default'`. `project` is `null` (or `undefined`) for a turn without one.
pub fn resolve_effort(project: &JValue, global_default: &JValue) -> Effort {
    Effort::of(project.get("reasoningEffort"))
        .or_else(|| Effort::of(global_default))
        .unwrap_or(Effort::Default)
}

/// `effortForRoute(effort, routedRole)`: a turn routed Fast runs at Low; it only ever lowers.
/// `routed_role` is `None` for `null`/`undefined` (chat.cjs passes `null` for a non-Automatic turn).
pub fn effort_for_route(effort: Effort, routed_role: Option<&str>) -> Effort {
    if routed_role == Some("fast") && effort != Effort::Low {
        Effort::Low
    } else {
        effort
    }
}

/// `fastRouteThinkingOff({ auto, routedRole, projectEffort })`: an Automatic turn routed Fast whose
/// project names no effort of its own.
pub fn fast_route_thinking_off(
    auto: bool,
    routed_role: Option<&str>,
    project_effort: &JValue,
) -> bool {
    auto && routed_role == Some("fast") && !valid_effort(project_effort)
}

/// The two environment variables `localEngine` reads (`process.env`, as the server process holds
/// them: real text, `None` when unset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Engine<'a> {
    /// `MODEL_MANAGER_KIND`.
    pub kind: Option<&'a str>,
    /// `INFERENCE_BASE_URL`.
    pub inference_base_url: Option<&'a str>,
}

/// `localEngine(provider)`: the manager kind is exactly `'lemonade'` or `'llamacpp'` and the row's
/// `baseUrl` and `INFERENCE_BASE_URL` are the same URL once each is parsed (the WHATWG URL
/// Standard: the `url` crate here, Node's `new URL` there) and its `href` loses its trailing
/// slashes. A value that does not parse, or a `baseUrl` whose `String()` throws, is not the local
/// engine. The two parsers agree on every recorded pair (hosts, ports, IPv4 and IPv6 forms, IDNA,
/// dot segments, percent escapes, whitespace, non-special schemes); an input they read apart
/// (say, IDNA data of another Unicode release) would only change whether a Fast turn on the
/// default provider switches thinking off.
pub fn local_engine(provider: &JValue, engine: &Engine<'_>) -> bool {
    if !matches!(engine.kind, Some("lemonade" | "llamacpp")) {
        return false;
    }
    // `new URL(value)` reads a USVString: `String(value)`, then each lone surrogate is U+FFFD.
    let Ok(text) = js_json::to_js_string(provider.get("baseUrl")) else {
        return false;
    };
    let usv: String = text
        .chars()
        .map(|c| {
            if js_json::wtf16::lone_unit(c).is_some() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect();
    let (Some(row), Some(env)) = (
        normalize(&usv),
        engine.inference_base_url.and_then(normalize),
    ) else {
        return false;
    };
    row == env
}

/// `new URL(value).href.replace(/\/+$/, '')`; `None` where `new URL` throws.
fn normalize(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    Some(url.as_str().trim_end_matches('/').to_string())
}
