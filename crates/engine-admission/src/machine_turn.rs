//! core chat-machines.cjs (#1840, #1849, #1851, #1854, #1880) and machine-health.cjs (#1638, #1861): a chat
//! role served by another machine's OpenAI-compatible server, as the turn sees it. Read-only and inert: nothing
//! in the front calls this yet (pool item 1, p8-3 CONTINUE 4 "machines"); a turn on a machine still runs in Node.
//!
//! What it carries, in Node's words and order:
//! - [`provider_row`] / [`MachineTurns::measured_provider`]: the provider-shaped row a turn uses for the role on
//!   its machine (`machine:<id>`, no key, `<base>/v1`, the measured context and thinking switch);
//! - [`MachineTurns::ready`]: whether the machine answers its health check AND serves the assigned model, with
//!   the "unreachable" / "serves …, not …" texts ([`unreachable_text`], [`wrong_model_text`]);
//! - the facts a machine's server says about itself ([`props_facts_of`] over `/props`, [`model_ids`] over
//!   `/v1/models`), kept beside its health result in one ~30 s cache ([`HealthCache`]);
//! - a Manual pick of a model a machine serves ([`manual_target`]), the Manual list ([`machine_models`]) and the
//!   Automatic roles as a turn sees them ([`effective_roles`], [`local_roles`]);
//!
//! Not here: the turns in flight per machine (`beginTurn`/`busy`/`onTurn`, #1880). In the front the native machine
//! service owns them (noevia-server machine_service.rs `turnBegin`/`turnEnd`, which also stops a measurement on that
//! machine); a wiring takes the turn there, around the request, as chat.cjs `holdMachine` does.
//!
//! Every request goes through [`Fetch`]; [`crate::machine_health::NetFetch`] is the on-this-network client (model_stream::net: the
//! address checked at connect time, no redirect, no proxy) with a deadline and a body cap.
use crate::machine_health::{Fetch, FetchError, HealthCache, ProbeOptions};
use crate::machines::Target;
use js_json::{wtf16, JValue};
use std::future::Future;
use std::time::Duration;

/// `ROLE_LABEL`.
pub const CHAT_ROLES: [&str; 5] = ["fast", "smart", "code", "creative", "vision"];
pub const MODELS_TIMEOUT: Duration = Duration::from_millis(5000);
pub const PROPS_TIMEOUT: Duration = Duration::from_millis(2500);
pub const MODELS_BODY_CAP: usize = 256 * 1024;
/// A /props answer carries the whole chat template (#1850 reads it for the tools gate).
pub const PROPS_BODY_CAP: usize = 1024 * 1024;
pub const MAX_MODELS: usize = 200;
pub const CONTEXT_MIN: f64 = 2048.0;
pub const CONTEXT_MAX: f64 = 2_000_000.0;
/// A llama-server that does not say its context gets the local engine's conservative floor.
pub const CONTEXT_FALLBACK: u32 = 8192;
/// #1854: the measurements that belong to this machine's engine and are never taken on another machine.
pub const NOT_MEASURED_ON_MACHINES: [&str; 4] = [
    "autotune",
    "speculativeDecoding",
    "longContextProfile",
    "presets",
];

/// `ROLE_LABEL[role] || role`.
pub fn label(role: &str) -> &str {
    match role {
        "fast" => "Fast",
        "smart" => "Smart",
        "code" => "Code",
        "creative" => "Creative",
        "vision" => "Vision",
        other => other,
    }
}

fn is_chat_role(role: &str) -> bool {
    CHAT_ROLES.contains(&role)
}

