//! What core answers on `POST /internal/code/engine` for one Code start (`rust-code-executor.cjs`
//! `createCodeEngineBridge`, wired by `index.cjs`): the engine and model the harness gets, whether
//! the Code role runs on an inference machine, whether a tune is active, the project's shared
//! context and (when the Planner pipeline or the Planner review is on) the role engine's endpoint.
//!
//! The order is core's: the project (404), the machine (`codeTargetFrom` asks the registry for the
//! Code role, then reads the roles, then asks for Smart), the engine (only on this machine), the
//! tune state (only on this machine), the shared context (`loadChats` before `forCode`), the role.
//! A throw anywhere is core's 503.

use crate::{provider, shared, Thrown};
use chat_turn::js;
use js_json::{wtf16, JValue};

/// The environment core reads for the default provider row (`index.cjs` `INFERENCE_BASE`,
/// `INFERENCE_KEY`, `DEFAULT_PROVIDER_ID`, `DEFAULT_PROVIDER_LABEL`, `MODEL_MANAGER_KIND`, and
/// `DEFAULT_PROVIDER_CAPABILITIES`).
#[derive(Clone, PartialEq)]
pub struct Deployment {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub api_key: String,
    pub kind: String,
}

impl std::fmt::Debug for Deployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment")
            .field("id", &self.id)
            .field("label", &self.label)
            .field(
                "base_url",
                &provider::without_userinfo(self.base_url.clone()),
            )
            .field("api_key", &"<redacted>")
            .field("kind", &self.kind)
            .finish()
    }
}

impl Deployment {
    /// `process.env.X || ...`: an empty value counts as unset.
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Deployment {
        let env = |k: &str| get(k).filter(|v| !v.is_empty());
        Deployment {
            base_url: env("INFERENCE_BASE_URL")
                .or_else(|| env("LEMONADE_BASE_URL"))
                .unwrap_or_else(|| "http://host.docker.internal:11434".into()),
            api_key: env("INFERENCE_API_KEY")
                .or_else(|| env("LEMONADE_API_KEY"))
                .unwrap_or_default(),
            id: env("DEFAULT_PROVIDER_ID").unwrap_or_else(|| "default".into()),
            label: env("DEFAULT_PROVIDER_LABEL").unwrap_or_else(|| "Local inference".into()),
            kind: env("MODEL_MANAGER_KIND").unwrap_or_else(|| {
                if env("LEMONADE_BASE_URL").is_some() {
                    "lemonade".into()
                } else {
                    "none".into()
                }
            }),
        }
    }

    /// The default row as the view holds it (`{ ...defaultProvider, shared: true }`).
    pub fn row(&self) -> JValue {
        let mut items = vec![
            ("id".to_string(), JValue::Str(self.id.clone())),
            ("label".to_string(), JValue::Str(self.label.clone())),
            ("baseUrl".to_string(), JValue::Str(self.base_url.clone())),
            ("apiKey".to_string(), JValue::Str(self.api_key.clone())),
            ("shared".to_string(), JValue::Bool(true)),
        ];
        if let Some(caps) = provider::engine_capabilities(&self.kind) {
            items.push(("capabilities".to_string(), caps));
        }
        JValue::Obj(items)
    }

    /// `getProvider(DEFAULT_PROVIDER_ID)`: the default row always leads the view, so it is this row
    /// with its effective capabilities, whatever else the view holds.
    pub fn provider(&self) -> JValue {
        provider::get_provider(&[self.row()], &JValue::Str(self.id.clone()), &self.id)
            .unwrap_or(JValue::Null)
    }
}

/// The Code environment core reads (`CODE_ENGINE_URL`, `CODE_ENGINE_API_KEY`,
/// `CODE_CONTEXT_TOKENS`), as set (an empty value is kept: it counts as unset where core reads it).
#[derive(Clone, Default, PartialEq)]
pub struct CodeEnv {
    pub engine_url: Option<String>,
    pub engine_api_key: Option<String>,
    pub context_tokens: Option<String>,
}

impl std::fmt::Debug for CodeEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeEnv")
            .field("engine_url", &self.engine_url)
            .field(
                "engine_api_key",
                &self.engine_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("context_tokens", &self.context_tokens)
            .finish()
    }
}

