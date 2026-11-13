//! Which model answers a turn, and as which role (slice P5b): noevia-core `server/chat.cjs` from
//! `projectProvider` and `wantsAuto` (~917-926) and from the Automatic roles check to the manual
//! chat-model guard (~1056-1165), with what those lines call: `auto-roles-check.cjs` in full
//! (`missingRoles`, `staleRolesError`, `creativeMoveIsFree`), `route-preload.cjs` `preloadTarget`,
//! `chat-model-kind.cjs` `isChatGenerationModel`, `model-system.cjs` `isSystemModel` (by name), and
//! the resend record of `recordResend`. The classifier is [`crate::router`] and
//! [`crate::system_one`].
//!
//! Pure: every read of a store or a service is a seam the caller answers, in chat.cjs's order:
//! 1. [`wants_auto`], then the provider row of [`provider_id`] (`getProvider`).
//! 2. Automatic: `servedCatalogue()` (null when the manager is off or unreadable), [`auto_check`];
//!    the pre-load of [`AutoCheck::preload`] (`startPreload`), the classification
//!    (`classifyFastOrSmart`), then [`auto_pick`].
//! 3. Manual: [`manual_model`]; when [`wants_loaded_model`], `modelsInstalled()` (its error ignored)
//!    and `lastLoadedModel()` as the model.
//! 4. Both: [`require_model`]; when [`chat_guard_applies`], `servedCatalogue()` again and
//!    [`chat_guard`].
//!
//! Preconditions (later slices): no routing modes (`routingModes` off, so no `routeTarget`, no cloud
//! route and no `route_pending` question), no Inference machines (`chatMachines` null), and the
//! provider passes the egress and ChatGPT checks chat.cjs runs between steps 1 and 2. The tool
//! gate's move to a tool-capable role (#1550) and the long-context profile (#1079) come after.
//!
//! Model names and role values are JS values (`JValue`), compared as `===` does: two JSON objects
//! are never the same one. `autoRoles()` is read once: Node reads it again at each use, and gets the
//! same workspace's value within a turn.

use std::collections::HashMap;

use js_json::{wtf16, JValue};

use crate::{js, router, sampling, Thrown};

/// A role of `auto-roles-check.cjs` `LABELS`, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Fast,
    Smart,
    Vision,
    Code,
    Creative,
}

impl Role {
    pub const ALL: [Role; 5] = [
        Role::Fast,
        Role::Smart,
        Role::Vision,
        Role::Code,
        Role::Creative,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Fast => "fast",
            Role::Smart => "smart",
            Role::Vision => "vision",
            Role::Code => "code",
            Role::Creative => "creative",
        }
    }

    /// `LABELS[role]`.
    pub fn label(self) -> &'static str {
        match self {
            Role::Fast => "Fast",
            Role::Smart => "Smart",
            Role::Vision => "Vision",
            Role::Code => "Code",
            Role::Creative => "Creative",
        }
    }

    pub fn of(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

/// A JSON answer chat.cjs sends instead of the turn (`json(res, status, { error })`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub error: String,
}

impl Refusal {
    /// The answer's body, `{ error }`.
    pub fn body(&self) -> JValue {
        JValue::obj([("error", JValue::from(self.error.as_str()))])
    }
}

/// Why the routing ends the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    Refused(Refusal),
    /// Node throws (a `String()` of an object with its own `toString`, a property read of `null`),
    /// and the turn fails as an uncaught error does.
    Thrown,
}

impl From<Thrown> for Halt {
    fn from(_: Thrown) -> Self {
        Halt::Thrown
    }
}

/// `classifyFastOrSmart(message)`'s answer: `role` and `routingDecision` (`null` for a classifier
/// that answers a bare role string).
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub role: Role,
    pub decision: JValue,
}

/// What the routing reads of the turn.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    /// The turn's project record; `null` without one.
    pub project: &'a JValue,
    /// `autoRoles()`: the workspace's role object, `null` when none is saved.
    pub auto_roles: &'a JValue,
    /// `DEFAULT_PROVIDER_ID`.
    pub default_provider_id: &'a str,
    /// The provider row (`getProvider(provider_id(..))`).
    pub provider: &'a JValue,
    /// `body.message`.
    pub message: &'a str,
    /// `modelManager.enabled` (truthy).
    pub manager_enabled: bool,
    /// `body.compactOnly` (truthy).
    pub compact_only: bool,
    /// `body.mode === 'cowork'` (a Task turn).
    pub task_mode: bool,
    /// The turn carries images: attached or retrieved ones, or a message whose content is not text.
    pub has_images: bool,
    /// chat.cjs `codeProject(project)`: the project shares a Code chat or has Code tasks.
    pub code_project: bool,
}

