//! The provider checks of `rust-chat-native.cjs` `refusal()` (`provider`, then `provider-key`),
//! and what a front that runs this crate's call path may lift.
//!
//! Lifting is all or nothing for a row: `provider-key` (a turn that sends a key) goes only together
//! with `provider` (a non-default or external row), never alone. Inference machines, the sign-in
//! provider and route targets stay refused in this slice: they have their own transports
//! ([`crate::target::Fetch`]) and their own checks (machine readiness, the broker, the cloud grant).

/// The caps token a front sends when it runs this call path (not sent by any front yet; core
/// does not know it yet, so it changes nothing).
pub const CAP: &str = "providers";

/// The facts `refusal()` reads for these two checks (`preparedTurn`'s `t`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// `t.providerId` (absent: `None`; Node's `!==` then compares with the default id's text).
    pub provider_id: Option<String>,
    pub default_provider_id: Option<String>,
    /// Truthiness of `t.machine`, `t.external`, `t.chatgpt`, `t.routeTarget`, `t.authorized`.
    pub machine: bool,
    pub external: bool,
    pub sign_in: bool,
    pub route_target: bool,
    pub authorized: bool,
}

/// Node's answer for these two checks, in its order.
pub fn node_reason(f: &Facts) -> Option<&'static str> {
    if f.provider_id != f.default_provider_id || f.machine || f.external || f.sign_in || f.route_target {
        return Some("provider");
    }
    if f.authorized {
        return Some("provider-key");
    }
    None
}

/// The reason for a front that sends [`CAP`]: a provider row (non-default, external, with or
/// without a key) runs natively; a machine, the sign-in provider or a route target still answers
/// `provider`. Without the token, exactly [`node_reason`].
pub fn with_caps(f: &Facts, caps: &[&str]) -> Option<&'static str> {
    if !caps.contains(&CAP) {
        return node_reason(f);
    }
    if f.machine || f.sign_in || f.route_target {
        return Some("provider");
    }
    None
}

/// The caps header's tokens as core reads them (`a,b,c`, trimmed, empty ones dropped); a header
/// over 1000 UTF-16 units names no caps at all.
pub fn caps(header: &str) -> Vec<&str> {
    if js_json::js_len(header) > 1000 {
        return Vec::new();
    }
    header.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
}
