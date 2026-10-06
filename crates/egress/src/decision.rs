//! The proxy's verdict (`code-egress.cjs` `check`), split around the one DNS lookup so the
//! caller owns the I/O: [`check_target`] decides everything that needs no resolution,
//! [`check_addresses`] judges the answers and picks the single address to connect to.
//!
//! Order and wording are the JS ones: 407 no valid token, 400 unreadable target, 403 port,
//! 403 host not granted, 502 no answers, 403 any private answer, 407 token gone meanwhile.

use std::net::IpAddr;

use crate::ip::{is_ip, is_private_ip};
use crate::policy::{host_allowed, parse_target};
use crate::tokens::{Expired, GrantId, TokenStore};

/// `STATUS_TEXT` in code-egress.cjs, with its `|| 'Forbidden'` fallback.
pub fn status_text(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        502 => "Bad Gateway",
        _ => "Forbidden",
    }
}

/// A deny, with the reason the refusal log and response body carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub reason: String,
    pub task_id: Option<String>,
    pub host: Option<String>,
}

impl Refusal {
    fn new(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            task_id: None,
            host: None,
        }
    }
    fn task(mut self, task_id: &str) -> Self {
        self.task_id = Some(task_id.to_owned());
        self
    }
    fn host(mut self, host: &str) -> Self {
        self.host = Some(host.to_owned());
        self
    }
}

/// The request passed every check that needs no DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub grant: GrantId,
    pub task_id: String,
    pub host: String,
    pub port: u16,
    /// The host is an IP literal: judge it as written, do not resolve it.
    pub literal: bool,
}

/// An allow: connect to exactly `address:port`, never to a fresh lookup of `host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub grant: GrantId,
    pub task_id: String,
    pub host: String,
    pub port: u16,
    pub address: IpAddr,
}

/// Token, target syntax, port and host allowlist. Sweeps expired grants into `expired`.
pub fn check_target(
    store: &mut TokenStore,
    proxy_authorization: Option<&[u8]>,
    target: Option<&str>,
    default_port: u16,
    allowed_ports: &[u16],
    now_ms: u64,
    expired: &mut Vec<Expired>,
) -> Result<Pending, Refusal> {
    let Some(g) = store.authorize(proxy_authorization, now_ms, expired) else {
        return Err(Refusal::new(407, "no valid task token"));
    };
    let Some(t) = target.and_then(|t| parse_target(t, default_port)) else {
        return Err(Refusal::new(400, "unreadable target").task(&g.task_id));
    };
    if !allowed_ports.contains(&t.port) {
        return Err(Refusal::new(403, format!("port {} is not allowed", t.port))
            .task(&g.task_id)
            .host(&t.host));
    }
    if !host_allowed(&t.host, &g.domains) {
        return Err(Refusal::new(403, "host is not on this task\u{2019}s list")
            .task(&g.task_id)
            .host(&t.host));
    }
    Ok(Pending {
        grant: g.id,
        task_id: g.task_id,
        literal: is_ip(&t.host) != 0,
        host: t.host,
        port: t.port,
    })
}

/// Judges the addresses for `pending`: the literal itself, or every resolver answer (an error
/// is passed as an empty list). Every answer must be public; the first is the one to use.
pub fn check_addresses(
    store: &TokenStore,
    pending: &Pending,
    resolved: &[IpAddr],
) -> Result<Allowed, Refusal> {
    let refuse = |status: u16, reason: &str| {
        Err(Refusal::new(status, reason)
            .task(&pending.task_id)
            .host(&pending.host))
    };
    let address = if pending.literal {
        if is_private_ip(&pending.host) {
            return refuse(403, "host resolves to a private address");
        }
        // Public per the JS text rule; std parses every zone-less literal Node accepts, so the
        // only failure left is a `%zone`, which is refused rather than guessed at.
        match pending.host.parse::<IpAddr>() {
            Ok(a) => a,
            Err(_) => return refuse(403, "host resolves to a private address"),
        }
    } else {
        let Some(first) = resolved.first() else {
            return refuse(502, "host does not resolve");
        };
        if resolved.iter().any(|a| is_private_ip(&a.to_string())) {
            return refuse(403, "host resolves to a private address");
        }
        *first
    };
    if !store.contains(pending.grant) {
        return refuse(407, "task token was revoked");
    }
    Ok(Allowed {
        grant: pending.grant,
        task_id: pending.task_id.clone(),
        host: pending.host.clone(),
        port: pending.port,
        address,
    })
}