/// JS `===` over JSON values (SameValueZero, as `Set.has` and `includes`, for the same values).
pub fn strict_eq(a: &JValue, b: &JValue) -> bool {
    !matches!(a, JValue::Obj(_) | JValue::Arr(_)) && a == b
}

fn is_default(provider: &JValue, default: &str) -> bool {
    provider.get("id").as_str() == Some(default)
}

/// `projectProvider`: the project's provider, `'lemonade'` read as the default one.
pub fn project_provider(project: &JValue, default: &str) -> JValue {
    match project.get("provider") {
        JValue::Str(s) if s == "lemonade" => JValue::from(default),
        other => other.clone(),
    }
}

/// `wantsAuto`: Automatic is the default (a free chat without its context project too), on the
/// default provider only, and only with roles saved.
pub fn wants_auto(project: &JValue, auto_roles: &JValue, default: &str) -> bool {
    let routing = !project.truthy() || project.get("routing").as_str() == Some("auto");
    let provider = project_provider(project, default);
    routing && (!provider.truthy() || provider.as_str() == Some(default)) && auto_roles.truthy()
}

/// The id chat.cjs asks `getProvider` for: `wantsAuto ? DEFAULT_PROVIDER_ID : projectProvider || DEFAULT_PROVIDER_ID`.
pub fn provider_id(wants_auto: bool, project: &JValue, default: &str) -> JValue {
    let provider = project_provider(project, default);
    if wants_auto || !provider.truthy() {
        JValue::from(default)
    } else {
        provider
    }
}

/// `missingRoles(roles, installed)`: the roles whose model `installed` does not serve (a row with
/// `missingFile` serves nothing). An unknown catalogue (not a list) reports nothing.
pub fn missing_roles(roles: &JValue, installed: &JValue) -> Result<Vec<(Role, JValue)>, Thrown> {
    let (true, JValue::Arr(rows)) = (roles.truthy(), installed) else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for m in rows {
        if m.is_nullish() {
            return Err(Thrown);
        }
        if !m.get("missingFile").truthy() {
            names.push(m.get("name"));
        }
    }
    Ok(Role::ALL
        .into_iter()
        .filter_map(|role| {
            let model = roles.get(role.as_str());
            (model.truthy() && !names.iter().any(|n| strict_eq(n, model)))
                .then(|| (role, model.clone()))
        })
        .collect())
}

/// `staleRolesError(missing)`.
pub fn stale_roles_error(missing: &[(Role, JValue)]) -> Result<Option<String>, Thrown> {
    if missing.is_empty() {
        return Ok(None);
    }
    let mut list = Vec::with_capacity(missing.len());
    for (role, model) in missing {
        let model = js_json::to_js_string(model).map_err(|_| Thrown)?;
        list.push(format!("{} ({model})", role.label()));
    }
    Ok(Some(format!(
        "Auto routing uses models that are no longer installed: {}. Choose new ones in Settings → Models & routing.",
        list.join(", ")
    )))
}

/// `/^(embedding|embeddings|rerank|reranking|reranker)$/i.test(label)` (`NOT_CHAT`, `NON_CHAT_LABEL`):
/// ASCII case folding only (a non-Unicode JS regex).
fn not_chat_label(label: &JValue) -> Result<bool, Thrown> {
    let text = js_json::to_js_string(label).map_err(|_| Thrown)?;
    Ok(
        ["embedding", "embeddings", "rerank", "reranking", "reranker"]
            .iter()
            .any(|w| text.eq_ignore_ascii_case(w)),
    )
}

/// `creativeMoveIsFree(roles, routedRole, installed)`: the Creative model is the routed role's, is
/// loaded, or no chat model is loaded. An unknown catalogue keeps the router's pick (false).
pub fn creative_move_is_free(
    roles: &JValue,
    routed: Role,
    installed: &JValue,
) -> Result<bool, Thrown> {
    let creative = roles.get("creative");
    let (true, JValue::Arr(rows)) = (creative.truthy(), installed) else {
        return Ok(false);
    };
    if strict_eq(roles.get(routed.as_str()), creative) {
        return Ok(true);
    }
    let mut loaded = Vec::new();
    for m in rows {
        if !m.truthy()
            || !m.get("loaded").truthy()
            || ["system", "servedElsewhere", "sidecarProtected"]
                .iter()
                .any(|k| m.get(k).truthy())
        {
            continue;
        }
        // `(m.labels || []).some(...)`: a label list that is not an array has no `some` (TypeError).
        let not_chat = match m.get("labels") {
            JValue::Arr(labels) => {
                let mut hit = false;
                for l in labels {
                    if not_chat_label(l)? {
                        hit = true;
                        break;
                    }
                }
                hit
            }
            other if other.truthy() => return Err(Thrown),
            _ => false,
        };
        if !not_chat {
            loaded.push(m.get("name"));
        }
    }
    Ok(loaded.is_empty() || loaded.iter().any(|n| strict_eq(n, creative)))
}

