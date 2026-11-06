//! Where a turn goes, or why it is refused (core chat.cjs:900-1150, `handleChatInner`): routing
//! modes (#778: the cloud check, the sensitive-content card, the remembered choice), the provider,
//! the external-provider rules (#447), the ChatGPT connection, the Auto roles (#343, #1816) and the
//! model the turn answers with (#848, #409).
//!
//! The awaits of that section are the caller's [`RouteIo`]: the served catalogue, the sensitivity
//! check and the person's answer to the card. Dropping [`decide`]'s future at any of them leaves
//! nothing behind (it holds no lock or permit), and the events sent so far are in [`Decided::events`]
//! only once it has returned.
//!
//! The classifiers this section calls (`rm.preRule`, `classifyTaskForSampling`, the auto-router)
//! are taken as their verdicts in [`Turn`]; machine assignments (`chatMachines`) and the #1863
//! pre-load are not part of this stage (a turn with machines stays on Node until they are).
use js_json::JValue;

/// A provider row (`getProvider(id)`), as far as this section reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: String,
    pub label: Option<String>,
    pub kind: Option<String>,
    /// `provider.external === true`.
    pub external: bool,
    pub shared: bool,
    pub base_url: String,
}

/// The routing mode's settings for this chat (`routingModes.settings(project)`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// `local`, `cloud` or `hybrid`; `None` or empty: no mode chosen.
    pub mode: Option<String>,
    /// `None`: `resolveRoute`'s default, `ask`.
    pub when_sensitive: Option<String>,
    pub cloud: Cloud,
}

/// `settings.cloud`: the cloud provider and a model per role.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cloud {
    pub provider_id: Option<String>,
    pub fast: Option<String>,
    pub smart: Option<String>,
    pub code: Option<String>,
}

impl Cloud {
    /// routing-modes.cjs `cloudModel(cloud, role)`: smart → fast → code fallback.
    pub fn model(&self, role: &str) -> String {
        let own = match role {
            "fast" => &self.fast,
            "smart" => &self.smart,
            "code" => &self.code,
            _ => &None,
        };
        [own, &self.smart, &self.fast, &self.code]
            .into_iter()
            .flatten()
            .find(|m| !m.is_empty())
            .cloned()
            .unwrap_or_default()
    }
}

/// The saved Auto roles (`autoRoles()`): role → model name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub fast: Option<String>,
    pub smart: Option<String>,
    pub vision: Option<String>,
    pub code: Option<String>,
    pub creative: Option<String>,
}

const LABELS: [(&str, &str); 5] = [
    ("fast", "Fast"),
    ("smart", "Smart"),
    ("vision", "Vision"),
    ("code", "Code"),
    ("creative", "Creative"),
];

impl Roles {
    /// `roles[role]` when it is a non-empty name.
    pub fn get(&self, role: &str) -> Option<&str> {
        match role {
            "fast" => self.fast.as_deref(),
            "smart" => self.smart.as_deref(),
            "vision" => self.vision.as_deref(),
            "code" => self.code.as_deref(),
            "creative" => self.creative.as_deref(),
            _ => None,
        }
        .filter(|m| !m.is_empty())
    }
}

/// A served catalogue row (`servedCatalogue()`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub labels: Vec<String>,
    pub missing_file: bool,
    pub loaded: bool,
    pub system: bool,
    pub served_elsewhere: bool,
    pub sidecar_protected: bool,
}

/// auto-roles-check.cjs `missingRoles(roles, installed)`: roles whose model is not served.
pub fn missing_roles(roles: &Roles, installed: Option<&[Entry]>) -> Vec<(&'static str, String)> {
    let Some(installed) = installed else {
        return Vec::new();
    };
    LABELS
        .iter()
        .filter_map(|(role, _)| {
            let m = roles.get(role)?;
            let served = installed.iter().any(|e| !e.missing_file && e.name == m);
            (!served).then(|| (*role, m.to_owned()))
        })
        .collect()
}

/// auto-roles-check.cjs `staleRolesError(missing)`.
pub fn stale_roles_error(missing: &[(&str, String)]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let list: Vec<String> = missing
        .iter()
        .map(|(role, model)| {
            let label = LABELS.iter().find(|(r, _)| r == role).map_or("", |(_, l)| l);
            format!("{label} ({model})")
        })
        .collect();
    Some(format!(
        "Auto routing uses models that are no longer installed: {}. Choose new ones in Settings → Models & routing.",
        list.join(", ")
    ))
}

fn not_chat_label(l: &str) -> bool {
    matches!(
        l.to_ascii_lowercase().as_str(),
        "embedding" | "embeddings" | "rerank" | "reranking" | "reranker"
    )
}

