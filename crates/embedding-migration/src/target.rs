//! A target `{ baseUrl, model, profile }` (`embedding-config.cjs`), the profile names
//! (`embedding-profile.cjs`) and whether a `/v1/models` list serves a model
//! (`embedding-served.cjs`).

use crate::{served_json, Failure};
use chat_turn::js;
use js_json::{to_js_string, wtf16, JValue};

pub const DEFAULT_PROFILE: &str = "nomic-v1";
/// `profileNames()`, in `PROFILES` order.
pub const PROFILES: [&str; 2] = ["nomic-v1", "embeddinggemma-2"];
/// What V8 says when `String(v)` meets an object with an own `toString`.
pub const COERCE_ERROR: &str = "Cannot convert object to primitive value";

/// Where vectors come from. `base_url: None` is the inference engine. Seeded targets are kept as
/// the environment gave them (an unknown profile fails closed later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub base_url: Option<String>,
    pub model: String,
    pub profile: String,
}

impl Target {
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("baseUrl", self.base_url.clone().into()),
            ("model", self.model.as_str().into()),
            ("profile", self.profile.as_str().into()),
        ])
    }

    fn address(&self) -> Option<&str> {
        self.base_url.as_deref().filter(|a| !a.is_empty())
    }
}

pub(crate) fn json_of(t: Option<&Target>) -> JValue {
    t.map_or(JValue::Null, Target::to_json)
}

/// `sameIdentity`: the same profile and model (where the model runs does not count).
pub fn same_identity(a: Option<&Target>, b: Option<&Target>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.profile == b.profile && a.model == b.model)
}

/// `sameTarget`: the same identity on the same address.
pub fn same_target(a: Option<&Target>, b: Option<&Target>) -> bool {
    same_identity(a, b) && a.and_then(Target::address) == b.and_then(Target::address)
}

/// `String(v ?? fallback)`.
pub(crate) fn text(v: &JValue, fallback: &str) -> Result<String, Failure> {
    if v.is_nullish() {
        return Ok(fallback.to_string());
    }
    to_js_string(v).map_err(|_| Failure::plain(COERCE_ERROR))
}

fn invalid(message: impl Into<String>) -> Failure {
    Failure::new(400, "invalidTarget", message)
}

/// `normalizeBaseUrl`: `None` for an empty address; the text without trailing slashes otherwise.
pub fn normalize_base_url(value: &JValue) -> Result<Option<String>, Failure> {
    let raw = text(value, "")?;
    let address = js::trim(&raw);
    if address.is_empty() {
        return Ok(None);
    }
    let url = url::Url::parse(address)
        .map_err(|_| invalid("The embeddings address must be an http(s) URL."))?;
    let filled = |part: Option<&str>| part.is_some_and(|p| !p.is_empty());
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || filled(url.password())
        || filled(url.query())
        || filled(url.fragment())
    {
        return Err(invalid("The embeddings address must be a plain http(s) URL, with no credentials, query or fragment."));
    }
    Ok(Some(address.trim_end_matches('/').to_string()))
}

/// `MODEL_RE` (`/^[^\s\u0000-\u001f]{1,200}$/`, counted in UTF-16 code units).
fn model_name_ok(model: &str) -> bool {
    (1..=200).contains(&wtf16::len(model)) && !model.chars().any(|c| js::is_js_space(c) || c < ' ')
}

/// `normalizeTarget`: a validated copy, or the 400 `invalidTarget` refusal.
pub fn normalize_target(t: &JValue) -> Result<Target, Failure> {
    if !t.truthy() || !matches!(t, JValue::Obj(_) | JValue::Arr(_)) {
        return Err(invalid("An embedding target needs a model."));
    }
    let model = js::trim(&text(t.get("model"), "")?).to_string();
    if !model_name_ok(&model) {
        return Err(invalid(
            "An embedding target needs a model name (no spaces, at most 200 characters).",
        ));
    }
    let profile = text(t.get("profile"), DEFAULT_PROFILE)?;
    let profile = match js::trim(&profile) {
        "" => DEFAULT_PROFILE,
        named => named,
    };
    if !PROFILES.contains(&profile) {
        return Err(invalid(format!(
            "Unknown embedding profile \"{profile}\" (known: {}).",
            PROFILES.join(", ")
        )));
    }
    Ok(Target {
        base_url: normalize_base_url(t.get("baseUrl"))?,
        model,
        profile: profile.to_string(),
    })
}

