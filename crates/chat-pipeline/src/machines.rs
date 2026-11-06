//! Chat roles on an Inference machine (core chat-machines.cjs, #1840, #1849, #1851, #1854, #1880): which
//! role runs where, whether its machine can take the turn, and the provider row a turn uses there.
//! The machine list, its health probe and the facts cache (served models, `/props`) are I/O behind
//! [`MachinesIo`].
use crate::text::{as_str, get};
use js_json::JValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// chat-machines.cjs `CHAT_ROLES`, in `ROLE_LABEL` order.
pub const CHAT_ROLES: [&str; 5] = ["fast", "smart", "code", "creative", "vision"];
/// `CONTEXT_FALLBACK`.
pub const CONTEXT_FALLBACK: f64 = 8192.0;

/// `ROLE_LABEL[role]`.
pub fn role_label(role: &str) -> &str {
    match role {
        "fast" => "Fast",
        "smart" => "Smart",
        "code" => "Code",
        "creative" => "Creative",
        "vision" => "Vision",
        other => other,
    }
}

/// `machines.chatTarget(role)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub role: String,
    pub model: String,
    pub port: u16,
    pub base_url: String,
    pub machine_id: String,
    pub machine_name: String,
}

/// `health.probe(baseUrl)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    pub state: Option<String>,
    pub reason: Option<String>,
}

/// The machine's facts (`facts(t)`): each `None` when never read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub models: Option<Vec<String>>,
    pub context_tokens: Option<f64>,
    pub thinking: Option<bool>,
    pub vision: Option<bool>,
}

/// What the module reads.
#[allow(async_fn_in_trait)]
pub trait MachinesIo {
    /// Whether a machine list is configured (`machines`).
    fn has_machines(&self) -> bool;
    /// `machines.chatTarget(role)`; a throw is `None`.
    fn chat_target(&self, role: &str) -> Option<Target>;
    /// Whether a health checker exists (`health.probe`).
    fn has_health(&self) -> bool;
    /// `health.probe(baseUrl)`; a throw is `Err`.
    async fn probe(&self, base_url: &str) -> Result<Health, ()>;
    /// `facts(t)`: the served models and `/props` facts, through the health cache.
    async fn facts(&self, t: &Target) -> Facts;
}

/// `target(role)`: the role's machine, or `None` when it runs here.
pub fn target(io: &impl MachinesIo, role: &str) -> Option<Target> {
    if !io.has_machines() || !CHAT_ROLES.contains(&role) {
        return None;
    }
    io.chat_target(role)
}

/// `remoteRoles()`, in role order.
pub fn remote_roles(io: &impl MachinesIo) -> Vec<(String, Target)> {
    CHAT_ROLES.iter().filter_map(|r| target(io, r).map(|t| ((*r).to_owned(), t))).collect()
}

fn with_roles(roles: &JValue, f: impl Fn(&mut Vec<(String, JValue)>)) -> JValue {
    match roles {
        JValue::Obj(items) => {
            let mut out = items.clone();
            f(&mut out);
            JValue::Obj(out)
        }
        other => other.clone(),
    }
}

/// `effectiveRoles(roles)`: a role on a machine names the model that machine serves.
pub fn effective_roles(io: &impl MachinesIo, roles: &JValue) -> JValue {
    if !roles.truthy() {
        return roles.clone();
    }
    let remote = remote_roles(io);
    if remote.is_empty() {
        return roles.clone();
    }
    with_roles(roles, |out| {
        for (role, t) in &remote {
            js_json::wtf16::set(out, role, JValue::Str(t.model.clone()));
        }
    })
}

/// `localRoles(roles)`: the roles this machine's engine serves.
pub fn local_roles(io: &impl MachinesIo, roles: &JValue) -> JValue {
    if !roles.truthy() {
        return roles.clone();
    }
    let remote = remote_roles(io);
    if remote.is_empty() {
        return roles.clone();
    }
    with_roles(roles, |out| out.retain(|(k, _)| !remote.iter().any(|(r, _)| r == k)))
}

fn unreachable_text(t: &Target, detail: &str) -> String {
    let l = role_label(&t.role);
    format!(
        "{l} model’s machine ({}) is unreachable{}. Check that it is on and serving {} on port {}, or move the {l} role back to this machine in Settings → Inference machines.",
        t.machine_name,
        if detail.is_empty() { String::new() } else { format!(": {detail}") },
        t.model,
        t.port
    )
}