/// auto-roles-check.cjs `creativeMoveIsFree(roles, routedRole, installed)`.
fn creative_move_is_free(roles: &Roles, routed: &str, installed: Option<&[Entry]>) -> bool {
    let (Some(creative), Some(installed)) = (roles.get("creative"), installed) else {
        return false;
    };
    if roles.get(routed) == Some(creative) {
        return true;
    }
    let loaded: Vec<&str> = installed
        .iter()
        .filter(|m| {
            m.loaded
                && !m.system
                && !m.served_elsewhere
                && !m.sidecar_protected
                && !m.labels.iter().any(|l| not_chat_label(l))
        })
        .map(|m| m.name.as_str())
        .collect();
    loaded.is_empty() || loaded.contains(&creative)
}

/// chat-model-kind.cjs `isChatGenerationModel(name, labels)`; `system` is model-system.cjs
/// `isSystemModel(name)` (the caller's roles file).
pub fn is_chat_generation_model(name: &str, labels: &[String], system: bool) -> bool {
    if name.is_empty() || system {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    if lower.contains("embed") || lower.contains("rerank") {
        return false;
    }
    !labels.iter().any(|l| not_chat_label(l))
}

/// The sensitivity check's verdict (`routingModes.sensitivity(...)`); `None`: it gave nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub flagged: Option<bool>,
    pub flag: Option<String>,
    pub cause: Option<String>,
}

/// The person's answer to the sensitive-content card (`routingModes.awaitChoice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub choice: Option<String>,
    pub remember: bool,
}

/// The ChatGPT connection of the account (`chatgptOAuth.status(owner).state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGpt {
    /// The server's ChatGPT feature (`chatgptOAuth && chatgptEnabled()`).
    pub enabled: bool,
    pub state: String,
}

/// What this section reads of the turn.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    /// The resolved project (`project`), or `None`.
    pub project: Option<Project>,
    /// `body.projectId` was given.
    pub body_project: bool,
    pub chat_id: Option<String>,
    pub space_id: JValue,
    pub compact_only: bool,
    pub default_provider: String,
    pub diary_project_id: String,
    /// `autoRoles()` at the Auto decision, and at every later read (a save may land between).
    pub roles_first: Option<Roles>,
    pub roles_later: Option<Roles>,
    /// `routingModes` is wired and `enabled() === true`, with these settings.
    pub routing_modes: Option<Option<Settings>>,
    /// The chat's list entry flags (`rm.chatFlags`).
    pub force_local: bool,
    pub allow_cloud: bool,
    /// `rm.preRule` of what the router reads (hybrid only).
    pub pre_flag: Option<String>,
    /// The auto-router's role (`classifyFastOrSmart`).
    pub router_role: String,
    /// The routed roles for which `classifyTaskForSampling({routedRole, message}) === 'creative'`.
    pub creative_for: Vec<String>,
    pub manager_enabled: bool,
    /// `lastLoadedModel()` after `modelsInstalled()`.
    pub last_loaded: Option<String>,
    /// `requestScope` workspace user (the card needs one).
    pub route_user: Option<String>,
    /// `authn.user.id`.
    pub owner: Option<String>,
    /// `currentWorkspace().userId` (`chatWorkspace`).
    pub workspace_user: Option<String>,
    /// `None`: no ChatGPT connection support wired (`!chatgptOAuth`).
    pub chatgpt: Option<ChatGpt>,
    /// model-system.cjs `isSystemModel(name)`.
    pub system_model: fn(&str) -> bool,
}

/// The project fields this section reads.
#[derive(Clone, Debug, Default)]
pub struct Project {
    pub id: String,
    pub routing: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// The section's awaits.
#[allow(async_fn_in_trait)]
pub trait RouteIo {
    /// `getProvider(id)`.
    fn provider(&self, id: &str) -> Option<ProviderRow>;
    /// `servedCatalogue()`; `None` when unreadable.
    async fn catalogue(&self) -> Option<Vec<Entry>>;
    /// `routingModes.sensitivity(...)`.
    async fn check(&self) -> Option<Verdict>;
    /// `routingModes.openChoice`: the card is registered at the front's registry (S9) before it is
    /// shown; `false` when it could not be.
    async fn open_choice(&self) -> bool;
    /// `routingModes.awaitChoice`, after `route_pending` went out; `None`: no answer (it threw).
    async fn await_choice(&self) -> Option<Answer>;
    /// `chatSignal.signal.aborted` after the route was decided.
    fn aborted(&self) -> bool;
}

/// How the section ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `json(res, status, {error})`: a plain answer, or (`streamed`) an `error` event on the stream
    /// the card opened (`{type:'error', text}`, then the end).
    Refused { status: u16, error: String, streamed: bool },
    /// The client went away while the route was decided: the stream (if any) ends, nothing more.
    Aborted,
    /// Node throws here (a provider id with no row: `provider.id` of `null`): the turn's unhandled error.
    Thrown,
    /// The turn goes on with this provider and model.
    Go(Go),
}