/// `route-preload.cjs` `preloadTarget(...)` as chat.cjs calls it (no role on an Inference machine):
/// the one local model every role this turn could end on names, or `None`.
fn preload_target(t: &Turn<'_>) -> Option<JValue> {
    let roles = t.auto_roles;
    let context_high = t.project.get("contextProfile").as_str() == Some("high");
    if !roles.truthy() || t.has_images || context_high {
        return None;
    }
    let (fast, smart) = (roles.get("fast"), roles.get("smart"));
    let model = if fast.truthy() { fast } else { smart };
    if !model.truthy() || (fast.truthy() && smart.truthy() && !strict_eq(fast, smart)) {
        return None;
    }
    let creative = roles.get("creative");
    if creative.truthy() && !strict_eq(creative, model) {
        return None;
    }
    let code = roles.get("code");
    let likely_code = || router::wants_code(t.message) || t.task_mode || t.code_project;
    if code.truthy() && !strict_eq(code, model) && likely_code() {
        return None;
    }
    Some(model.clone())
}

/// What the roles check settled before the classifier runs.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoCheck {
    /// The Creative role's model is not served: it is optional, so it never fails the turn, and the
    /// router's pick then stands.
    pub creative_gone: bool,
    /// The model to load beside the decision (#1863), for the caller's `startPreload`; `None`: no
    /// pre-load.
    pub preload: Option<JValue>,
}

/// The Automatic roles check: 400 without roles, 409 when a role other than Creative names a model
/// the catalogue does not serve; `catalogue` is `servedCatalogue()`'s answer (null: unknown).
pub fn auto_check(t: &Turn<'_>, catalogue: &JValue) -> Result<AutoCheck, Halt> {
    if !t.auto_roles.truthy() {
        return Err(Halt::Refused(Refusal {
            status: 400,
            error: "Auto routing is not configured yet — pick Fast and Smart models in the model popup first.".into(),
        }));
    }
    let missing = missing_roles(t.auto_roles, catalogue)?;
    let creative_gone = missing.iter().any(|(r, _)| *r == Role::Creative);
    let others: Vec<(Role, JValue)> = missing
        .into_iter()
        .filter(|(r, _)| *r != Role::Creative)
        .collect();
    if let Some(error) = stale_roles_error(&others)? {
        return Err(Halt::Refused(Refusal { status: 409, error }));
    }
    let local =
        !t.compact_only && is_default(t.provider, t.default_provider_id) && t.manager_enabled;
    Ok(AutoCheck {
        creative_gone,
        preload: if local { preload_target(t) } else { None },
    })
}

/// An Automatic turn's route.
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    /// `routedRole`.
    pub role: Role,
    /// `model`: the role's model (`roles[routedRole]`).
    pub model: JValue,
    /// `routingDecision`, its `effectiveRole` set to the role (as the classifier gave it otherwise).
    pub decision: JValue,
    /// What `recordResend` writes for a regenerated or retried message (`recordOutcome`'s entry), or
    /// `None`.
    pub resend: Option<JValue>,
}

/// The Automatic pick after the classification: a role with no model falls back to Smart (else
/// Fast); a creative prompt moves to the Creative role when one is served and the move costs no
/// model swap. `resend` is `body.resend`.
pub fn auto_pick(
    t: &Turn<'_>,
    catalogue: &JValue,
    check: &AutoCheck,
    c: &Classification,
    resend: &JValue,
) -> Result<Picked, Thrown> {
    let roles = t.auto_roles;
    let mut role = c.role;
    if !roles.get(role.as_str()).truthy() {
        role = if roles.get("smart").truthy() {
            Role::Smart
        } else {
            Role::Fast
        };
    }
    if !check.creative_gone
        && sampling::creative_route(Some(role.as_str()), t.message, roles)
        && creative_move_is_free(roles, role, catalogue)?
    {
        role = Role::Creative;
    }
    let mut decision = c.decision.clone();
    if let JValue::Obj(items) = &mut decision {
        wtf16::set(items, "effectiveRole", JValue::from(role.as_str()));
    }
    let status = match decision.get("status") {
        s if s.truthy() => s.clone(),
        _ => JValue::Null,
    };
    Ok(Picked {
        role,
        model: roles.get(role.as_str()).clone(),
        decision,
        resend: resend_record(resend, role, status),
    })
}

/// `recordResend(resend, { auto: true, role, status })`: only a regenerate or a retry is recorded,
/// with only the enumerated previous role and status.
fn resend_record(resend: &JValue, role: Role, status: JValue) -> Option<JValue> {
    let kind = resend
        .get("kind")
        .as_str()
        .filter(|k| matches!(*k, "regenerate" | "retry"))?;
    let of = |key: &str, allowed: &[&str]| match resend.get(key).as_str() {
        Some(v) if allowed.contains(&v) => JValue::from(v),
        _ => JValue::Null,
    };
    Some(JValue::obj([
        ("event", JValue::from(kind)),
        ("previousRole", of("role", &["fast", "smart", "code"])),
        ("previousStatus", of("status", &["accepted", "fallback"])),
        ("auto", JValue::Bool(true)),
        ("role", JValue::from(role.as_str())),
        ("status", status),
    ]))
}