fn wrong_model_text(t: &Target, served: &[String]) -> String {
    let l = role_label(&t.role);
    let first: Vec<&str> = served.iter().take(3).map(String::as_str).collect();
    let list = if first.is_empty() { "no model".to_owned() } else { first.join(", ") };
    format!(
        "The {l} machine ({}) serves {list}{}, not {}. Start {} on port {}, or choose the model it serves for the {l} role in Settings → Inference machines.",
        t.machine_name,
        if served.len() > 3 { " and more" } else { "" },
        t.model,
        t.model,
        t.port
    )
}

/// `ready(t)`'s answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ready {
    Ok,
    /// `{ ok:false, state, code, error }`.
    Not { state: String, code: &'static str, error: String },
}

/// `ready(t)` (#1851): the machine answers its health check and serves the assigned model.
pub async fn ready(io: &impl MachinesIo, t: Option<&Target>) -> Ready {
    let Some(t) = t else { return Ready::Ok };
    if !io.has_health() {
        return Ready::Ok;
    }
    let r = io.probe(&t.base_url).await.unwrap_or(Health { state: Some("unreachable".into()), reason: Some("network".into()) });
    if r.state.as_deref() != Some("up") {
        let reason = r.reason.clone().unwrap_or_default();
        let detail = if r.reason.as_deref() == Some("not-on-this-network") {
            "its address is not on this network".to_owned()
        } else if r.state.as_deref() == Some("down") {
            let why = reason.strip_prefix("http-").map(|s| format!("HTTP {s}")).unwrap_or(reason);
            format!("it answered {why}").trim().to_owned()
        } else if r.reason.as_deref() == Some("timeout") {
            "no answer in time".to_owned()
        } else {
            String::new()
        };
        return Ready::Not {
            state: r.state.filter(|s| !s.is_empty()).unwrap_or_else(|| "unreachable".into()),
            code: "machineUnreachable",
            error: unreachable_text(t, &detail),
        };
    }
    let f = io.facts(t).await;
    if let Some(models) = &f.models {
        if !models.contains(&t.model) {
            return Ready::Not { state: "wrongModel".into(), code: "machineWrongModel", error: wrong_model_text(t, models) };
        }
    }
    Ready::Ok
}

/// `contextTokens(t)`.
pub async fn context_tokens(io: &impl MachinesIo, t: &Target) -> f64 {
    io.facts(t).await.context_tokens.filter(|c| *c != 0.0).unwrap_or(CONTEXT_FALLBACK)
}

/// `provider(t, { contextTokens, thinking })`: the provider row a turn uses for the role on its machine.
pub fn provider(t: &Target, context_tokens: Option<f64>, thinking: Option<bool>) -> JValue {
    let s = |v: &str| JValue::Str(v.to_owned());
    let mut row = vec![
        ("id", JValue::Str(format!("machine:{}", t.machine_id))),
        ("label", JValue::Str(format!("{} model’s machine ({})", role_label(&t.role), t.machine_name))),
        ("baseUrl", JValue::Str(format!("{}/v1", t.base_url))),
        ("apiKey", s("")),
        ("shared", JValue::Bool(true)),
    ];
    if let Some(c) = context_tokens.filter(|c| *c != 0.0 && !c.is_nan()) {
        row.push(("contextTokens", JValue::Num(c)));
    }
    let mut machine = vec![
        ("id", s(&t.machine_id)),
        ("name", s(&t.machine_name)),
        ("role", s(&t.role)),
        ("port", JValue::Num(f64::from(t.port))),
        ("model", s(&t.model)),
    ];
    if thinking == Some(true) {
        machine.push(("thinkingSwitch", JValue::Bool(true)));
    }
    row.push(("machine", JValue::obj(machine)));
    JValue::obj(row)
}

/// `measuredProvider(t)` (#1854).
pub async fn measured_provider(io: &impl MachinesIo, t: &Target) -> JValue {
    let f = io.facts(t).await;
    provider(t, Some(f.context_tokens.filter(|c| *c != 0.0).unwrap_or(CONTEXT_FALLBACK)), f.thinking)
}

/// How a request to a machine failed at the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectFailure {
    /// `EPUBLICADDR`.
    PublicAddress,
    /// The machine answered with a redirect.
    Redirect,
    /// `TimeoutError`.
    Timeout,
    Other,
}

/// `connectError(t, err)`.
pub fn connect_error(t: &Target, why: ConnectFailure) -> String {
    let detail = match why {
        ConnectFailure::PublicAddress => "its address is not on this network",
        ConnectFailure::Redirect => "it answered with a redirect, which is never followed",
        ConnectFailure::Timeout => "no answer in time",
        ConnectFailure::Other => "",
    };
    unreachable_text(t, detail)
}