/// `seedFromEnv(env, 'default', log)`: the targets the environment describes, and the warnings
/// Node logs.
pub fn seed_from_env(env: &dyn Fn(&str) -> Option<String>) -> (crate::State, Vec<String>) {
    let pick = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            env(key)
                .map(|v| js::trim(&v).to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let active = Target {
        base_url: pick(&["EMBEDDING_BASE_URL"]),
        model: pick(&["EMBEDDING_MODEL", "EMBED_MODEL"]).unwrap_or_else(|| "default".into()),
        profile: pick(&["EMBEDDING_PROFILE"]).unwrap_or_else(|| DEFAULT_PROFILE.into()),
    };
    let mut warnings = Vec::new();
    let next = pick(&["EMBEDDING_NEXT_MODEL"]).map(|model| {
        let profile = pick(&["EMBEDDING_NEXT_PROFILE"]);
        if profile.is_none() {
            warnings.push(format!("[embedding] EMBEDDING_NEXT_MODEL={model} is set without EMBEDDING_NEXT_PROFILE: using {DEFAULT_PROFILE}, which sends text unframed. Set the profile if the model needs prompts (known: {}).", PROFILES.join(", ")));
        }
        Target {
            base_url: pick(&["EMBEDDING_NEXT_BASE_URL"]),
            model,
            profile: profile.unwrap_or_else(|| DEFAULT_PROFILE.into()),
        }
    });
    let next = next.filter(|n| !same_target(Some(n), Some(&active)));
    let state = crate::State {
        active,
        next,
        previous: None,
    };
    (state, warnings)
}

const MAX_ECHO: usize = 20;
const MAX_ECHO_CHARS: usize = 120;

fn canonical(s: &str) -> String {
    let lower = js::trim(s).to_lowercase();
    lower.strip_suffix(":latest").unwrap_or(&lower).to_string()
}

fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    let drive = matches!(b, [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic());
    let ends = |ext: &str| {
        s.len() >= ext.len()
            && s.get(s.len() - ext.len()..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(ext))
    };
    s.starts_with(['/', '~'])
        || s.starts_with("./")
        || s.starts_with("../")
        || drive
        || ends(".gguf")
        || ends(".bin")
        || ends(".safetensors")
}

/// `checkServed`'s verdict for `model` on the list `/v1/models` gave (`None`: the request
/// failed). A refusal carries status 409, as every caller sends it.
pub fn served_verdict(model: &str, ids: Option<&[String]>) -> Result<(), Failure> {
    let model = js::trim(model);
    let shown = wtf16::slice(model, MAX_ECHO_CHARS);
    let Some(ids) = ids else {
        return Err(Failure::new(
            409,
            "endpointUnverified",
            format!("Could not confirm that the embeddings server serves {shown}."),
        ));
    };
    let want = canonical(model);
    if !want.is_empty() && ids.iter().any(|id| canonical(id) == want) {
        return Ok(());
    }
    let served: Vec<String> = ids
        .iter()
        .take(MAX_ECHO)
        .map(|id| wtf16::slice(id, MAX_ECHO_CHARS))
        .collect();
    let message = if !ids.is_empty() && ids.iter().all(|id| looks_like_path(id)) {
        format!("The embeddings server lists only a model file, not model names, so it cannot be confirmed that it serves {shown}. Start it with --alias {shown} (or load {shown} there).")
    } else {
        format!("The embeddings server does not serve {shown}.")
    };
    Err(Failure::new(409, "modelNotServed", message).with(crate::Detail::Served(served)))
}

/// `checkServed(servedModels, { baseUrl, model })`.
pub fn check_served<H: crate::Host>(
    host: &mut H,
    base_url: Option<&str>,
    model: &str,
) -> Result<(), Failure> {
    let model = js::trim(model);
    let ids = host.served_models(base_url.filter(|a| !a.is_empty()), model);
    served_verdict(model, ids.as_deref())
}

/// The `served` list of a refusal as the answer shows it.
pub fn served_list(failure: &Failure) -> Option<JValue> {
    failure.served().map(served_json)
}
