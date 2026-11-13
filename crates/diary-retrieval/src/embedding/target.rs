//! Targets: reading one from JSON (`normalize_target`) and from the environment-seeded
//! configuration tree (`seed_from_cfg`).

use serde_json::{json, Value};

use super::config::State;
use super::pystr::{is_space, str_or_empty, strip};
use super::{known_profiles, EmbeddingConfigError, PROFILES};
use crate::{Target, DEFAULT_PROFILE};

impl Target {
    /// `Target.to_json()`: `{base_url, model, profile}` in that order.
    pub fn to_json(&self) -> Value {
        json!({"base_url": self.base_url, "model": self.model, "profile": self.profile})
    }
}

/// `_MODEL_RE.match(model)` for `^[^\s\x00-\x1f]{1,200}$`.
///
/// Python's `$` also matches just before one final `\n`, so a body followed by a single `\n`
/// matches. `normalize_target` strips first, which removes any trailing newline, so that
/// allowance can never let a model id through there; it is kept so the function is the regex.
pub fn model_matches(model: &str) -> bool {
    let body = model.strip_suffix('\n').unwrap_or(model);
    let n = body.chars().count();
    (1..=200).contains(&n) && body.chars().all(|c| !is_space(c) && u32::from(c) >= 0x20)
}

/// `re.match(r"^(https?)://([^/?#@\s]+)(/[^?#\s]*)?$", text)` for a stripped `text`.
fn base_url_matches(text: &str) -> bool {
    let Some(rest) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    else {
        return false;
    };
    let host_len: usize = rest
        .chars()
        .take_while(|&c| !matches!(c, '/' | '?' | '#' | '@') && !is_space(c))
        .map(char::len_utf8)
        .sum();
    if host_len == 0 {
        return false;
    }
    let Some(path) = rest.get(host_len..) else {
        return false;
    };
    // `$` also matches before a final newline; `text` is stripped, so none is left.
    let path = path.strip_suffix('\n').unwrap_or(path);
    match path.strip_prefix('/') {
        None => path.is_empty(),
        Some(tail) => tail
            .chars()
            .all(|c| !matches!(c, '?' | '#') && !is_space(c)),
    }
}

const BASE_URL_MESSAGE: &str =
    "The embeddings address must be a plain http(s) URL, with no credentials, query or fragment.";

/// `_normalize_base_url`: empty is `None`, anything but a plain http(s) URL is refused, trailing
/// slashes are dropped.
pub fn normalize_base_url(value: Option<&Value>) -> Result<Option<String>, EmbeddingConfigError> {
    let text = str_or_empty(value);
    let text = strip(&text);
    if text.is_empty() {
        return Ok(None);
    }
    if !base_url_matches(text) {
        return Err(EmbeddingConfigError::invalid_target(BASE_URL_MESSAGE));
    }
    Ok(Some(text.trim_end_matches('/').to_owned()))
}

/// `normalize_target(raw)` for a decoded JSON value: `model`, `profile` (default `nomic-v1`) and
/// `base_url` / `baseUrl` (the first key present wins, even when null).
pub fn normalize_target(raw: &Value) -> Result<Target, EmbeddingConfigError> {
    let Value::Object(map) = raw else {
        return Err(EmbeddingConfigError::invalid_target(
            "An embedding target needs a model.",
        ));
    };
    let model = strip(&str_or_empty(map.get("model"))).to_owned();
    if !model_matches(&model) {
        return Err(EmbeddingConfigError::invalid_target(
            "An embedding target needs a model name (no spaces, at most 200 characters).",
        ));
    }
    let profile_text = str_or_empty(map.get("profile"));
    let profile = strip(&profile_text);
    let profile = if profile.is_empty() {
        DEFAULT_PROFILE
    } else {
        profile
    };
    if !PROFILES.contains(&profile) {
        return Err(EmbeddingConfigError::invalid_target(format!(
            "Unknown embedding profile \"{profile}\" (known: {}).",
            known_profiles()
        )));
    }
    let base = map.get("base_url").or_else(|| map.get("baseUrl"));
    Ok(Target {
        base_url: normalize_base_url(base)?,
        model,
        profile: profile.to_owned(),
    })
}

