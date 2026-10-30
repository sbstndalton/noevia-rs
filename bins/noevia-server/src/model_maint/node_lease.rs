//! The front's lease on Node's own maintenance gate (core `model-maintenance-lease.cjs`,
//! sbstndalton/noevia#2210). With no inference gate (the normal production case) nothing at the engine
//! address arbitrates between this front's reload and Node's chat, tune, calibration, sweep, idle-unload,
//! delete and prune. While chat lives in Node, Node's in-process gate is that arbiter: a lease is a hold of
//! it. Node refuses it with 409 while its work runs, and holds its own work back while the lease lasts.
//!
//! The call goes to Node's loopback address only, with the per-start secret the web supervisor hands both
//! processes. If Node cannot be reached the reload fails closed with nothing changed. The lease ends by
//! itself if it is not renewed, so a front that dies frees Node.

use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use std::time::{Duration, Instant};

const PATH: &str = "/internal/model-maintenance/lease";
const TOKEN_HEADER: &str = "x-noevia-store-token";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// A lease Node granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGrant {
    pub id: String,
    pub ttl: Duration,
    pub renew_every: Duration,
    /// Node ends the lease this long after granting it, however often it is renewed.
    pub hard: Duration,
    /// Just before asking: Node's clocks started no earlier, so times counted from here are never long.
    pub asked: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseFault {
    /// Node's own work is running, or something already holds its gate.
    Busy,
    /// No lease: Node did not answer, or answered anything else.
    Unavailable,
}

pub struct NodeLeases {
    client: reqwest::Client,
    base: String,
    token: Option<HeaderValue>,
}

impl std::fmt::Debug for NodeLeases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeLeases")
            .field("base", &self.base)
            .field("token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

fn is_lease_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl NodeLeases {
    /// `base`: Node's loopback address (`NOEVIA_LEGACY_UPSTREAM`, already validated by the config).
    pub fn new(base: Option<&str>, token: Option<&str>) -> Result<NodeLeases, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("model maintenance lease client: {e}"))?;
        let token = token
            .filter(|t| !t.is_empty())
            .and_then(|t| HeaderValue::from_str(t).ok())
            .map(|mut t| {
                t.set_sensitive(true);
                t
            });
        Ok(NodeLeases {
            client,
            base: base.unwrap_or("").trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> Option<reqwest::RequestBuilder> {
        if self.base.is_empty() {
            return None;
        }
        let token = self.token.clone()?;
        Some(
            self.client
                .request(method, format!("{}{path}", self.base))
                .header(HeaderName::from_static(TOKEN_HEADER), token)
                .timeout(CALL_TIMEOUT),
        )
    }

    pub async fn acquire(&self) -> Result<LeaseGrant, LeaseFault> {
        let asked = Instant::now();
        let req = self
            .request(reqwest::Method::POST, PATH)
            .ok_or(LeaseFault::Unavailable)?;
        let res = req.send().await.map_err(|_| LeaseFault::Unavailable)?;
        match res.status().as_u16() {
            201 => {
                let bytes = res.bytes().await.map_err(|_| LeaseFault::Unavailable)?;
                let body: Value =
                    serde_json::from_slice(&bytes).map_err(|_| LeaseFault::Unavailable)?;
                let id = body
                    .get("lease")
                    .and_then(Value::as_str)
                    .filter(|l| is_lease_id(l))
                    .ok_or(LeaseFault::Unavailable)?;
                let ttl = body
                    .get("ttlMs")
                    .and_then(Value::as_u64)
                    .filter(|t| *t > 0)
                    .ok_or(LeaseFault::Unavailable)?;
                let ms = |key: &str| {
                    body.get(key)
                        .and_then(Value::as_u64)
                        .filter(|t| *t > 0)
                        .map(Duration::from_millis)
                        .ok_or(LeaseFault::Unavailable)
                };
                Ok(LeaseGrant {
                    id: id.to_owned(),
                    ttl: Duration::from_millis(ttl),
                    renew_every: ms("renewMs")?,
                    hard: ms("hardMs")?,
                    asked,
                })
            }
            409 => Err(LeaseFault::Busy),
            _ => Err(LeaseFault::Unavailable),
        }
    }

    /// `Some(true)`: renewed; `Some(false)`: the lease has ended; `None`: no answer.
    pub async fn renew(&self, id: &str) -> Option<bool> {
        let res = self
            .request(reqwest::Method::POST, &format!("{PATH}/{id}/renew"))?
            .send()
            .await
            .ok()?;
        match res.status().as_u16() {
            200 => Some(true),
            404 => Some(false),
            _ => None,
        }
    }

    /// Best effort: an unreleased lease ends by itself.
    pub async fn release(&self, id: &str) {
        if let Some(req) = self.request(reqwest::Method::DELETE, &format!("{PATH}/{id}")) {
            let _ = req.send().await;
        }
    }
}