/// JS white space and line terminators (`String.prototype.trim`).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// `StringToNumber`.
fn string_to_number(text: &str) -> f64 {
    let t = js_trim(text);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = |digits: &str, base: u32| {
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(base)) {
            return f64::NAN;
        }
        digits
            .chars()
            .filter_map(|c| c.to_digit(base))
            .fold(0.0, |acc, d| acc * f64::from(base) + f64::from(d))
    };
    let bytes = t.as_bytes();
    if bytes.len() > 2 && bytes.first() == Some(&b'0') {
        match bytes.get(1) {
            Some(b'x' | b'X') => return radix(t.get(2..).unwrap_or(""), 16),
            Some(b'o' | b'O') => return radix(t.get(2..).unwrap_or(""), 8),
            Some(b'b' | b'B') => return radix(t.get(2..).unwrap_or(""), 2),
            _ => {}
        }
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (unsigned.get(..at).unwrap_or(""), unsigned.get(at + 1..)),
        None => (unsigned, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) || int.is_empty())
                && (digits(frac) || frac.is_empty())
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    if !mantissa_ok || !exponent_ok {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `Number(v)`.
fn to_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(x) => *x,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Null => 0.0,
        JValue::Str(text) => string_to_number(text),
        // `Number([])` is 0 and `Number([x])` is `Number(String(x))`; objects are NaN.
        JValue::Arr(items) => match items.as_slice() {
            [] => 0.0,
            [JValue::Null | JValue::Undefined] => 0.0,
            [one @ (JValue::Num(_) | JValue::Str(_))] => to_number(one),
            [JValue::Arr(inner)] => to_number(&JValue::Arr(inner.clone())),
            _ => f64::NAN,
        },
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
    }
}

/// `/\benable_thinking\b/`: ASCII word boundaries on both sides.
fn names_thinking_switch(template: &str) -> bool {
    const WORD: &str = "enable_thinking";
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = template.as_bytes();
    let mut from = 0;
    while let Some(at) = template.get(from..).and_then(|rest| rest.find(WORD)) {
        let start = from + at;
        let end = start + WORD.len();
        let before = start == 0 || bytes.get(start - 1).is_none_or(|b| !word(*b));
        let after = bytes.get(end).is_none_or(|b| !word(*b));
        if before && after {
            return true;
        }
        from = start + 1;
    }
    false
}

/// What a llama-server `/props` body says (each `None` when it does not say).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PropsFacts {
    pub context_tokens: Option<u32>,
    pub thinking: Option<bool>,
    pub vision: Option<bool>,
}

/// core `propsFactsOf(body)`.
pub fn props_facts_of(body: &JValue) -> PropsFacts {
    let n = to_number(body.get("default_generation_settings").get("n_ctx"));
    let context_tokens =
        (n.fract() == 0.0 && (CONTEXT_MIN..=CONTEXT_MAX).contains(&n)).then_some(n as u32);
    let thinking = match body.get("chat_template") {
        JValue::Str(t) if !t.is_empty() => Some(names_thinking_switch(t)),
        _ => None,
    };
    let modalities = body.get("modalities");
    let vision = if modalities.truthy() {
        modalities.get("vision").as_bool()
    } else {
        None
    };
    PropsFacts {
        context_tokens,
        thinking,
        vision,
    }
}

/// core `modelIds(body)`: llama-server `data[].id`, older servers `models[].name|model`; at most
/// [`MAX_MODELS`], each 1..=200 UTF-16 units of non-control text, no repeats.
pub fn model_ids(body: &JValue) -> Vec<String> {
    let rows: &[JValue] = match (body.get("data"), body.get("models")) {
        (JValue::Arr(rows), _) => rows,
        (_, JValue::Arr(rows)) => rows,
        _ => &[],
    };
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        // `typeof r === 'string' ? r : r?.id ?? r?.model ?? r?.name`: `??` passes over null and undefined only.
        let id = match r {
            JValue::Str(id) => Some(id),
            _ => match [r.get("id"), r.get("model"), r.get("name")]
                .into_iter()
                .find(|v| !v.is_nullish())
            {
                Some(JValue::Str(id)) => Some(id),
                _ => None,
            },
        };
        if let Some(id) = id {
            let clean = !js_trim(id).is_empty()
                && wtf16::len(id) <= 200
                && !id.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f);
            if clean && !out.iter().any(|o| o == id) {
                out.push(id.clone());
            }
        }
        if out.len() >= MAX_MODELS {
            break;
        }
    }
    out
}

/// core `unreachableText(t, detail)`.
pub fn unreachable_text(t: &Target, detail: &str) -> String {
    let role = label(&t.role);
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    format!(
        "{role} model\u{2019}s machine ({}) is unreachable{detail}. Check that it is on and serving {} on port {}, or move the {role} role back to this machine in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.port
    )
}

