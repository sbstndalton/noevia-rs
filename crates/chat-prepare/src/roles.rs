//! Which model answers a turn, and as which role (slice P5b): noevia-core `server/chat.cjs` from
//! `projectProvider` and `wantsAuto` (~917-926) and from the Automatic roles check to the manual
//! chat-model guard (~1056-1165), with what those lines call: `auto-roles-check.cjs` in full
//! (`missingRoles`, `staleRolesError`, `creativeMoveIsFree`), `route-preload.cjs` `preloadTarget`,
//! `chat-model-kind.cjs` `isChatGenerationModel`, `model-system.cjs` `isSystemModel` (by name), and
//! the resend record of `recordResend`. The classifier is [`crate::router`] and
//! [`crate::system_one`].
//!
//! Pure: [`Routing`] owns chat.cjs's order and asks the caller for every read of a store or a
//! service ([`Seam`]); the caller answers each with a [`Reply`] until [`Step::Done`]:
//! 1. `autoRoles()` for `wantsAuto`, only when the project leaves the turn on Automatic and the
//!    default provider (Node's `&&` reads it last), then `getProvider(id)`.
//! 2. Automatic: `autoRoles()` again (`savedRoles`: none is the 400), `servedCatalogue()` (the 409
//!    for stale roles), `startPreload` of the pre-load target, `classifyFastOrSmart`, the pick and
//!    its resend log entry ([`Step::Done`]'s `resend`, owed even when the pick then refuses).
//! 3. Manual: the project's model; without one on the local engine, `modelsInstalled()` (its error
//!    ignored) and `lastLoadedModel()`.
//! 4. Both: the 400 for no model; for the project's own pick on the local provider,
//!    `servedCatalogue()` again and the 409 for a non-chat model.
//!
//! `codeProject(project)` is a closure ([`Turn::code_project`]), called only when it decides the
//! pre-load, as Node's `likelyCode` reads it. The pre-load (#1863) leaves the caller work on every
//! outcome ([`Preload`]): at the response's finish or close `releaseIfUnused` (chat.cjs ~1106-1112),
//! and when the pick names another model, right after it, `use()` unless the turn was aborted and
//! `cancel()` (~1131).
//!
//! Preconditions (later slices): no routing modes (`routingModes` off, so no `routeTarget`, no cloud
//! route and no `route_pending` question), no Inference machines (`chatMachines` null), and the
//! provider passes the egress and ChatGPT checks chat.cjs runs on the row before step 2 (the caller
//! runs them before it answers [`Seam::Provider`]). The tool gate's move to a tool-capable role
//! (#1550) and the long-context profile (#1079) come after.
//!
//! Model names and role values are JS values (`JValue`), compared as `===` does: two JSON objects
//! are never the same one.

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
    /// The caller answered a [`Seam`] it was not asked, or stepped a finished [`Routing`]: a bug of
    /// the caller, never an answer Node gives.
    OutOfOrder,
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

/// What the routing reads of the turn besides its seams.
#[derive(Clone, Copy)]
pub struct Turn<'a> {
    /// The turn's project record; `null` without one.
    pub project: &'a JValue,
    /// `DEFAULT_PROVIDER_ID`.
    pub default_provider_id: &'a str,
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
    /// chat.cjs `codeProject(project)`: the project shares a Code chat or has Code tasks. Called
    /// only when the pre-load would otherwise go ahead, Code names another model, the message has
    /// no code and it is not a Task turn.
    pub code_project: &'a dyn Fn() -> bool,
    /// `body.resend`.
    pub resend: &'a JValue,
    /// The roles file's section roles (model-system.cjs `sectionRolesOf`: trimmed, lower-cased).
    pub system_roles: &'a HashMap<String, String>,
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
fn preload_target(t: &Turn<'_>, roles: &JValue) -> Option<JValue> {
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
    let likely_code = || router::wants_code(t.message) || t.task_mode || (t.code_project)();
    if code.truthy() && !strict_eq(code, model) && likely_code() {
        return None;
    }
    Some(model.clone())
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

/// The 409 for a stored pick that names an embedding, reranking or system routing model.
/// `catalogue` is `servedCatalogue()`'s answer.
fn chat_guard(
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

/// What the routing asks of its caller next.
#[derive(Debug, Clone, PartialEq)]
pub enum Seam {
    /// `autoRoles()`: the workspace's roles, `null` when none is saved.
    AutoRoles,
    /// `getProvider(id)`: the provider row.
    Provider(JValue),
    /// `servedCatalogue()`: the served list, `null` when the manager is off or unreadable.
    ServedCatalogue,
    /// `startPreload({ manager, model, signal, busy, log })`: whether a pre-load started.
    StartPreload(JValue),
    /// `classifyFastOrSmart(message)`.
    Classify,
    /// `modelsInstalled()`, answered once it settles (an error is ignored).
    ModelsInstalled,
    /// `lastLoadedModel()`.
    LastLoadedModel,
}

/// The caller's answer to the last [`Seam`]; [`Reply::Start`] for the first step.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Start,
    AutoRoles(JValue),
    Provider(JValue),
    Catalogue(JValue),
    Preload { started: bool },
    Classified(Classification),
    Installed,
    LastLoaded(JValue),
}

/// What the caller does with the pre-load (#1863), whatever the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preload {
    NotStarted,
    /// When the response finishes or closes, call its `releaseIfUnused` once (idle: no other chat
    /// turn running or waiting, and the engine not busy).
    Started,
    /// Started, and the pick names another model: right away `use()` it unless the turn was
    /// aborted, then `cancel()`; and `releaseIfUnused` at the end, as for [`Preload::Started`].
    Abandoned,
}

