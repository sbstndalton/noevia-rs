//! `embedding-config.cjs`'s target rules. A target is `{ baseUrl, model, profile }`: `baseUrl` null
//! means "the inference engine", a URL a dedicated embeddings server. The identity of stored
//! vectors is (profile, model); the endpoint is where the model runs, not what it is.

use serde_json::{json, Map, Value};

use crate::js;

/// An error the Node code throws with `status` / `code` (and sometimes more fields) set. The route
/// turns it into `{ error, status, code?, coverage?, parity?, served? }`; `status` None is a plain
/// `Error` (the route answers 500).
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: Option<u16>,
    pub code: Option<&'static str>,
    pub message: String,
    /// `coverage`, `parity` or `served`, in that order, when the Node error carried them.
    pub extra: Map<String, Value>,
}

impl Refusal {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            code: Some(code),
            message: message.into(),
            extra: Map::new(),
        }
    }

    /// A plain `Error(message)`: no status, no code.
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            status: None,
            code: None,
            message: message.into(),
            extra: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.to_owned(), value);
        self
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The framing profiles core knows (`embedding-profile.cjs`): data from the caller, so no profile
/// name is spelled here. `known` is in core's order (it is printed in a refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    pub default: String,
    pub known: Vec<String>,
}

impl Profiles {
    pub fn is_known(&self, name: &str) -> bool {
        self.known.iter().any(|k| k == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub base_url: Option<String>,
    pub model: String,
    pub profile: String,
}

impl Target {
    /// The JSON shape core saves and answers with, in its key order.
    pub fn to_json(&self) -> Value {
        json!({ "baseUrl": self.base_url, "model": self.model, "profile": self.profile })
    }

    /// `identityOf`: profile NUL model.
    pub fn identity(&self) -> String {
        format!("{}\u{0}{}", self.profile, self.model)
    }
}

/// `sameIdentity(a, b)`: both present, same profile and model.
pub fn same_identity(a: Option<&Target>, b: Option<&Target>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.identity() == b.identity())
}

/// `sameTarget(a, b)`: same identity and the same address (an empty address is the engine).
pub fn same_target(a: Option<&Target>, b: Option<&Target>) -> bool {
    let address = |t: &Target| t.base_url.clone().filter(|u| !u.is_empty());
    same_identity(a, b) && matches!((a, b), (Some(a), Some(b)) if address(a) == address(b))
}

/// `opt` serialised as core does: the target, or null.
pub fn opt_json(t: Option<&Target>) -> Value {
    t.map_or(Value::Null, Target::to_json)
}

fn invalid(message: impl Into<String>) -> Refusal {
    Refusal::new(400, "invalidTarget", message)
}

/// `normalizeBaseUrl`: blank is the engine (None); else a plain http(s) URL with no credentials,
/// query or fragment, given back as typed (trimmed) without its trailing slashes.
pub fn normalize_base_url(value: Option<&Value>) -> Result<Option<String>, Refusal> {
    let raw = js::js_string(value);
    let text = js::trim(&raw);
    if text.is_empty() {
        return Ok(None);
    }
    let url = url::Url::parse(text)
        .map_err(|_| invalid("The embeddings address must be an http(s) URL."))?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none_or(str::is_empty)
        && url.query().is_none_or(str::is_empty)
        && url.fragment().is_none_or(str::is_empty);
    if !plain {
        return Err(invalid(
            "The embeddings address must be a plain http(s) URL, with no credentials, query or fragment.",
        ));
    }
    Ok(Some(text.trim_end_matches('/').to_owned()))
}

/// `MODEL_RE`: 1 to 200 UTF-16 units, none a JavaScript space or a C0 control.
fn model_ok(model: &str) -> bool {
    let len = js::utf16_len(model);
    (1..=200).contains(&len) && !model.chars().any(|c| js::is_space(c) || c <= '\u{1f}')
}

/// `normalizeTarget(t)`: a validated copy, or the 400 refusal.
pub fn normalize_target(t: Option<&Value>, profiles: &Profiles) -> Result<Target, Refusal> {
    let Some(Value::Object(t)) = t else {
        return Err(invalid("An embedding target needs a model."));
    };
    let model = js::trim(&js::js_string(t.get("model"))).to_owned();
    if !model_ok(&model) {
        return Err(invalid(
            "An embedding target needs a model name (no spaces, at most 200 characters).",
        ));
    }
    let profile = match t.get("profile") {
        None | Some(Value::Null) => profiles.default.clone(),
        given => js::trim(&js::js_string(given)).to_owned(),
    };
    let profile = if profile.is_empty() {
        profiles.default.clone()
    } else {
        profile
    };
    if !profiles.is_known(&profile) {
        return Err(invalid(format!(
            "Unknown embedding profile \"{profile}\" (known: {}).",
            profiles.known.join(", ")
        )));
    }
    Ok(Target {
        base_url: normalize_base_url(t.get("baseUrl"))?,
        model,
        profile,
    })
}

/// The three targets of a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub active: Target,
    pub next: Option<Target>,
    pub previous: Option<Target>,
}

/// `seedFromEnv(env, fallbackModel, log)`: the targets the environment describes. Nothing is
/// validated here: an unknown profile is kept as given and fails closed later. Returns the warning
/// core logs for a next model without a profile.
pub fn seed_from_env(
    get: &dyn Fn(&str) -> Option<String>,
    fallback_model: &str,
    profiles: &Profiles,
) -> (State, Option<String>) {
    let pick = |keys: &[&str]| {
        keys.iter()
            .map(|k| js::trim(&get(k).unwrap_or_default()).to_owned())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let or = |v: String, d: &str| if v.is_empty() { d.to_owned() } else { v };
    let fallback = if fallback_model.is_empty() {
        "default"
    } else {
        fallback_model
    };
    let active = Target {
        base_url: Some(pick(&["EMBEDDING_BASE_URL"])).filter(|v| !v.is_empty()),
        model: or(pick(&["EMBEDDING_MODEL", "EMBED_MODEL"]), fallback),
        profile: or(pick(&["EMBEDDING_PROFILE"]), &profiles.default),
    };
    let next_model = pick(&["EMBEDDING_NEXT_MODEL"]);
    let next_profile = pick(&["EMBEDDING_NEXT_PROFILE"]);
    let warning = (!next_model.is_empty() && next_profile.is_empty()).then(|| {
        format!(
            "[embedding] EMBEDDING_NEXT_MODEL={next_model} is set without EMBEDDING_NEXT_PROFILE: using {}, which sends text unframed. Set the profile if the model needs prompts (known: {}).",
            profiles.default,
            profiles.known.join(", ")
        )
    });
    let next = (!next_model.is_empty()).then(|| Target {
        base_url: Some(pick(&["EMBEDDING_NEXT_BASE_URL"])).filter(|v| !v.is_empty()),
        model: next_model,
        profile: or(next_profile, &profiles.default),
    });
    let next = next.filter(|n| !same_target(Some(n), Some(&active)));
    (
        State {
            active,
            next,
            previous: None,
        },
        warning,
    )
}