/// core `wrongModelText(t, served)`.
pub fn wrong_model_text(t: &Target, served: &[String]) -> String {
    let role = label(&t.role);
    let first = served
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let first = if first.is_empty() {
        "no model".to_owned()
    } else {
        first
    };
    let more = if served.len() > 3 { " and more" } else { "" };
    format!(
        "The {role} machine ({}) serves {first}{more}, not {}. Start {} on port {}, or choose the model it serves for the {role} role in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.model, t.port
    )
}

/// Why a request to a machine failed at the socket (core `connectError`'s three cases, else none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectWhy {
    /// `EPUBLICADDR`: the on-this-network rule refused the address.
    NotOnThisNetwork,
    /// The machine answered with a redirect.
    Redirect,
    /// `TimeoutError`.
    Timeout,
    Other,
}

/// core `connectError(t, err)`. (`machines::connect_error` has no timeout case; this one has Node's three.)
pub fn connect_error(t: &Target, why: ConnectWhy) -> String {
    unreachable_text(
        t,
        match why {
            ConnectWhy::NotOnThisNetwork => "its address is not on this network",
            ConnectWhy::Redirect => "it answered with a redirect, which is never followed",
            ConnectWhy::Timeout => "no answer in time",
            ConnectWhy::Other => "",
        },
    )
}

/// core `provider(t, { contextTokens, thinking })`: the provider row a turn uses for the role on its machine,
/// keys in Node's order.
pub fn provider_row(t: &Target, context_tokens: Option<u32>, thinking: Option<bool>) -> JValue {
    let mut row = vec![
        (
            "id".to_owned(),
            JValue::from(format!("machine:{}", t.machine_id)),
        ),
        (
            "label".to_owned(),
            JValue::from(format!(
                "{} model\u{2019}s machine ({})",
                label(&t.role),
                t.machine_name
            )),
        ),
        (
            "baseUrl".to_owned(),
            JValue::from(format!("{}/v1", t.base_url)),
        ),
        ("apiKey".to_owned(), JValue::from("")),
        ("shared".to_owned(), JValue::Bool(true)),
    ];
    if let Some(ctx) = context_tokens.filter(|c| *c != 0) {
        row.push(("contextTokens".to_owned(), JValue::Num(f64::from(ctx))));
    }
    let mut machine = vec![
        ("id".to_owned(), JValue::from(t.machine_id.as_str())),
        ("name".to_owned(), JValue::from(t.machine_name.as_str())),
        ("role".to_owned(), JValue::from(t.role.as_str())),
        ("port".to_owned(), JValue::Num(f64::from(t.port))),
        ("model".to_owned(), JValue::from(t.model.as_str())),
    ];
    if thinking == Some(true) {
        machine.push(("thinkingSwitch".to_owned(), JValue::Bool(true)));
    }
    row.push(("machine".to_owned(), JValue::Obj(machine)));
    JValue::Obj(row)
}

/// What a machine's server says about itself (#1851, #1854), kept beside its health result: the models it serves
/// (`/v1/models`) and its `/props` facts. A read that fails keeps the last known value; each is `None` only when
/// nothing has ever been read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineFacts {
    pub models: Option<Vec<String>>,
    pub context_tokens: Option<u32>,
    pub thinking: Option<bool>,
    pub vision: Option<bool>,
}

/// core `measurements(t)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurements {
    pub context_tokens: Option<u32>,
    pub thinking: Option<bool>,
    pub vision: Option<bool>,
    pub not_measured: [&'static str; 4],
}

/// A `modelsAt` failure: always HTTP 502, with the code and text the Settings route shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsError {
    pub status: u16,
    /// `publicAddress` | `machineUnreachable` | `tooLarge`.
    pub code: &'static str,
    pub message: String,
}

/// core `ready(t)`: `{ ok:true }` | `{ ok:false, state, code, error }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    Ok,
    Refused {
        /// The probe's state word, or `wrongModel`.
        state: String,
        /// `machineUnreachable` | `machineWrongModel`.
        code: &'static str,
        error: String,
    },
}

/// What chat.cjs answers for a turn whose machine is not ready (l.1253-1259).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotSent {
    /// HTTP status of a turn that has not started streaming: always 503.
    pub status: u16,
    pub code: &'static str,
    /// The `error` (JSON) or the `error` event's `text` (stream).
    pub error: String,
    /// chat-admission.cjs `log503(reason)` (#1896), for a JSON answer only.
    pub log503: &'static str,
    /// The `[machines] …` warning line.
    pub warning: String,
}