/// `manualTarget(model, catalogue)` (#1849, #1855): a Manual pick of a model a role's machine serves and this
/// machine's catalogue does not list. `catalogue` is read only when some role's machine serves `model`;
/// `Err` (unreadable) keeps it here.
pub async fn manual_target<F, Fut>(io: &impl MachinesIo, model: &JValue, catalogue: F) -> Option<Target>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<JValue, ()>>,
{
    let model = as_str(model).filter(|m| !m.is_empty())?;
    let matched = remote_roles(io).into_iter().map(|(_, t)| t).find(|t| t.model == model)?;
    let rows = catalogue().await.ok()?;
    if let JValue::Arr(rows) = &rows {
        if rows.iter().any(|m| m.truthy() && as_str(get(m, "name")) == Some(model)) {
            return None;
        }
    }
    Some(matched)
}

/// #1880: chat turns in flight per machine. [`Turns::begin`] counts one until its guard drops (a turn
/// cancelled at any await releases it).
#[derive(Clone, Default)]
pub struct Turns {
    counts: Arc<Mutex<HashMap<String, u64>>>,
}

/// One counted turn; released on drop.
pub struct TurnGuard {
    counts: Arc<Mutex<HashMap<String, u64>>>,
    id: String,
}

impl Turns {
    /// `beginTurn(machineId)`.
    pub fn begin(&self, machine_id: &str) -> TurnGuard {
        *self.counts.lock().unwrap_or_else(PoisonError::into_inner).entry(machine_id.to_owned()).or_default() += 1;
        TurnGuard { counts: Arc::clone(&self.counts), id: machine_id.to_owned() }
    }

    /// `busy(machineId)`.
    pub fn busy(&self, machine_id: &str) -> bool {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner).get(machine_id).copied().unwrap_or(0) > 0
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = counts.get_mut(&self.id) {
            *n -= 1;
            if *n == 0 {
                counts.remove(&self.id);
            }
        }
    }
}

/// `propsFactsOf(body)` (#1854): a llama-server `/props` body's context, thinking switch and image modality.
pub fn props_facts_of(body: &JValue) -> Facts {
    let n = crate::text::js_number(get(get(body, "default_generation_settings"), "n_ctx"));
    let template = get(body, "chat_template");
    let modalities = get(body, "modalities");
    Facts {
        models: None,
        context_tokens: (n.fract() == 0.0 && (2048.0..=2_000_000.0).contains(&n)).then_some(n),
        thinking: as_str(template).filter(|t| !t.is_empty()).map(|t| {
            t.match_indices("enable_thinking").any(|(at, m)| {
                let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                !word(t[..at].chars().next_back()) && !word(t[at + m.len()..].chars().next())
            })
        }),
        vision: if modalities.truthy() { get(modalities, "vision").as_bool() } else { None },
    }
}

/// `modelIds(body)`: the ids a `/v1/models` answer lists (at most 200, distinct, no control characters).
pub fn model_ids(body: &JValue) -> Vec<String> {
    let rows = match (get(body, "data"), get(body, "models")) {
        (JValue::Arr(a), _) | (_, JValue::Arr(a)) => a.as_slice(),
        _ => &[],
    };
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let id = match r {
            JValue::Str(s) => JValue::Str(s.clone()),
            JValue::Obj(_) => [get(r, "id"), get(r, "model"), get(r, "name")].into_iter().find(|v| !v.is_nullish()).cloned().unwrap_or(JValue::Undefined),
            _ => JValue::Undefined,
        };
        if let JValue::Str(id) = id {
            if !crate::text::trim(&id).is_empty()
                && js_json::wtf16::len(&id) <= 200
                && !id.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}')
                && !out.contains(&id)
            {
                out.push(id);
            }
        }
        if out.len() >= 200 {
            break;
        }
    }
    out
}

/// `facts(t)`'s `load(prev)`: the models read (`Some` when `/v1/models` answered and parsed) and the
/// `/props` facts (`Some` when it answered ok), over the last known ones.
pub fn load_facts(prev: Option<&Facts>, models: Option<Vec<String>>, props: Option<Facts>) -> Facts {
    let mut out = prev.cloned().unwrap_or_default();
    if let Some(m) = models {
        out.models = Some(m);
    }
    if let Some(p) = props {
        if let Some(c) = p.context_tokens {
            out.context_tokens = Some(c);
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