impl CodeEnv {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> CodeEnv {
        CodeEnv {
            engine_url: get("CODE_ENGINE_URL"),
            engine_api_key: get("CODE_ENGINE_API_KEY"),
            context_tokens: get("CODE_CONTEXT_TOKENS"),
        }
    }
}

fn set_text(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

/// `codeTargetFrom(chatMachines, autoRoles)`: the Code role's machine, else (when no Code model is
/// chosen here) the Smart role's; `Null` for this machine. `target(role)` is
/// `chatMachines.target(role)` (`machines.chatTarget(role) || null`), whose error propagates;
/// `roles` is `autoRoles()` (`None` where it throws, read as no Code model here).
pub fn code_target<E>(
    mut target: impl FnMut(&str) -> Result<JValue, E>,
    roles: impl FnOnce() -> Option<JValue>,
) -> Result<JValue, E> {
    let code = target("code")?;
    if code.truthy() {
        return Ok(code);
    }
    let local_code = roles().is_some_and(|r| r.get("code").truthy());
    if local_code {
        return Ok(JValue::Null);
    }
    target("smart")
}

/// `autoRoles()?.code || autoRoles()?.smart || lastLoadedModel() || null`.
fn model(roles: &JValue, last_loaded: &JValue) -> JValue {
    [roles.get("code"), roles.get("smart"), last_loaded]
        .into_iter()
        .find(|v| v.truthy())
        .cloned()
        .unwrap_or(JValue::Null)
}

/// `base ? (/\/v1$/.test(base) ? base : `${base}/v1`) : null` of a base with its trailing slashes
/// removed (`.replace(/\/+$/, '')`).
fn v1(base: &str) -> JValue {
    let base = base.trim_end_matches('/');
    if base.is_empty() {
        JValue::Null
    } else if base.ends_with("/v1") {
        JValue::Str(base.to_string())
    } else {
        JValue::Str(format!("{base}/v1"))
    }
}

/// `index.cjs` `codeEngine()` on this machine (the machine branch is never asked: the bridge reads
/// the engine only when the Code role is here). `provider` is `getProvider(DEFAULT_PROVIDER_ID)`.
/// `Err` where `(CODE_ENGINE_URL || provider?.baseUrl || '').replace` throws (a base address that
/// is not text).
pub fn engine(
    env: &CodeEnv,
    provider: &JValue,
    roles: &JValue,
    last_loaded: &JValue,
) -> Result<JValue, Thrown> {
    let base = match set_text(&env.engine_url) {
        Some(url) => url.to_string(),
        None => match provider.get("baseUrl") {
            JValue::Str(s) => s.clone(),
            other if !other.truthy() => String::new(),
            _ => return Err(Thrown),
        },
    };
    let api_key = match set_text(&env.engine_api_key) {
        Some(k) => JValue::Str(k.to_string()),
        None => Some(provider.get("apiKey"))
            .filter(|k| k.truthy())
            .cloned()
            .unwrap_or(JValue::Null),
    };
    // `Number(process.env.CODE_CONTEXT_TOKENS) || undefined`.
    let tokens = env
        .context_tokens
        .as_deref()
        .map_or(f64::NAN, js::string_to_number);
    Ok(JValue::obj([
        ("baseUrl", v1(&base)),
        ("apiKey", api_key),
        ("model", model(roles, last_loaded)),
        (
            "contextTokens",
            if tokens != 0.0 && !tokens.is_nan() {
                JValue::Num(tokens)
            } else {
                JValue::Undefined
            },
        ),
    ]))
}

/// `index.cjs` `codeRoleEndpoint()` on this machine, read through `roleFacts`: the role engine's
/// endpoint as this process reaches the default provider (not `CODE_ENGINE_URL`), with the key held
/// back when the provider is external. `external` is `isExternalProvider(provider)`
/// ([`provider::is_external`]). `Err` where `String(provider.baseUrl)` cannot be written.
pub fn role(
    provider: &JValue,
    roles: &JValue,
    last_loaded: &JValue,
    external: bool,
) -> Result<JValue, Thrown> {
    let base = provider.get("baseUrl");
    let base = if base.truthy() {
        js_json::to_js_string(base).map_err(|_| Thrown)?
    } else {
        String::new()
    };
    let key = provider.get("apiKey");
    Ok(JValue::obj([
        ("baseUrl", v1(&base)),
        (
            "apiKey",
            if external || !key.truthy() {
                JValue::Null
            } else {
                key.clone()
            },
        ),
        ("model", model(roles, last_loaded)),
        ("external", JValue::Bool(external)),
        (
            "jsonSchema",
            JValue::Bool(
                provider.get("capabilities").get("jsonSchemaParam") == &JValue::Bool(true),
            ),
        ),
    ]))
}

/// What one start reads of the deployment and the account, besides the project and the machine.
pub struct Inputs<'a> {
    pub env: &'a CodeEnv,
    /// `getProvider(DEFAULT_PROVIDER_ID)` ([`Deployment::provider`]).
    pub provider: &'a JValue,
    /// `isExternalProvider(provider)`.
    pub external: bool,
    /// `autoRoles()`: the account's `auto-roles.json`, `null` when it does not read.
    pub roles: &'a JValue,
    /// `lastLoadedModel()`.
    pub last_loaded: &'a JValue,
    /// `features.enabled('codePipeline') || features.enabled('plannerReview')`.
    pub role_wanted: bool,
}