/// Where the turn goes on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Go {
    pub provider: ProviderRow,
    pub model: String,
    /// The Auto role that answers (`routedRole`), if Auto.
    pub role: Option<String>,
    /// The routing decision (`routeTarget.route`, `.reason`), if routing modes decided one.
    pub route: Option<(String, String)>,
    /// #1646: a hybrid cloud route the person did not grant.
    pub hybrid_ungranted: bool,
}

/// What [`decide`] sent and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decided {
    /// The events sent on the stream the card opened (`route_pending`, `route_remembered`), in order.
    /// `route_pending` carries no id here: the caller's registry names it.
    pub events: Vec<JValue>,
    /// The card opened the event stream (`earlyStream`).
    pub streamed: bool,
    pub outcome: Outcome,
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

struct Route {
    route: Option<&'static str>,
    reason: String,
    remember: Option<&'static str>,
}

/// routing-modes.cjs `resolveRoute(...)`.
async fn resolve_route(
    io: &impl RouteIo,
    events: &mut Vec<JValue>,
    streamed: &mut bool,
    a: &ResolveArgs<'_>,
) -> Route {
    let local = |reason: &str| Route {
        route: a.has_local.then_some("local"),
        reason: reason.to_owned(),
        remember: None,
    };
    if a.hard_local {
        return local("sensitive-rule");
    }
    if a.force_local {
        return local("force-local");
    }
    if a.mode == "local" || !a.has_cloud {
        return local("mode");
    }
    let cloud = |reason: &str| Route {
        route: Some("cloud"),
        reason: reason.to_owned(),
        remember: None,
    };
    if a.mode == "cloud" {
        return cloud("mode");
    }
    let mut flag = a.pre_flag.map(str::to_owned);
    if flag.is_none() {
        let verdict = io.check().await;
        match verdict {
            Some(Verdict { flagged: Some(false), .. }) => {}
            Some(v) => flag = Some(v.flag.filter(|f| !f.is_empty()).unwrap_or_else(|| "unavailable".into())),
            None => flag = Some("unavailable".into()),
        }
    }
    let Some(flag) = flag else {
        return cloud("mode");
    };
    if flag == "unavailable" {
        if a.allow_cloud {
            return cloud("remembered");
        }
        return local("check-unavailable");
    }
    if a.allow_cloud {
        return cloud("remembered");
    }
    if a.when_sensitive == "local" {
        return local("sensitive-rule");
    }
    let answer = match a.route_user {
        None => None,
        Some(_) => {
            if !*streamed {
                *streamed = true;
            }
            if io.open_choice().await {
                events.push(obj(vec![
                    ("type", JValue::Str("route_pending".into())),
                    ("flag", JValue::Str(flag.clone())),
                ]));
                io.await_choice().await
            } else {
                Some(Answer { choice: Some("timeout".into()), remember: false })
            }
        }
    };
    match answer.as_ref().and_then(|a| a.choice.as_deref()) {
        Some("cloud") => Route {
            route: Some("cloud"),
            reason: "user-choice".into(),
            remember: answer.as_ref().filter(|a| a.remember).map(|_| "cloud"),
        },
        Some("local") => Route {
            remember: answer.as_ref().filter(|a| a.remember).map(|_| "local"),
            ..local("user-choice")
        },
        _ => local("fail-closed"),
    }
}

struct ResolveArgs<'a> {
    mode: &'a str,
    when_sensitive: &'a str,
    has_cloud: bool,
    has_local: bool,
    hard_local: bool,
    force_local: bool,
    allow_cloud: bool,
    pre_flag: Option<&'a str>,
    route_user: Option<&'a str>,
}

/// chat.cjs:900-1150: where the turn goes, or why it is refused, in Node's order.
pub async fn decide(t: &Turn, io: &impl RouteIo) -> Decided {
    let mut events = Vec::new();
    let mut streamed = false;
    let outcome = decide_inner(t, io, &mut events, &mut streamed).await;
    let outcome = match outcome {
        Outcome::Refused { status, error, .. } => Outcome::Refused { status, error, streamed },
        other => other,
    };
    Decided { events, streamed, outcome }
}