/// `(project && project.model) || null`: the manual pick.
pub fn manual_model(project: &JValue) -> JValue {
    match project.get("model") {
        m if m.truthy() => m.clone(),
        _ => JValue::Null,
    }
}

/// A Manual turn with no pick on the local engine takes the model the manager last reported as
/// loaded: the caller reads `modelsInstalled()` (ignoring its error), then `lastLoadedModel()`.
pub fn wants_loaded_model(t: &Turn<'_>, model: &JValue) -> bool {
    !model.truthy() && is_default(t.provider, t.default_provider_id) && t.manager_enabled
}

/// The 400 for a turn with no model.
pub fn require_model(t: &Turn<'_>, model: &JValue) -> Result<(), Halt> {
    if model.truthy() {
        return Ok(());
    }
    let error = if is_default(t.provider, t.default_provider_id) {
        "no model selected and none loaded — pick one in the model popup".to_string()
    } else {
        let label = t.provider.get("label");
        let name = if label.truthy() {
            label
        } else {
            t.provider.get("id")
        };
        let name = js_json::to_js_string(name).map_err(|_| Halt::Thrown)?;
        format!("no model selected for {name} — pick one in the model popup")
    };
    Err(Halt::Refused(Refusal { status: 400, error }))
}

/// Whether the manual pick is checked against the catalogue (#409): a Manual turn answering the
/// project's own stored model on the local provider. `model` is [`manual_model`]'s or the loaded
/// model: Node's `project.model === model` is true for the project's own value even when it is an
/// object (the same reference), and a loaded model is never an object, so values are compared.
pub fn chat_guard_applies(t: &Turn<'_>, wants_auto: bool, model: &JValue) -> bool {
    !wants_auto
        && t.project.truthy()
        && t.project.get("model") == model
        && is_default(t.provider, t.default_provider_id)
}

/// The 409 for a stored pick that names an embedding, reranking or system routing model.
/// `catalogue` is `servedCatalogue()`'s answer; `system_roles` the roles file's section roles
/// (model-system.cjs `sectionRolesOf`: trimmed, lower-cased).
pub fn chat_guard(
    model: &JValue,
    catalogue: &JValue,
    system_roles: &HashMap<String, String>,
) -> Result<(), Halt> {
    let mut labels = &JValue::Undefined;
    if let JValue::Arr(rows) = catalogue {
        for m in rows {
            if m.is_nullish() {
                return Err(Halt::Thrown);
            }
            if strict_eq(m.get("name"), model) {
                labels = m.get("labels");
                break;
            }
        }
    }
    if is_chat_generation_model(model, labels, system_roles)? {
        return Ok(());
    }
    let name = js_json::to_js_string(model).map_err(|_| Halt::Thrown)?;
    Err(Halt::Refused(Refusal {
        status: 409,
        error: format!("{name} is an embedding, reranking or routing model and cannot answer chat messages — pick a chat model in the model popup."),
    }))
}

/// `chat-model-kind.cjs` `isChatGenerationModel(name, labels)`. A copy of `noevia-server`'s
/// `model_kind` (a bin this crate cannot depend on) over JS values, with JS trimming and ASCII-only
/// case folding (its regexes are non-Unicode `/i`).
pub fn is_chat_generation_model(
    name: &JValue,
    labels: &JValue,
    system_roles: &HashMap<String, String>,
) -> Result<bool, Thrown> {
    let n = if name.truthy() {
        js_json::to_js_string(name).map_err(|_| Thrown)?
    } else {
        String::new()
    };
    if n.is_empty() || is_system_model(&n, system_roles) {
        return Ok(false);
    }
    let lower = n.to_ascii_lowercase();
    if lower.contains("embed") || lower.contains("rerank") {
        return Ok(false);
    }
    if let JValue::Arr(labels) = labels {
        for l in labels {
            if not_chat_label(l)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// `model-system.cjs` `isSystemModel(id)` (no model path): the roles file's explicit role, else the
/// deprecated Laya name rule (`/^laya(?:[_.-]|$)/i` on the trimmed name).
pub fn is_system_model(id: &str, system_roles: &HashMap<String, String>) -> bool {
    let key = js::trim(id);
    if let Some(role) = (!key.is_empty()).then(|| system_roles.get(key)).flatten() {
        return role == "system-routing" || role == "laya";
    }
    key.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("laya"))
        && key
            .get(4..)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['_', '.', '-']))
}
