//! The full static product catalog, generated from contracts/features/catalog.json.
//! Node remains the synchronous feature writer and public route owner.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AvailabilityKind {
    Static,
    DecisionChoice,
    SystemOne,
    Browser,
    CodeSandbox,
    TrustProxy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spec {
    pub name: &'static str,
    pub env: &'static str,
    pub setting: &'static str,
    pub default: bool,
    pub experimental: bool,
    pub restart: bool,
    pub legacy: Option<&'static str>,
    pub label: &'static str,
    pub description: &'static str,
    pub availability: AvailabilityKind,
}

include!("catalog_generated.rs");

pub const INVALID_ENV: &str = "Invalid boolean in feature env var: expected true/false";

/// Match core features.cjs parseEnv, including ECMAScript trim. An invalid
/// nonblank value is a startup error, including when a saved setting exists.
pub fn parse_env(raw: Option<&str>) -> Result<Option<bool>, &'static str> {
    let Some(raw) = raw else { return Ok(None) };
    let units: Vec<u16> = raw.encode_utf16().collect();
    let trimmed = String::from_utf16_lossy(policy_leaves::js_trim(&units));
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.to_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(Some(true)),
        "0" | "false" | "off" | "no" => Ok(Some(false)),
        _ => Err(INVALID_ENV),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Default,
    Env,
    Admin,
}

impl Source {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Env => "env",
            Self::Admin => "admin",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    /// The owning runtime has not supplied a verdict; dynamic flags fail closed.
    Unknown,
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Evaluation {
    /// Selected admin/env value before availability and restart constraints.
    pub value: bool,
    pub source: Source,
    pub locked: bool,
    pub available: bool,
    /// None means a restart-only flag has no known current Node boot value.
    pub pending_restart: Option<bool>,
    /// The effective answer to Node's enabled(name).
    pub enabled: bool,
}

/// Evaluate any catalog flag with explicit runtime availability and the current
/// Node process's boot value for restart-only flags. Unknown dynamic facts and
/// unknown boot values cannot enable a flag.
pub fn evaluate_with_context(
    feature: Feature,
    env: Option<bool>,
    stored_text: Option<&str>,
    availability: Availability,
    boot_value: Option<bool>,
) -> Evaluation {
    let definition = spec(feature);
    let (value, source) = match env {
        Some(value) => (value, Source::Env),
        None => match stored_text {
            Some("true") => (true, Source::Admin),
            Some("false") => (false, Source::Admin),
            _ => (definition.default, Source::Default),
        },
    };
    let available = definition.availability == AvailabilityKind::Static
        || availability == Availability::Available;
    let pending_restart = definition
        .restart
        .then(|| boot_value.map(|boot| boot != value))
        .flatten();
    let active = if definition.restart {
        boot_value.unwrap_or(false)
    } else {
        value
    };
    Evaluation {
        value,
        source,
        locked: source == Source::Env,
        available,
        pending_restart: if definition.restart {
            pending_restart
        } else {
            Some(false)
        },
        enabled: active && available,
    }
}

/// Compatibility API for the three native production consumers. Their current
/// settings are re-read by their existing callers, and only nativeClientAuth
/// needs the TRUST_PROXY availability input.
pub fn evaluate(
    feature: Feature,
    env: Option<bool>,
    stored_text: Option<&str>,
    trust_proxy: bool,
) -> Evaluation {
    let availability = if feature == Feature::NativeClientAuth {
        if trust_proxy {
            Availability::Available
        } else {
            Availability::Unavailable
        }
    } else {
        Availability::Unknown
    };
    evaluate_with_context(feature, env, stored_text, availability, None)
}