/// `normalize_target(target)`: a `Target` is normalised through its JSON form.
pub fn normalize_target_from(target: &Target) -> Result<Target, EmbeddingConfigError> {
    normalize_target(&target.to_json())
}

/// `cfg.get("a.b")`: a dotted walk through nested objects; `None` when a step is missing or not
/// an object.
pub fn cfg_get<'a>(cfg: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut node = cfg;
    for part in dotted.split('.') {
        node = node.as_object()?.get(part)?;
    }
    Some(node)
}

fn cfg_text(cfg: &Value, key: &str) -> String {
    strip(&str_or_empty(cfg_get(cfg, key))).to_owned()
}

fn none_if_empty(s: &str) -> Option<String> {
    let s = s.trim_end_matches('/');
    (!s.is_empty()).then(|| s.to_owned())
}

/// `seed_from_cfg(cfg)`: the targets the configuration (environment) describes. An unknown
/// profile is kept, so it fails closed later.
pub fn seed_from_cfg(cfg: &Value) -> State {
    let get = |key: &str| cfg_text(cfg, key);
    let or = |s: String, default: &str| if s.is_empty() { default.to_owned() } else { s };
    let active = Target {
        base_url: none_if_empty(&get("llm.embed_base_url")),
        model: or(get("llm.embed_model"), "default"),
        profile: or(get("llm.embed_profile"), DEFAULT_PROFILE),
    };
    let mut next = None;
    if !get("llm.embed_next_model").is_empty() {
        let candidate = Target {
            base_url: none_if_empty(&get("llm.embed_next_base_url")),
            model: get("llm.embed_next_model"),
            profile: or(get("llm.embed_next_profile"), DEFAULT_PROFILE),
        };
        if candidate != active {
            next = Some(candidate);
        }
    }
    State {
        active,
        next,
        previous: None,
    }
}

/// The warning `seed_from_cfg` logs when a next model is set without a profile (the model
/// silently gets unframed text).
pub fn seed_warning(cfg: &Value) -> Option<String> {
    let model = cfg_text(cfg, "llm.embed_next_model");
    if model.is_empty() || !cfg_text(cfg, "llm.embed_next_profile").is_empty() {
        return None;
    }
    Some(format!(
        "LLM_EMBED_NEXT_MODEL={model} is set without LLM_EMBED_NEXT_PROFILE: using {DEFAULT_PROFILE}, \
         which sends text unframed. Set the profile if the model needs prompts (known: {}).",
        known_profiles()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_regex_allows_one_final_newline_only() {
        assert!(model_matches("qa-embed-a"));
        assert!(model_matches("qa-embed-a\n"));
        assert!(!model_matches("qa-embed-a\n\n"));
        assert!(!model_matches("\n"));
        assert!(!model_matches("qa embed"));
        assert!(!model_matches(&"x".repeat(201)));
        assert!(model_matches(&"x".repeat(200)));
    }

    #[test]
    fn a_model_with_a_trailing_newline_is_stripped_not_refused() -> Result<(), EmbeddingConfigError>
    {
        let t = normalize_target(&json!({"model": "qa-embed-a\n"}))?;
        assert_eq!(t.model, "qa-embed-a");
        Ok(())
    }

    #[test]
    fn base_url_shapes() {
        assert!(base_url_matches("http://qa-embed.invalid:8080/v1"));
        assert!(base_url_matches("https://qa-embed.invalid"));
        assert!(!base_url_matches("ftp://qa-embed.invalid"));
        assert!(!base_url_matches("http://u@qa-embed.invalid"));
        assert!(!base_url_matches("http://qa-embed.invalid/a?b"));
        assert!(!base_url_matches("http:///x"));
    }
}