/// chat.cjs's answer for a [`Ready::Refused`]; `role` is the routed role, else the target's.
pub fn not_sent(ready: &Ready, routed_role: Option<&str>, t: &Target) -> Option<NotSent> {
    let Ready::Refused { state, code, error } = ready else {
        return None;
    };
    let role = routed_role
        .filter(|r| !r.is_empty())
        .unwrap_or(t.role.as_str());
    Some(NotSent {
        status: 503,
        code: *code,
        error: error.clone(),
        log503: if *code == "machineWrongModel" {
            "machine-wrong-model"
        } else {
            "machine-unreachable"
        },
        warning: format!("[machines] {role} role's machine is {state}; the turn is not sent"),
    })
}

/// The detail chat-machines.cjs `ready` puts after "is unreachable" for a probe that is not up.
pub fn probe_detail(p: &crate::machine_health::Probe) -> String {
    use crate::machine_health::State;
    let reason = p.reason.as_deref().unwrap_or("");
    if reason == "not-on-this-network" {
        "its address is not on this network".to_owned()
    } else if p.state == State::Down {
        let r = reason
            .strip_prefix("http-")
            .map_or_else(|| reason.to_owned(), |rest| format!("HTTP {rest}"));
        js_trim(&format!("it answered {r}")).to_owned()
    } else if reason == "timeout" {
        "no answer in time".to_owned()
    } else {
        String::new()
    }
}

/// core `ready`'s decision from the probe and the facts: not up is unreachable; up but not serving the model (when
/// the models could be read at all) is the wrong model.
pub fn ready_of(
    t: &Target,
    probe: &crate::machine_health::Probe,
    facts: Option<&MachineFacts>,
) -> Ready {
    use crate::machine_health::State;
    if probe.state != State::Up {
        return Ready::Refused {
            state: probe.state.word().to_owned(),
            code: "machineUnreachable",
            error: unreachable_text(t, &probe_detail(probe)),
        };
    }
    if let Some(models) = facts.and_then(|f| f.models.as_ref()) {
        if !models.iter().any(|m| *m == t.model) {
            return Ready::Refused {
                state: "wrongModel".to_owned(),
                code: "machineWrongModel",
                error: wrong_model_text(t, models),
            };
        }
    }
    Ready::Ok
}

/// The machine's address as saved (`machine.address`): [`Target::base_url`] is `<address>:<port>`.
pub fn address_of(t: &Target) -> &str {
    t.base_url
        .strip_suffix(&format!(":{}", t.port))
        .unwrap_or(&t.base_url)
}

/// `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The roles on another machine, in role order (core `remoteRoles`), from the `inference:machines` row's text.
pub fn remote_roles(row: Option<&str>) -> Vec<Target> {
    CHAT_ROLES
        .iter()
        .filter_map(|role| crate::machines::chat_target(row, role))
        .collect()
}

/// core `effectiveRoles(roles)`: the Automatic roles as a turn sees them, a role on a machine naming the model that
/// machine serves. Not an object, or nothing on a machine: unchanged.
pub fn effective_roles(roles: &JValue, remote: &[Target]) -> JValue {
    let JValue::Obj(items) = roles else {
        return roles.clone();
    };
    let mut out = items.clone();
    for t in remote {
        wtf16::set(&mut out, &t.role, JValue::from(t.model.as_str()));
    }
    JValue::Obj(out)
}

/// core `localRoles(roles)`: the roles this machine's engine serves (a role on a machine is not checked against
/// the local catalogue).
pub fn local_roles(roles: &JValue, remote: &[Target]) -> JValue {
    let JValue::Obj(items) = roles else {
        return roles.clone();
    };
    JValue::Obj(
        items
            .iter()
            .filter(|(k, _)| !remote.iter().any(|t| t.role == *k))
            .cloned()
            .collect(),
    )
}

/// One row of the Manual list (#1849): a model a machine serves, and the roles it serves it for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineModel {
    pub model: String,
    pub machine_id: String,
    pub machine_name: String,
    pub roles: Vec<String>,
}