/// The settled route.
#[derive(Debug, Clone, PartialEq)]
pub struct Routed {
    /// `wantsAuto`.
    pub wants_auto: bool,
    /// `projectProvider`.
    pub project_provider: JValue,
    /// The provider row (`provider`).
    pub provider: JValue,
    /// `model`.
    pub model: JValue,
    /// `routedRole` (`None` on a Manual turn).
    pub role: Option<Role>,
    /// `routingDecision`, its `effectiveRole` set to the role (`null` on a Manual turn).
    pub decision: JValue,
}

/// One step of [`Routing`].
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Ask(Seam),
    /// The route, or why the turn ends; and, either way, what the caller still does: the pre-load
    /// and, once an Automatic pick was made, the resend log entry (`recordResend` runs before the
    /// 400 for a pick with no model, so a refused turn can owe it too). The entry is written
    /// before the pre-load's `use()`/`cancel()`.
    Done {
        route: Result<Routed, Halt>,
        preload: Preload,
        /// What `recordResend` writes for a regenerated or retried message (`recordOutcome`'s
        /// entry).
        resend: Option<JValue>,
    },
}

enum Stage {
    Start,
    WantsRoles,
    Provider,
    SavedRoles,
    Catalogue,
    Preload,
    Classify,
    Installed,
    LastLoaded,
    Guard,
    Over,
}

/// chat.cjs's routing of one turn, in its order (see the module doc).
pub struct Routing<'a> {
    t: Turn<'a>,
    stage: Stage,
    wants: bool,
    provider: JValue,
    roles: JValue,
    catalogue: JValue,
    creative_gone: bool,
    preload_model: JValue,
    preload: Preload,
    model: JValue,
    role: Option<Role>,
    decision: JValue,
    resend: Option<JValue>,
}

impl<'a> Routing<'a> {
    pub fn new(t: Turn<'a>) -> Self {
        Routing {
            t,
            stage: Stage::Start,
            wants: false,
            provider: JValue::Null,
            roles: JValue::Null,
            catalogue: JValue::Null,
            creative_gone: false,
            preload_model: JValue::Null,
            preload: Preload::NotStarted,
            model: JValue::Null,
            role: None,
            decision: JValue::Null,
            resend: None,
        }
    }

    /// The next seam, given the answer to the last one. A reply to a seam that was not asked, or a
    /// step after [`Step::Done`], ends with [`Halt::OutOfOrder`].
    pub fn step(&mut self, reply: Reply) -> Step {
        let default = self.t.default_provider_id;
        match (std::mem::replace(&mut self.stage, Stage::Over), reply) {
            (Stage::Start, Reply::Start) => {
                let p = self.t.project;
                let auto = !p.truthy() || p.get("routing").as_str() == Some("auto");
                let provider = project_provider(p, default);
                if auto && (!provider.truthy() || provider.as_str() == Some(default)) {
                    self.ask(Stage::WantsRoles, Seam::AutoRoles)
                } else {
                    self.ask_provider()
                }
            }
            (Stage::WantsRoles, Reply::AutoRoles(roles)) => {
                self.wants = roles.truthy();
                self.ask_provider()
            }
            (Stage::Provider, Reply::Provider(row)) => {
                self.provider = row;
                if self.wants {
                    return self.ask(Stage::SavedRoles, Seam::AutoRoles);
                }
                self.model = manual_model(self.t.project);
                if !self.model.truthy() && self.on_default() && self.t.manager_enabled {
                    self.ask(Stage::Installed, Seam::ModelsInstalled)
                } else {
                    self.settle()
                }
            }
            (Stage::SavedRoles, Reply::AutoRoles(roles)) => {
                if !roles.truthy() {
                    return self.halt(Halt::Refused(Refusal {
                        status: 400,
                        error: "Auto routing is not configured yet — pick Fast and Smart models in the model popup first.".into(),
                    }));
                }
                self.roles = roles;
                self.ask(Stage::Catalogue, Seam::ServedCatalogue)
            }
            (Stage::Catalogue, Reply::Catalogue(catalogue)) => {
                self.catalogue = catalogue;
                match self.check() {
                    Err(h) => self.halt(h),
                    Ok(Some(model)) => {
                        self.preload_model = model.clone();
                        self.ask(Stage::Preload, Seam::StartPreload(model))
                    }
                    Ok(None) => self.ask(Stage::Classify, Seam::Classify),
                }
            }
            (Stage::Preload, Reply::Preload { started }) => {
                if started {
                    self.preload = Preload::Started;
                }
                self.ask(Stage::Classify, Seam::Classify)
            }
            (Stage::Classify, Reply::Classified(c)) => match self.pick(&c) {
                Err(Thrown) => self.halt(Halt::Thrown),
                Ok(()) => self.settle(),
            },
            (Stage::Installed, Reply::Installed) => {
                self.ask(Stage::LastLoaded, Seam::LastLoadedModel)
            }
            (Stage::LastLoaded, Reply::LastLoaded(model)) => {
                self.model = model;
                self.settle()
            }
            (Stage::Guard, Reply::Catalogue(catalogue)) => {
                match chat_guard(&self.model, &catalogue, self.t.system_roles) {
                    Err(h) => self.halt(h),
                    Ok(()) => self.done(),
                }
            }
            _ => self.halt(Halt::OutOfOrder),
        }
    }