/// The bridge's answer. Its `Debug` names the fields of [`Answer::Facts`] but never their values
/// (the engine and role keys are in them).
#[derive(Clone, PartialEq)]
pub enum Answer {
    /// 404 `project not found`.
    NotFound,
    /// 503 `Code engine facts are unavailable.`; the reason core logs.
    Unavailable(&'static str),
    /// 200 with this body.
    Facts(JValue),
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answer::NotFound => f.write_str("NotFound"),
            Answer::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
            Answer::Facts(facts) => {
                let names: Vec<&str> = match facts {
                    JValue::Obj(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
                    _ => Vec::new(),
                };
                write!(f, "Facts({names:?}, values redacted)")
            }
        }
    }
}

/// Core's answer for one start. `project` is `getProject(projectId)` for the user; `target(role)`
/// is `chatMachines.target(role)` (an `Err` is core's `machinesUnavailable`); `paused()` is
/// `modelManager.tuningActive()`, asked only when the Code role is on this machine.
pub fn answer<E>(
    project: Option<&JValue>,
    inputs: &Inputs<'_>,
    target: impl FnMut(&str) -> Result<JValue, E>,
    paused: impl FnOnce() -> JValue,
) -> Answer {
    let Some(project) = project else {
        return Answer::NotFound;
    };
    let Ok(machine) = code_target(target, || Some(inputs.roles.clone())) else {
        return Answer::Unavailable("machinesUnavailable");
    };
    let remote = machine.truthy();
    let engine = if remote {
        JValue::Null
    } else {
        match engine(
            inputs.env,
            inputs.provider,
            inputs.roles,
            inputs.last_loaded,
        ) {
            Ok(e) => JValue::obj(
                ["baseUrl", "apiKey", "model", "contextTokens"].map(|k| (k, nullish(e.get(k)))),
            ),
            Err(Thrown) => return Answer::Unavailable("TypeError"),
        }
    };
    let paused = !remote && paused() == JValue::Bool(true);
    let context =
        match shared::load_chats(project).and_then(|chats| shared::for_code(project, &chats)) {
            Ok(text) => text,
            Err(Thrown) => return Answer::Unavailable("TypeError"),
        };
    let mut items = vec![
        ("engine".to_string(), engine),
        ("machine".to_string(), JValue::Bool(remote)),
        ("paused".to_string(), JValue::Bool(paused)),
        // `String(context(project) || '')`.
        ("context".to_string(), JValue::Str(context)),
    ];
    if inputs.role_wanted {
        let facts = if remote {
            JValue::Null
        } else {
            match role(
                inputs.provider,
                inputs.roles,
                inputs.last_loaded,
                inputs.external,
            ) {
                Ok(r) => r,
                Err(Thrown) => return Answer::Unavailable("TypeError"),
            }
        };
        wtf16::set(&mut items, "role", facts);
    }
    Answer::Facts(JValue::Obj(items))
}

/// `value ?? null`.
fn nullish(v: &JValue) -> JValue {
    if v.is_nullish() {
        JValue::Null
    } else {
        v.clone()
    }
}