/// core `machineModels()`.
pub fn machine_models(remote: &[Target]) -> Vec<MachineModel> {
    let mut out: Vec<MachineModel> = Vec::new();
    for t in remote {
        match out
            .iter_mut()
            .find(|r| r.model == t.model && r.machine_id == t.machine_id)
        {
            Some(row) => row.roles.push(t.role.clone()),
            None => out.push(MachineModel {
                model: t.model.clone(),
                machine_id: t.machine_id.clone(),
                machine_name: t.machine_name.clone(),
                roles: vec![t.role.clone()],
            }),
        }
    }
    out
}

/// core `manualTarget(model, catalogue)` (#1849, #1855): a Manual pick runs on a machine when a chat role's machine
/// serves exactly that name and this machine's catalogue does not list it. `catalogue` is read only when some role's
/// machine serves the name; an unreadable catalogue (`Err`) keeps the pick here.
pub async fn manual_target<C, Fut>(model: &str, remote: &[Target], catalogue: C) -> Option<Target>
where
    C: FnOnce() -> Fut,
    Fut: Future<Output = Result<JValue, ()>>,
{
    if model.is_empty() {
        return None;
    }
    let found = remote.iter().find(|t| t.model == model)?;
    let rows = catalogue().await.ok()?;
    if let JValue::Arr(rows) = &rows {
        if rows.iter().any(|m| m.get("name").as_str() == Some(model)) {
            return None;
        }
    }
    Some(found.clone())
}

/// chat.cjs l.1246-1251: which machine a turn is sent to, before any request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnFacts<'a> {
    /// An Automatic turn (`wantsAuto`).
    pub automatic: bool,
    /// A route target row is set (`routeTarget?.row`).
    pub route_row: bool,
    /// The provider is the default one (this machine's engine).
    pub default_provider: bool,
    /// The routed role of an Automatic turn.
    pub routed_role: Option<&'a str>,
    /// The model a Manual turn names.
    pub model: &'a str,
}

/// The pick of [`pick`]: nothing (the turn runs here), an Automatic role's machine, or a Manual pick that needs
/// [`manual_target`] (the catalogue read is async).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    Here,
    Role(Target),
    Manual,
}

/// The synchronous half of chat.cjs's machine choice.
pub fn pick(f: &TurnFacts<'_>, row: Option<&str>) -> Pick {
    if f.route_row || !f.default_provider {
        return Pick::Here;
    }
    if f.automatic {
        return match f.routed_role.filter(|r| !r.is_empty() && is_chat_role(r)) {
            Some(role) => crate::machines::chat_target(row, role).map_or(Pick::Here, Pick::Role),
            None => Pick::Here,
        };
    }
    Pick::Manual
}

/// core `createChatMachines`'s request side over one fetch and one shared health cache.
pub struct MachineTurns<F> {
    health: Option<HealthCache<F>>,
    fetch: F,
}

impl<F: Fetch + Clone> MachineTurns<F> {
    /// With the shared health cache (production) over the same fetch.
    pub fn new(fetch: F) -> Self {
        MachineTurns {
            health: Some(HealthCache::new(fetch.clone())),
            fetch,
        }
    }
}

impl<F: Fetch> MachineTurns<F> {
    /// `health` `None`: every machine is ready and facts are read on each call (core's `health = null`).
    pub fn with(fetch: F, health: Option<HealthCache<F>>) -> Self {
        MachineTurns { health, fetch }
    }

    pub fn health(&self) -> Option<&HealthCache<F>> {
        self.health.as_ref()
    }

