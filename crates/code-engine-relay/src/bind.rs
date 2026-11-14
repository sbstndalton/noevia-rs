//! code-machine.cjs `startEngineRelayFromEnv` and `bindProblem` (#1876): where the relay listens.
//!
//! The relay listens on `CODE_ENGINE_RELAY_PORT` on web's own code-network address only, never on
//! every interface: `CODE_ENGINE_RELAY_BIND` when it is set (validated), else the address the relay's
//! host name resolves to (`CODE_ENGINE_RELAY_HOST`, else `CODE_EGRESS_HOST`, else `egress`); a name
//! that does not resolve binds loopback, which the sandbox cannot reach (fail closed). An address
//! that is not one of this machine's interface addresses keeps the relay off, and so does a listener
//! that cannot bind ([`crate::Relay::disable`]): Machine Code tasks then refuse with
//! `machineRelayOff`; web never fails to start because of the relay.
use js_json::JValue;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

/// `SERVER_LIMITS` (#1876): the sandbox shares the relay's network, so slow or token-less
/// connections must not pin it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Receiving the request head.
    pub headers_timeout: Duration,
    /// Receiving a whole request (not the streamed answer).
    pub request_timeout: Duration,
    pub keep_alive_timeout: Duration,
    pub max_connections: usize,
    pub connections_checking_interval: Duration,
}

pub const SERVER_LIMITS: Limits = Limits {
    headers_timeout: Duration::from_secs(10),
    request_timeout: Duration::from_secs(120),
    keep_alive_timeout: Duration::from_secs(5),
    max_connections: 32,
    connections_checking_interval: Duration::from_secs(1),
};

/// The address a name that does not resolve binds.
pub const LOOPBACK: &str = "127.0.0.1";

/// JS `String.prototype.trim`'s white space.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

/// Node `net.isIP(a) !== 0`: a dotted IPv4 address (no leading zeros) or an IPv6 address, which
/// may carry a zone (`fe80::1%eth0`).
fn is_ip(a: &str) -> bool {
    if a.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    match a.split_once('%') {
        Some((addr, zone)) => !zone.is_empty() && addr.parse::<Ipv6Addr>().is_ok(),
        None => a.parse::<Ipv6Addr>().is_ok(),
    }
}

/// `bindProblem(address, interfaces)`: why `address` may not be the relay's bind address, or `None`
/// when it is one of `interfaces` (this machine's interface addresses, `os.networkInterfaces()`).
pub fn bind_problem(address: &str, interfaces: &[String]) -> Option<&'static str> {
    let a = js_trim(address);
    if a.is_empty() {
        return Some("is empty");
    }
    if !is_ip(a) {
        return Some("is not an IP address");
    }
    // /^(0\.0\.0\.0|(0*:)+0*|::)$/: 0.0.0.0, or only zeros and colons with at least one colon.
    if a == "0.0.0.0" || (a.contains(':') && a.chars().all(|c| c == '0' || c == ':')) {
        return Some("would listen on every interface");
    }
    let norm = |x: &str| {
        let x = x.to_lowercase();
        match x.split_once('%') {
            Some((addr, _)) => addr.to_string(),
            None => x,
        }
    };
    let me = norm(a);
    if !interfaces.iter().any(|x| !x.is_empty() && norm(x) == me) {
        return Some("is not one of this machine's interface addresses");
    }
    None
}

/// Where the listener's address comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// `CODE_ENGINE_RELAY_BIND`, trimmed (set, even empty: then it is refused).
    Explicit(String),
    /// Resolve this host name.
    Lookup(String),
}

impl Bind {
    /// The address to listen on, given `lookup` for a name: a name that does not resolve binds
    /// [`LOOPBACK`], with the `relay.bind_fallback_loopback` log entry.
    pub fn address(
        &self,
        lookup: impl FnOnce(&str) -> Result<String, String>,
    ) -> (String, Option<JValue>) {
        match self {
            Bind::Explicit(a) => (a.clone(), None),
            Bind::Lookup(host) => match lookup(host) {
                Ok(a) => (a, None),
                Err(reason) => (
                    LOOPBACK.to_string(),
                    Some(JValue::obj([
                        ("event", JValue::Str("relay.bind_fallback_loopback".into())),
                        ("host", JValue::Str(host.clone())),
                        ("reason", JValue::Str(reason)),
                    ])),
                ),
            },
        }
    }
}

/// What the environment says about the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvRelay {
    /// `CODE_ENGINE_RELAY_PORT` when it is an integer in 1..=65535; `None`: no relay.
    pub port: Option<u16>,
    pub host: String,
    /// `http://{host}:{port}/v1`, the relay as the sandbox reaches it.
    pub url: Option<String>,
    pub bind: Bind,
}

/// JS `Number(text)` for the forms a port can take: white space trimmed, empty is 0, decimal
/// (sign, fraction, exponent), `0x`/`0o`/`0b`, `Infinity`; anything else is NaN.
fn js_number(text: &str) -> f64 {
    let t = js_trim(text);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |p: &str, r: u32| {
        t.get(2..)
            .filter(|d| !d.is_empty() && t.get(..2).is_some_and(|h| h.eq_ignore_ascii_case(p)))
            .map(|d| {
                d.chars().try_fold(0f64, |acc, c| {
                    c.to_digit(r).map(|v| acc * f64::from(r) + f64::from(v))
                })
            })
    };
    for (p, r) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(n) = radix(p, r) {
            return n.unwrap_or(f64::NAN);
        }
    }
    let body = t.strip_prefix(['+', '-']).unwrap_or(t);
    if body == "Infinity" {
        return if t.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // Rust also reads "inf", "nan" and friends; JS reads only digits, '.', and an exponent.
    if !body
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        || !body.starts_with(|c: char| c.is_ascii_digit() || c == '.')
    {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `startEngineRelayFromEnv`'s reading of the environment. `Err` is the start error Node throws
/// for a relay host that is not a host name (only with a port set).
pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<EnvRelay, String> {
    let port = get("CODE_ENGINE_RELAY_PORT").map_or(f64::NAN, |p| js_number(&p));
    let port = (port.fract() == 0.0 && (1.0..=65535.0).contains(&port)).then_some(port as u16);
    let pick = |k: &str| get(k).filter(|v| !v.is_empty());
    let host = pick("CODE_ENGINE_RELAY_HOST")
        .or_else(|| pick("CODE_EGRESS_HOST"))
        .unwrap_or_else(|| "egress".into());
    let host = js_trim(&host).to_string();
    if port.is_some()
        && (host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    {
        return Err(format!(
            "CODE_ENGINE_RELAY_HOST should be a host name, not \"{host}\""
        ));
    }
    let url = port.map(|p| format!("http://{host}:{p}/v1"));
    let bind = match get("CODE_ENGINE_RELAY_BIND") {
        Some(b) => Bind::Explicit(js_trim(&b).to_string()),
        None => Bind::Lookup(host.clone()),
    };
    Ok(EnvRelay {
        port,
        host,
        url,
        bind,
    })
}

/// The log entry for an address [`bind_problem`] refused (`relay.bind_rejected`).
pub fn rejected(port: u16, bind: &str, reason: &str) -> JValue {
    JValue::obj([
        ("event", JValue::Str("relay.bind_rejected".into())),
        ("port", JValue::Num(f64::from(port))),
        ("bind", JValue::Str(bind.into())),
        ("reason", JValue::Str(reason.into())),
    ])
}
