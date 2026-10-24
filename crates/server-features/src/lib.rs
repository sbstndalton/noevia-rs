//! The three product flags already read by native request paths.
//! Node's synchronous feature service remains the writer and public route owner.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Feature {
    NativeClientAuth,
    RoutingModes,
    ChatgptOAuth,
}

impl Feature {
    pub const ALL: [Self; 3] = [
        Self::NativeClientAuth,
        Self::RoutingModes,
        Self::ChatgptOAuth,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "nativeClientAuth" => Some(Self::NativeClientAuth),
            "routingModes" => Some(Self::RoutingModes),
            "chatgptOAuth" => Some(Self::ChatgptOAuth),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Spec {
    pub name: &'static str,
    pub env: &'static str,
    pub setting: &'static str,
    pub default: bool,
}

pub const fn spec(feature: Feature) -> Spec {
    match feature {
        Feature::NativeClientAuth => Spec {
            name: "nativeClientAuth",
            env: "NOEVIA_FEATURE_NATIVE_CLIENT_AUTH",
            setting: "feature:nativeClientAuth",
            default: false,
        },
        Feature::RoutingModes => Spec {
            name: "routingModes",
            env: "NOEVIA_FEATURE_ROUTING_MODES",
            setting: "feature:routingModes",
            default: false,
        },
        Feature::ChatgptOAuth => Spec {
            name: "chatgptOAuth",
            env: "NOEVIA_FEATURE_CHATGPT_OAUTH",
            setting: "feature:chatgptOAuth",
            default: false,
        },
    }
}

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
pub struct Evaluation {
    /// The selected setting, before runtime availability constrains it.
    pub value: bool,
    pub source: Source,
    pub locked: bool,
    pub available: bool,
    /// The answer to Node's enabled(name), which may differ from value.
    pub enabled: bool,
}

/// Resolve one current native flag from parsed env and an exact TEXT setting
/// value. Callers must read their existing authoritative setting on each use;
/// this function intentionally owns no cache, store, audit, or side effect.
pub fn evaluate(
    feature: Feature,
    env: Option<bool>,
    stored_text: Option<&str>,
    trust_proxy: bool,
) -> Evaluation {
    let (value, source) = match env {
        Some(value) => (value, Source::Env),
        None => match stored_text {
            Some("true") => (true, Source::Admin),
            Some("false") => (false, Source::Admin),
            _ => (spec(feature).default, Source::Default),
        },
    };
    let available = feature != Feature::NativeClientAuth || trust_proxy;
    Evaluation {
        value,
        source,
        locked: source == Source::Env,
        available,
        enabled: value && available,
    }
}