    /// core `modelsAt(address, port, name)`.
    pub async fn models_at(
        &self,
        address: &str,
        port: u32,
        name: &str,
    ) -> Result<Vec<String>, ModelsError> {
        let fail = |code, message| ModelsError {
            status: 502,
            code,
            message,
        };
        let url = format!("{address}:{port}/v1/models");
        let r = match self
            .fetch
            .get(&url, &[], MODELS_TIMEOUT, Some(MODELS_BODY_CAP))
            .await
        {
            Ok(r) => r,
            Err(FetchError::NotOnThisNetwork) => {
                return Err(fail(
                    "publicAddress",
                    "Machines must be on this network (a LAN or Tailscale address).".to_owned(),
                ))
            }
            Err(_) => {
                return Err(fail(
                    "machineUnreachable",
                    format!("{name} did not answer on port {port}."),
                ))
            }
        };
        if !r.ok() {
            return Err(fail(
                "machineUnreachable",
                format!(
                    "{name} answered HTTP {} on port {port}; it does not look like an OpenAI-compatible server.",
                    r.status
                ),
            ));
        }
        match r.body.map(|b| b.map(|text| wtf16::parse(&text))) {
            Some(Ok(Ok(body))) => Ok(model_ids(&body)),
            _ => Err(fail(
                "machineUnreachable",
                format!("{name} did not list its models on port {port}."),
            )),
        }
    }

    /// core `propsFacts(t)`: one GET of the machine's `/props` for the model; `None` for a non-2xx answer, `Err`
    /// for no answer, an oversized or unparsable body.
    async fn props_facts(&self, t: &Target) -> Result<Option<PropsFacts>, ()> {
        let url = format!(
            "{}/props?model={}",
            t.base_url,
            encode_uri_component(&t.model)
        );
        let r = self
            .fetch
            .get(&url, &[], PROPS_TIMEOUT, Some(PROPS_BODY_CAP))
            .await
            .map_err(|_| ())?;
        if !r.ok() {
            return Ok(None);
        }
        match r.body {
            Some(Ok(text)) => wtf16::parse(&text)
                .map(|b| Some(props_facts_of(&b)))
                .map_err(|_| ()),
            _ => Err(()),
        }
    }

    async fn load_facts(&self, t: &Target, prev: Option<MachineFacts>) -> MachineFacts {
        let mut out = prev.unwrap_or_default();
        let address = address_of(t);
        let (models, props) = tokio::join!(
            self.models_at(address, t.port, address),
            self.props_facts(t)
        );
        if let Ok(ids) = models {
            out.models = Some(ids);
        }
        if let Ok(Some(p)) = props {
            if p.context_tokens.is_some() {
                out.context_tokens = p.context_tokens;
            }
            if p.thinking.is_some() {
                out.thinking = p.thinking;
            }
            if p.vision.is_some() {
                out.vision = p.vision;
            }
        }
        out
    }

    /// core `facts(t)`: cached beside the health result under `<base>#facts#<model>`, so two machines (or this
    /// one) never share it.
    pub async fn facts(&self, t: &Target) -> MachineFacts {
        let Some(health) = &self.health else {
            return self.load_facts(t, None).await;
        };
        let key = format!("{}#facts#{}", t.base_url, t.model);
        health
            .remember(
                &key,
                |prev| async move { Some(self.load_facts(t, prev).await) },
            )
            .await
            .unwrap_or_default()
    }

    /// core `ready(t)`: the health check (shared and cached) and, when it is up, the assigned model.
    pub async fn ready(&self, t: &Target) -> Ready {
        let Some(health) = &self.health else {
            return Ready::Ok;
        };
        let probe = health.probe(&t.base_url, &ProbeOptions::default()).await;
        if probe.state != crate::machine_health::State::Up {
            return ready_of(t, &probe, None);
        }
        let facts = self.facts(t).await;
        ready_of(t, &probe, Some(&facts))
    }

    /// core `contextTokens(t)`: the machine's /props context, the last known on failure, else the floor.
    pub async fn context_tokens(&self, t: &Target) -> u32 {
        self.facts(t)
            .await
            .context_tokens
            .unwrap_or(CONTEXT_FALLBACK)
    }

    /// core `measurements(t)`.
    pub async fn measurements(&self, t: &Target) -> Measurements {
        let f = self.facts(t).await;
        Measurements {
            context_tokens: f.context_tokens,
            thinking: f.thinking,
            vision: f.vision,
            not_measured: NOT_MEASURED_ON_MACHINES,
        }
    }

    /// core `measuredProvider(t)`: the provider row with the machine's measured context and thinking switch.
    pub async fn measured_provider(&self, t: &Target) -> JValue {
        let f = self.facts(t).await;
        provider_row(
            t,
            Some(f.context_tokens.unwrap_or(CONTEXT_FALLBACK)),
            f.thinking,
        )
    }
}