fn refuse(status: u16, error: impl Into<String>) -> Outcome {
    Outcome::Refused { status, error: error.into(), streamed: false }
}

async fn decide_inner(
    t: &Turn,
    io: &impl RouteIo,
    events: &mut Vec<JValue>,
    streamed: &mut bool,
) -> Outcome {
    let def = t.default_provider.as_str();
    let project = t.project.as_ref();
    let project_provider: Option<&str> = project.and_then(|p| match p.provider.as_deref() {
        Some("lemonade") => Some(def),
        other => other,
    });
    let wants_auto = project.map_or(true, |p| p.routing.as_deref() == Some("auto"))
        && project_provider.map_or(true, |p| p.is_empty() || p == def)
        && t.roles_first.is_some();
    let mut route_target: Option<(Route, Option<ProviderRow>, Settings)> = None;
    if wants_auto {
        if let Some(Some(settings)) = &t.routing_modes {
            if let Some(mode) = settings.mode.as_deref().filter(|m| !m.is_empty()) {
                let cloud_row = settings
                    .cloud
                    .provider_id
                    .as_deref()
                    .filter(|id| !id.is_empty() && *id != def)
                    .and_then(|id| io.provider(id));
                let has_cloud = cloud_row.as_ref().is_some_and(|r| {
                    Some(r.id.as_str()) == settings.cloud.provider_id.as_deref()
                        && !settings.cloud.model("smart").is_empty()
                });
                let hard_local = matches!(&t.space_id, JValue::Str(s) if s.starts_with("diary"))
                    || project.is_some_and(|p| p.id == t.diary_project_id);
                let roles = t.roles_later.as_ref();
                if mode == "cloud" && !has_cloud && !hard_local && !t.force_local {
                    return refuse(
                        409,
                        "Cloud routing has no cloud provider and model yet. Choose them in Settings → Models & routing.",
                    );
                }
                let when = if t.compact_only {
                    "local"
                } else {
                    settings.when_sensitive.as_deref().unwrap_or("ask")
                };
                let decided = resolve_route(
                    io,
                    events,
                    streamed,
                    &ResolveArgs {
                        mode,
                        when_sensitive: when,
                        has_cloud,
                        has_local: roles.is_some_and(|r| r.get("fast").is_some() || r.get("smart").is_some()),
                        hard_local,
                        force_local: t.force_local,
                        allow_cloud: t.allow_cloud,
                        pre_flag: if mode == "hybrid" { t.pre_flag.as_deref() } else { None },
                        route_user: t.route_user.as_deref(),
                    },
                )
                .await;
                if io.aborted() {
                    return Outcome::Aborted;
                }
                if decided.route.is_none() {
                    return refuse(
                        409,
                        "Kept local, but no local model is set for Auto. Nothing was sent.",
                    );
                }
                if let (Some(remember), Some(_)) = (decided.remember, t.chat_id.as_deref().filter(|c| !c.is_empty())) {
                    events.push(if remember == "local" {
                        obj(vec![
                            ("type", JValue::Str("route_remembered".into())),
                            ("forceLocal", JValue::Bool(true)),
                            ("allowCloud", JValue::Bool(false)),
                        ])
                    } else {
                        obj(vec![
                            ("type", JValue::Str("route_remembered".into())),
                            ("allowCloud", JValue::Bool(true)),
                        ])
                    });
                }
                let row = if decided.route == Some("cloud") { cloud_row } else { None };
                route_target = Some((decided, row, settings.clone()));
            }
        }
    }
    let provider = match route_target.as_ref().and_then(|(_, row, _)| row.clone()) {
        Some(row) => ProviderRow { external: true, ..row },
        None => {
            let id = if wants_auto { def } else { project_provider.filter(|p| !p.is_empty()).unwrap_or(def) };
            match io.provider(id) {
                Some(p) => p,
                None => return Outcome::Thrown,
            }
        }
    };
    // providerProjection: the caller's base URL is already without userinfo (withoutUserinfo).
    let ext = provider_egress::Provider {
        kind: provider.kind.as_deref().map(units),
        external: provider.external,
        base_url: units(&provider.base_url),
        label: units(provider.label.as_deref().unwrap_or("")),
    };
    let project_id = project.map_or(JValue::Undefined, |p| JValue::Str(p.id.clone()));
    if let Some(text) = provider_egress::egress_refusal(
        Some(&ext),
        &egress_value(&t.space_id),
        &egress_value(&project_id),
        &egress_value(&JValue::Str(t.diary_project_id.clone())),
    ) {
        return refuse(409, String::from_utf16_lossy(&text));
    }
    if provider.kind.as_deref() == Some("chatgpt-oauth") {
        let Some(chatgpt) = t.chatgpt.as_ref().filter(|c| c.enabled) else {
            return refuse(
                409,
                "Sign in with ChatGPT is turned off on this server. Choose another provider in the model popup.",
            );
        };
        let owner = t.owner.as_deref().filter(|o| !o.is_empty());
        let foreign = t
            .workspace_user
            .as_deref()
            .is_some_and(|w| !w.is_empty() && Some(w) != owner);
        if provider.shared || owner.is_none() || foreign {
            return refuse(403, "A ChatGPT connection belongs to one account and cannot be used here.");
        }
        if chatgpt.state != "connected" {
            return refuse(
                409,
                if chatgpt.state == "reconnect" {
                    "Reconnect needed: your ChatGPT sign-in expired or was revoked. Sign in with ChatGPT again in Settings → AI providers."
                } else {
                    "ChatGPT is not connected. Sign in with ChatGPT in Settings → AI providers."
                },
            );
        }
    }
    let mut model = project.and_then(|p| p.model.clone()).filter(|m| !m.is_empty());
    let mut routed_role = None;
    let cloud_row = route_target.as_ref().is_some_and(|(_, row, _)| row.is_some());
    if wants_auto {
        let Some(roles) = t.roles_later.as_ref() else {
            return refuse(
                400,
                "Auto routing is not configured yet — pick Fast and Smart models in the model popup first.",
            );
        };
        let catalogue = io.catalogue().await;
        let missing = missing_roles(roles, catalogue.as_deref());
        let creative_gone = missing.iter().any(|(r, _)| *r == "creative");
        let rest: Vec<_> = missing.into_iter().filter(|(r, _)| *r != "creative").collect();
        if let Some(stale) = stale_roles_error(&rest) {
            return refuse(409, stale);
        }
        let mut role = t.router_role.clone();
        if roles.get(&role).is_none() {
            role = if roles.get("smart").is_some() { "smart".into() } else { "fast".into() };
        }
        if !cloud_row
            && !creative_gone
            && roles.get("creative").is_some()
            && t.creative_for.contains(&role)
            && creative_move_is_free(roles, &role, catalogue.as_deref())
        {
            role = "creative".into();
        }
        model = Some(match route_target.as_ref() {
            Some((_, Some(_), settings)) => settings.cloud.model(&role),
            _ => roles.get(&role).unwrap_or("").to_owned(),
        })
        .filter(|m| !m.is_empty());
        routed_role = Some(role);
    } else if model.is_none() && provider.id == def && t.manager_enabled {
        model = t.last_loaded.clone().filter(|m| !m.is_empty());
    }
    let Some(model) = model else {
        return refuse(
            400,
            if provider.id == def {
                "no model selected and none loaded — pick one in the model popup".to_owned()
            } else {
                let name = provider.label.as_deref().filter(|l| !l.is_empty()).unwrap_or(&provider.id);
                format!("no model selected for {name} — pick one in the model popup")
            },
        );
    };
    if !wants_auto
        && project.is_some_and(|p| p.model.as_deref() == Some(model.as_str()))
        && provider.id == def
    {
        let catalogue = io.catalogue().await;
        let labels = catalogue
            .as_deref()
            .and_then(|c| c.iter().find(|e| e.name == model))
            .map(|e| e.labels.clone())
            .unwrap_or_default();
        if !is_chat_generation_model(&model, &labels, (t.system_model)(&model)) {
            return refuse(
                409,
                format!("{model} is an embedding, reranking or routing model and cannot answer chat messages — pick a chat model in the model popup."),
            );
        }
    }
    let (route, hybrid_ungranted) = match route_target {
        Some((d, _, settings)) => {
            let route = d.route.unwrap_or("local");
            let ungranted = settings.mode.as_deref() == Some("hybrid")
                && route == "cloud"
                && !(t.allow_cloud || d.remember == Some("cloud") || d.reason == "user-choice");
            (Some((route.to_owned(), d.reason)), ungranted)
        }
        None => (None, false),
    };
    Outcome::Go(Go { provider, model, role: routed_role, route, hybrid_ungranted })
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The port's argument: a string as itself, anything else `null` (provider-egress.cjs `primitive`).
fn egress_value(v: &JValue) -> prompt_framing::json::Value {
    match v {
        JValue::Str(s) => prompt_framing::json::Value::Str(units(s)),
        _ => prompt_framing::json::Value::Null,
    }
}
