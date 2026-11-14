//! The provider checks of `rust-chat-native.cjs` `refusal()` (`provider`, then `provider-key`),
//! and what a front that runs this crate's call path may lift.
//!
//! Lifting is all or nothing for a row: `provider-key` (a turn that sends a key) goes only together
//! with `provider` (a non-default or external row), never alone. Inference machines, the sign-in
//! provider and route targets stay refused in this slice: they have their own transports
//! ([`crate::target::Fetch`]) and their own checks (machine readiness, the broker, the cloud grant).
//! The sign-in provider is lifted too, but only for a front that also names [`SIGNIN_CAP`] (it runs
//! the broker's transport itself); machines and route targets stay refused in every case, and so
//! do external keyed rows and the default row's key (core's `plainProviderRow`, noevia#2761).

/// The caps token a front sends when it runs this call path (not sent by any front yet; core
/// does not know it yet, so it changes nothing).
pub const CAP: &str = "providers";

/// The caps token of a front that runs the sign-in provider's transport (the broker's fetch) for a
/// turn itself. It lifts the sign-in row only together with [`CAP`]: the sign-in row is a provider
/// row too, so alone it lifts nothing.
pub const SIGNIN_CAP: &str = "signin-transport-v1";

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
    if f.provider_id != f.default_provider_id
        || f.machine
        || f.external
        || f.sign_in
        || f.route_target
    {
        return Some("provider");
    }
    if f.authorized {
        return Some("provider-key");
    }
    None
}

/// The reason for a front that sends [`CAP`], as core's `refusal()` answers it with that front's
/// lift (noevia-core `rust-chat-native.cjs` `frontRow`, council p8-3 + p10-1; noevia#2761):
/// - a plain provider row (`plainProviderRow`: non-default, no machine, not external, not the
///   sign-in row, no route target) runs natively, its key included;
/// - with [`SIGNIN_CAP`] as well, the sign-in row (`signinRow`: non-default, no machine, no route
///   target; external is its kind's own) runs natively too, as core lifts it with its
///   `NOEVIA_RUST_SIGNIN_TRANSPORT` switch on (this front cannot see core's switch: with it off
///   core keeps the row, which only keeps more turns in Node);
/// - every other row answers as [`node_reason`]: `provider` for a non-default, machine, external,
///   sign-in or route-target row, else `provider-key` for a turn that sends a key (the default
///   row's key stays core's).
///
/// Without [`CAP`], exactly [`node_reason`] ([`SIGNIN_CAP`] alone lifts nothing).
pub fn with_caps(f: &Facts, caps: &[&str]) -> Option<&'static str> {
    if !caps.contains(&CAP) {
        return node_reason(f);
    }
    if front_row(f, caps.contains(&SIGNIN_CAP)) {
        return None;
    }
    node_reason(f)
}

/// core `frontRow` for a front naming [`CAP`] (`signin`: it names [`SIGNIN_CAP`] too).
fn front_row(f: &Facts, signin: bool) -> bool {
    let other_row = f.provider_id != f.default_provider_id && !f.machine && !f.route_target;
    let plain = other_row && !f.external && !f.sign_in;
    let signin_row = signin && other_row && f.sign_in;
    plain || signin_row
}

/// The caps header's tokens as core reads them (`a,b,c`, trimmed, empty ones dropped); a header
/// over 1000 UTF-16 units names no caps at all.
pub fn caps(header: &str) -> Vec<&str> {
    if js_json::js_len(header) > 1000 {
        return Vec::new();
    }
    header
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}