    fn ask(&mut self, stage: Stage, seam: Seam) -> Step {
        self.stage = stage;
        Step::Ask(seam)
    }

    /// `getProvider(wantsAuto ? DEFAULT_PROVIDER_ID : projectProvider || DEFAULT_PROVIDER_ID)`.
    fn ask_provider(&mut self) -> Step {
        let provider = project_provider(self.t.project, self.t.default_provider_id);
        let id = if self.wants || !provider.truthy() {
            JValue::from(self.t.default_provider_id)
        } else {
            provider
        };
        self.ask(Stage::Provider, Seam::Provider(id))
    }

    fn on_default(&self) -> bool {
        is_default(&self.provider, self.t.default_provider_id)
    }

    /// The stale-roles check (Creative is optional: missing, the router's pick stands) and the
    /// pre-load target.
    fn check(&mut self) -> Result<Option<JValue>, Halt> {
        let missing = missing_roles(&self.roles, &self.catalogue)?;
        self.creative_gone = missing.iter().any(|(r, _)| *r == Role::Creative);
        let others: Vec<(Role, JValue)> = missing
            .into_iter()
            .filter(|(r, _)| *r != Role::Creative)
            .collect();
        if let Some(error) = stale_roles_error(&others)? {
            return Err(Halt::Refused(Refusal { status: 409, error }));
        }
        let local = !self.t.compact_only && self.on_default() && self.t.manager_enabled;
        Ok(if local {
            preload_target(&self.t, &self.roles)
        } else {
            None
        })
    }

    /// The pick: a role with no model falls back to Smart (else Fast); a creative prompt moves to
    /// the Creative role when it is served and the move costs no model swap.
    fn pick(&mut self, c: &Classification) -> Result<(), Thrown> {
        let roles = &self.roles;
        let mut role = c.role;
        if !roles.get(role.as_str()).truthy() {
            role = if roles.get("smart").truthy() {
                Role::Smart
            } else {
                Role::Fast
            };
        }
        if !self.creative_gone
            && sampling::creative_route(Some(role.as_str()), self.t.message, roles)
            && creative_move_is_free(roles, role, &self.catalogue)?
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
        self.model = roles.get(role.as_str()).clone();
        self.resend = resend_record(self.t.resend, role, status);
        self.role = Some(role);
        self.decision = decision;
        if self.preload == Preload::Started && !strict_eq(&self.preload_model, &self.model) {
            self.preload = Preload::Abandoned;
        }
        Ok(())
    }

    /// The 400 for a turn with no model, then the manual guard's catalogue read (#409): a Manual
    /// turn answering the project's own stored model on the local provider. Node's
    /// `project.model === model` is true for the project's own value even when it is an object
    /// (the same reference), and a loaded model is never an object, so values are compared.
    fn settle(&mut self) -> Step {
        if !self.model.truthy() {
            let error = if self.on_default() {
                "no model selected and none loaded — pick one in the model popup".to_string()
            } else {
                let label = self.provider.get("label");
                let name = if label.truthy() {
                    label
                } else {
                    self.provider.get("id")
                };
                match js_json::to_js_string(name) {
                    Ok(name) => {
                        format!("no model selected for {name} — pick one in the model popup")
                    }
                    Err(_) => return self.halt(Halt::Thrown),
                }
            };
            return self.halt(Halt::Refused(Refusal { status: 400, error }));
        }
        let project = self.t.project;
        if !self.wants
            && project.truthy()
            && project.get("model") == &self.model
            && self.on_default()
        {
            return self.ask(Stage::Guard, Seam::ServedCatalogue);
        }
        self.done()
    }

    fn done(&mut self) -> Step {
        Step::Done {
            route: Ok(Routed {
                wants_auto: self.wants,
                project_provider: project_provider(self.t.project, self.t.default_provider_id),
                provider: self.provider.clone(),
                model: self.model.clone(),
                role: self.role,
                decision: self.decision.clone(),
            }),
            preload: self.preload,
            resend: self.resend.clone(),
        }
    }

    fn halt(&mut self, h: Halt) -> Step {
        self.stage = Stage::Over;
        Step::Done {
            route: Err(h),
            preload: self.preload,
            resend: self.resend.clone(),
        }
    }
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
