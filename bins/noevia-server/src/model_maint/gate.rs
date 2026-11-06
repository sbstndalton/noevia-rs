//! The inference gate's maintenance hold, from the Rust front's side (core
//! `inference-gate-client.cjs` `holdMaintenance` / `renewMaintenance` / `releaseMaintenance` and
//! `inference-maintenance.cjs`, sbstndalton/noevia#2187). The gate (`bins/noevia-inference-gate`)
//! is the one maintenance authority: a hold taken here is seen by every client of the router, Node
//! included, and Node's chat, which goes through the gate, waits for its end.
//!
//! A [`Hold`] always has an absolute deadline. While it lasts a task renews the gate's hold every
//! third of its idle limit (the gate ends a hold nobody renews), takes it again if the gate lost
//! it, and gives it back at the deadline. A hold dropped without [`Hold::release`] (a panic, a
//! cancelled task) gives its token back from `Drop`; if even that cannot run, the gate ends the
//! hold by itself after its idle limit. A dropped HTTP client never reaches this: the work runs on
//! its own spawned task, which owns the hold.
//!
//! Without an admin token, or when nothing at the engine address knows the hold (404), a hold is a
//! lease on Node's own maintenance gate instead ([`node_lease`](super::node_lease)): renewed the same
//! way, but a lease that ends is never taken again: [`Hold::may_start`] stops the work before any step
//! the lease is not sure to outlast, because Node's work may run once it ends.

use super::lease;
use super::node_lease::{LeaseGrant, NodeLeases};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ADMIN_HEADER: &str = "x-noevia-gate-admin";
const CLIENT_HEADER: &str = "x-noevia-client";
/// `RELEASE_TIMEOUT_MS`.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// A gate found without the hold API is asked again after this.
const REPROBE: Duration = Duration::from_secs(5 * 60);
/// How long a renewal that finds the hold gone waits for the engine when it takes the hold again.
const RETAKE_WAIT: Duration = Duration::from_secs(10);
const MAX_WAIT: Duration = Duration::from_secs(600);

/// A hold the gate granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub token: String,
    pub idle_ttl: Duration,
}

/// Why the gate gave no hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateFault {
    /// The gate answered with a refusal of its own (`gate_error`): HTTP status, code, text.
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    /// No (usable) answer: `engine_unavailable`, 502.
    Unavailable,
}

impl GateFault {
    /// 409 `engine_busy` and 503 `maintenance_held`: work is running or another client holds it.
    pub fn busy(&self) -> bool {
        matches!(self, GateFault::Refused { status, code, .. }
            if *status == 409 || code == "engine_busy" || code == "maintenance_held")
    }
    pub fn status(&self) -> u16 {
        match self {
            GateFault::Refused { status, .. } => *status,
            GateFault::Unavailable => 502,
        }
    }
    /// The gate's refusal code (`engine_unavailable` when it gave no usable answer).
    pub fn code(&self) -> String {
        match self {
            GateFault::Refused { code, .. } => code.clone(),
            GateFault::Unavailable => "engine_unavailable".into(),
        }
    }
    pub fn message(&self) -> String {
        match self {
            GateFault::Refused { message, .. } => message.clone(),
            GateFault::Unavailable => {
                "The local model engine is not answering. Try again shortly.".into()
            }
        }
    }
}

fn is_token(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

pub struct Gate {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    absent_at: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("base", &self.base)
            .field("admin", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl Gate {
    pub fn new(base: &str, admin_token: Option<&str>) -> Result<Gate, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("inference gate client: {e}"))?;
        Ok(Gate {
            client,
            base: base.to_owned(),
            token: admin_token
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            absent_at: Mutex::new(None),
        })
    }

    /// Whether this process can ask for holds at all (an admin token is set).
    pub fn has_admin(&self) -> bool {
        self.token.is_some()
    }

    fn absent(&self) -> bool {
        self.absent_at
            .lock()
            .map(|a| a.is_some_and(|t| t.elapsed() < REPROBE))
            .unwrap_or(false)
    }

    fn mark_absent(&self, absent: bool) {
        if let Ok(mut a) = self.absent_at.lock() {
            *a = absent.then(Instant::now);
        }
    }

    /// A JSON answer of the gate's own endpoints: (status, body when JSON).
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<(u16, Option<Value>), GateFault> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static(CLIENT_HEADER),
            HeaderValue::from_static("front"),
        );
        if let Some(t) = self
            .token
            .as_deref()
            .and_then(|t| HeaderValue::from_str(t).ok())
        {
            let mut t = t;
            t.set_sensitive(true);
            headers.insert(HeaderName::from_static(ADMIN_HEADER), t);
        }
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .headers(headers)
            .timeout(timeout);
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let res = req.send().await.map_err(|_| GateFault::Unavailable)?;
        let status = res.status().as_u16();
        // The gate's answers are small; a larger one is not the gate's.
        let bytes = res.bytes().await.map_err(|_| GateFault::Unavailable)?;
        if bytes.len() > 16 * 1024 {
            return Err(GateFault::Unavailable);
        }
        Ok((status, serde_json::from_slice(&bytes).ok()))
    }

    /// `refusalOf(status, body)`: the gate's own refusal, when the body is one.
    fn refusal(status: u16, body: &Option<Value>) -> Option<GateFault> {
        let e = body.as_ref()?.get("error")?;
        if e.get("type")?.as_str()? != "gate_error" {
            return None;
        }
        let code = e.get("code")?.as_str()?.to_owned();
        let message = e
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(|m| clip(m, 300))
            .unwrap_or_else(|| "The inference gate refused the request.".into());
        Some(GateFault::Refused {
            status,
            code,
            message,
        })
    }

    /// Takes the gate's maintenance hold, waiting up to `wait` for the engine to be idle.
    /// `Ok(None)`: no admin token, or nothing at the engine address knows the hold.
    pub async fn hold(&self, reason: &str, wait: Duration) -> Result<Option<Grant>, GateFault> {
        if self.token.is_none() || self.absent() {
            return Ok(None);
        }
        let wait = wait.min(MAX_WAIT);
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(0);
        let (status, body) = self
            .call(
                reqwest::Method::POST,
                "/gate/v1/maintenance",
                Some(serde_json::json!({ "reason": clip(reason, 300), "wait_ms": wait_ms })),
                wait + Duration::from_secs(10),
            )
            .await?;
        if status != 404 {
            self.mark_absent(false);
        }
        if status == 201 {
            if let Some(token) = body
                .as_ref()
                .and_then(|b| b.get("hold"))
                .and_then(Value::as_str)
                .filter(|t| is_token(t))
            {
                let ttl = body
                    .as_ref()
                    .and_then(|b| b.get("idle_ttl_ms"))
                    .and_then(Value::as_u64)
                    .filter(|v| *v > 0)
                    .unwrap_or(60_000);
                return Ok(Some(Grant {
                    token: token.to_owned(),
                    idle_ttl: Duration::from_millis(ttl),
                }));
            }
        }
        let refusal = Self::refusal(status, &body);
        if status == 404
            && refusal
                .as_ref()
                .is_none_or(|r| matches!(r, GateFault::Refused { code, .. } if code == "not_found"))
        {
            self.mark_absent(true);
            return Ok(None);
        }
        Err(refusal.unwrap_or(GateFault::Unavailable))
    }

    /// Keeps the hold from expiring. `Some(true)`: renewed; `Some(false)`: it has ended; `None`: no answer.
    pub async fn renew(&self, token: &str, reason: Option<&str>) -> Option<bool> {
        if !is_token(token) {
            return Some(false);
        }
        let body = reason
            .map(|r| serde_json::json!({ "reason": clip(r, 300) }))
            .unwrap_or_else(|| serde_json::json!({}));
        match self
            .call(
                reqwest::Method::POST,
                &format!("/gate/v1/maintenance/{token}/renew"),
                Some(body),
                CALL_TIMEOUT,
            )
            .await
        {
            Ok((200, _)) => Some(true),
            Ok((404, _)) => Some(false),
            _ => None,
        }
    }

    /// Gives the hold back; best effort (the gate ends an unrenewed hold by itself).
    pub async fn release(&self, token: &str) {
        if is_token(token) {
            let _ = self
                .call(
                    reqwest::Method::DELETE,
                    &format!("/gate/v1/maintenance/{token}"),
                    None,
                    CALL_TIMEOUT,
                )
                .await;
        }
    }

    /// The admin header (`inference-gate-client.cjs` `adminHeaders()`), when a token is set: what a
    /// forced unload that may end a request in progress carries.
    pub fn admin_header(&self) -> Option<(HeaderName, HeaderValue)> {
        let mut value = HeaderValue::from_str(self.token.as_deref()?).ok()?;
        value.set_sensitive(true);
        Some((HeaderName::from_static(ADMIN_HEADER), value))
    }

    /// Whether any client holds the gate's maintenance hold now (`GET /gate/v1/status`).
    pub async fn held(&self) -> Option<bool> {
        match self
            .call(reqwest::Method::GET, "/gate/v1/status", None, CALL_TIMEOUT)
            .await
        {
            Ok((200, Some(b))) => b
                .get("maintenance")
                .and_then(|m| m.get("held"))
                .and_then(Value::as_bool),
            _ => None,
        }
    }
}

/// Added to a step's limit for what it does around its router call (reading models.ini).
const STEP_MARGIN: Duration = Duration::from_secs(15);

const RELEASE_LIMIT: Duration = Duration::from_secs(30);
const EXPIRY_MARGIN: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq)]
enum ReleaseState {
    Idle,
    Pending,
    Acknowledged,
    Expired,
    Unconfirmed,
}

struct Shared {
    /// The gate's hold token, or the id of Node's lease.
    token: Mutex<Option<String>>,
    released: AtomicBool,
    /// Wakes the renewer when the hold ends, so it stops by itself (and gives back a hold it was
    /// retaking at that moment) rather than being aborted mid-call.
    wake: tokio::sync::Notify,
    deadline: Mutex<Instant>,
    /// Node's lease only: when Node last confirmed it, and how long that confirmation lasts.
    confirmed: Mutex<Instant>,
    lease_ttl: Option<Duration>,
    /// When Node ends the lease whatever happens.
    lease_end: Option<Instant>,
    lost: AtomicBool,
    // Independent lease identity: taking the renewal token never proves DELETE completed.
    lease_id: Option<String>,
    node_generation: u64,
    // Upper bound measured after the grant arrived, unlike confirmed/lease_end (lower bounds).
    expires_by: Option<Instant>,
    release: Mutex<ReleaseState>,
    release_wake: tokio::sync::Notify,
    /// The authority's lease this hold is (row J): its fence. `None` for the gate's hold.
    auth: Option<(lease::Authority, lease::Grant)>,
}

impl Shared {
    fn new(
        token: Option<String>,
        deadline: Instant,
        lease_ttl: Option<Duration>,
        lease_end: Option<Instant>,
        expires_by: Option<Instant>,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            lease_id: lease_ttl.and(token.clone()),
            node_generation: 0,
            expires_by,
            release: Mutex::new(ReleaseState::Idle),
            release_wake: tokio::sync::Notify::new(),
            token: Mutex::new(token),
            released: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
            deadline: Mutex::new(deadline),
            confirmed: Mutex::new(Instant::now()),
            lease_ttl,
            lease_end,
            lost: AtomicBool::new(false),
            auth: None,
        })
    }

    /// Whether the authority still holds this lease, covering `step` from now (no authority: true).
    fn fenced(&self, step: Duration) -> bool {
        self.auth
            .as_ref()
            .is_none_or(|(a, g)| a.covers(&g.id, g.generation, step))
    }

    /// Gives the authority's lease back at once; idempotent, synchronous.
    fn release_authority(&self) {
        if let Some((a, g)) = self.auth.as_ref() {
            a.release(&g.id, Some(g.generation));
        }
    }

    fn deadline(&self) -> Instant {
        self.deadline
            .lock()
            .map(|d| *d)
            .unwrap_or_else(|_| Instant::now())
    }

    /// Sleeps `every` or to the deadline, or until the hold ends. False when it has ended.
    async fn pause(&self, every: Duration) -> bool {
        let left = self.deadline().saturating_duration_since(Instant::now());
        tokio::select! {
            () = tokio::time::sleep(every.min(left)) => {}
            () = self.wake.notified() => {}
        }
        !self.released.load(Ordering::Acquire)
    }
}

impl Shared {
    fn start_release(self: &Arc<Self>, node: Arc<NodeLeases>) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut state = match self.release.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            if *state != ReleaseState::Idle {
                return;
            }
            *state = ReleaseState::Pending;
        }
        let needs_recovery = self.lost.load(Ordering::Acquire)
            || !self.fenced(Duration::ZERO)
            || self
                .auth
                .as_ref()
                .is_some_and(|(a, _)| a.recovery_pending());
        let recovery_ticket = if needs_recovery {
            Some(self.auth.as_ref().map_or_else(
                || self.lease_id.clone().unwrap_or_default(),
                |(a, _)| a.require_recovery(),
            ))
        } else {
            None
        };
        self.release_authority();
        self.released.store(true, Ordering::Release);
        self.wake.notify_one();
        let shared = Arc::clone(self);
        rt.spawn(async move {
            let until = Instant::now() + RELEASE_LIMIT;
            let mut wait = Duration::from_millis(100);
            let outcome = loop {
                let now = Instant::now();
                // Acknowledged barrier precedes relinquishing the old latch. If Node is down,
                // authority registration reconstructs exclusion; retry without opening admission.
                let still_required = shared
                    .auth
                    .as_ref()
                    .is_none_or(|(a, _)| a.recovery_pending());
                if now >= until {
                    break ReleaseState::Unconfirmed;
                }
                if needs_recovery
                    && still_required
                    && !node
                        .recovery_barrier(true, recovery_ticket.as_deref().unwrap_or_default())
                        .await
                {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                if shared.expires_by.is_some_and(|at| now >= at) {
                    break ReleaseState::Expired;
                }
                if now >= until {
                    break ReleaseState::Unconfirmed;
                }
                let Some(id) = shared.lease_id.as_deref() else {
                    break ReleaseState::Unconfirmed;
                };
                let end = shared.expires_by.map_or(until, |at| at.min(until));
                if tokio::time::timeout_at(end.into(), node.release_confirmed(id))
                    .await
                    .is_ok_and(|ack| ack)
                {
                    break ReleaseState::Acknowledged;
                }
                tokio::time::sleep(wait.min(end.saturating_duration_since(Instant::now()))).await;
                wait = (wait * 2).min(Duration::from_secs(1));
            };
            match shared.release.lock() {
                Ok(mut s) => *s = outcome,
                Err(p) => *p.into_inner() = outcome,
            }
            shared.release_wake.notify_waiters();
        });
    }
}

/// Local admission transferred to a durable finalization fence. Dropping a task must not
/// clear it: only verified finalization/recovery explicitly releases this reservation.
pub(super) struct AdmissionFence {
    local: Arc<AtomicBool>,
}
impl AdmissionFence {
    pub(super) fn reserve(local: Arc<AtomicBool>) -> Option<Self> {
        local
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self { local })
    }
    pub(super) fn release(self) {
        self.local.store(false, Ordering::Release);
    }
}

/// A maintenance hold. Always bounded by a deadline; `release().await` ends it, `Drop` ends it too.
pub struct Hold {
    shared: Arc<Shared>,
    gate: Option<Arc<Gate>>,
    node: Option<Arc<NodeLeases>>,
    local: Arc<AtomicBool>,
    reason: String,
    local_finished: bool,
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hold")
            .field("gate", &self.token().is_some())
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl Hold {
    pub(super) fn recovery_proof(&self) -> Option<(&str, u64)> {
        self.shared
            .lease_id
            .as_deref()
            .map(|id| (id, self.shared.node_generation))
    }

    pub(super) fn defer_local_release(&mut self) -> AdmissionFence {
        self.local_finished = true;
        AdmissionFence {
            local: Arc::clone(&self.local),
        }
    }

    /// A lease on Node's gate, renewed until `deadline`.
    pub(super) fn leased(
        local: Arc<AtomicBool>,
        node: Arc<NodeLeases>,
        grant: LeaseGrant,
        auth: Option<(lease::Authority, lease::Grant)>,
        reason: &str,
        deadline: Instant,
    ) -> Hold {
        let mut shared = Shared::new(
            Some(grant.id),
            deadline,
            Some(grant.ttl),
            Some(grant.asked + grant.hard),
            Some(Instant::now() + grant.hard + EXPIRY_MARGIN),
        );
        if let Some(inner) = Arc::get_mut(&mut shared) {
            inner.auth = auth;
            inner.node_generation = grant.generation;
        }
        if let Ok(mut at) = shared.confirmed.lock() {
            *at = grant.asked;
        }
        let every = grant.renew_every;
        tokio::spawn(lease_loop(Arc::clone(&shared), Arc::clone(&node), every));
        Hold {
            shared,
            gate: None,
            node: Some(node),
            local,
            reason: reason.to_owned(),
            local_finished: false,
        }
    }

    /// A hold on the gate's grant, renewed until `deadline`.
    pub(super) fn gated(
        local: Arc<AtomicBool>,
        gate: Arc<Gate>,
        grant: Grant,
        reason: &str,
        deadline: Instant,
    ) -> Hold {
        let shared = Shared::new(Some(grant.token), deadline, None, None, None);
        let every = (grant.idle_ttl / 3).max(Duration::from_secs(1));
        tokio::spawn(renew_loop(
            Arc::clone(&shared),
            Arc::clone(&gate),
            every,
            reason.to_owned(),
        ));
        Hold {
            shared,
            gate: Some(gate),
            node: None,
            local,
            reason: reason.to_owned(),
            local_finished: false,
        }
    }

    /// The gate's hold token, while there is one: the header this process's requests to the
    /// engine carry (`X-Noevia-Maintenance`).
    pub fn token(&self) -> Option<String> {
        if self.node.is_some() {
            return None;
        }
        self.shared.token.lock().ok().and_then(|t| t.clone())
    }

    /// May a step of at most `limit` start? For Node's lease: only if it has not ended and is sure to
    /// outlast the step: Node's last confirmation and its hard limit both cover it, so the lease cannot
    /// end under a call in flight, even if Node stops hearing renewals now. Always true for the gate's
    /// hold, which goes on as it always did.
    pub fn may_start(&self, limit: Duration) -> bool {
        let Some(ttl) = self.shared.lease_ttl else {
            return true;
        };
        let covered = |until: Instant| Instant::now() + limit + STEP_MARGIN < until;
        !self.shared.lost.load(Ordering::Acquire)
            && self.shared.fenced(limit + STEP_MARGIN)
            && self
                .shared
                .confirmed
                .lock()
                .is_ok_and(|at| covered(*at + ttl))
            && self.shared.lease_end.is_none_or(covered)
    }

    /// Whether the hold still excludes everyone else: not lost, and for Node's lease, Node's last
    /// confirmation still covers now (an unconfirmed lease may already have ended at Node).
    pub fn excluded(&self) -> bool {
        if self.shared.lost.load(Ordering::Acquire) || self.shared.released.load(Ordering::Acquire)
        {
            return false;
        }
        let Some(ttl) = self.shared.lease_ttl else {
            return true;
        };
        let now = Instant::now();
        self.shared.fenced(Duration::ZERO)
            && self.shared.confirmed.lock().is_ok_and(|at| now < *at + ttl)
            && self.shared.lease_end.is_none_or(|end| now < end)
    }

    /// Owned exact-hold fence for asynchronous CAS resolvers. It never follows a replacement lease.
    pub(super) fn write_fence(&self) -> super::ini::WriteAllowed {
        let shared = Arc::clone(&self.shared);
        Arc::new(move || {
            !shared.lost.load(Ordering::Acquire)
                && !shared.released.load(Ordering::Acquire)
                && shared.fenced(Duration::ZERO)
                && shared.lease_ttl.is_none_or(|ttl| {
                    shared
                        .confirmed
                        .lock()
                        .is_ok_and(|at| Instant::now() < *at + ttl)
                })
                && shared.lease_end.is_none_or(|end| Instant::now() < end)
        })
    }

    /// Makes the deadline at least `needed` from now: the work has found out how long it may take.
    pub fn cover(&self, needed: Duration) {
        if let Ok(mut d) = self.shared.deadline.lock() {
            *d = (*d).max(Instant::now() + needed);
        }
    }

    /// Whether the gate (not just this process) holds the engine.
    pub fn gated_now(&self) -> bool {
        self.token().is_some()
    }

    /// Updates the reason the gate shows people.
    pub async fn set_reason(&self, reason: &str) {
        if let (Some(gate), Some(token)) = (self.gate.as_ref(), self.token()) {
            let _ = gate.renew(&token, Some(reason)).await;
        }
    }

    /// Join the same release operation as the deadline renewer. An absent renewal token is
    /// not an acknowledgment; only DELETE204 or the grant's conservative hard-expiry bound is.
    pub(super) async fn release_node_lease(&mut self) {
        let Some(node) = self.node.as_ref() else {
            return;
        };
        self.shared.start_release(Arc::clone(node));
        let until = Instant::now() + RELEASE_LIMIT;
        loop {
            let notified = self.shared.release_wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self
                .shared
                .release
                .lock()
                .map(|s| *s)
                .unwrap_or(ReleaseState::Unconfirmed);
            if !matches!(state, ReleaseState::Idle | ReleaseState::Pending) {
                return;
            }
            let wake = self
                .shared
                .expires_by
                .map_or(until, |expiry| expiry.min(until));
            if tokio::time::timeout_at(wake.into(), notified)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    pub(super) fn node_release_confirmed(&self) -> bool {
        self.node.is_none()
            || self
                .shared
                .expires_by
                .is_some_and(|end| Instant::now() >= end)
            || self
                .shared
                .release
                .lock()
                .is_ok_and(|s| matches!(*s, ReleaseState::Acknowledged | ReleaseState::Expired))
    }

    pub(super) fn lost(&self) -> bool {
        self.shared.lost.load(Ordering::Acquire)
    }

    /// Ends the hold: the gate's token is given back, the local flag cleared.
    pub async fn release(mut self) {
        self.finish();
        let token = self.shared.token.lock().ok().and_then(|mut t| t.take());
        if let (Some(gate), Some(token)) = (self.gate.as_ref(), token.as_ref()) {
            gate.release(token).await;
        }
        if self.node.is_some() {
            self.release_node_lease().await;
        }
    }

    fn finish(&mut self) {
        self.shared.released.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        if !self.local_finished {
            self.local.store(false, Ordering::Release);
            self.local_finished = true;
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.finish();
        if let Some(node) = self.node.take() {
            self.shared.start_release(node);
            return;
        }
        let token = self.shared.token.lock().ok().and_then(|mut t| t.take());
        let Some(token) = token else {
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if let Some(gate) = self.gate.take() {
            rt.spawn(async move { gate.release(&token).await });
        }
    }
}

async fn renew_loop(shared: Arc<Shared>, gate: Arc<Gate>, every: Duration, reason: String) {
    loop {
        if Instant::now() >= shared.deadline() {
            break;
        }
        if !shared.pause(every).await {
            return;
        }
        // A later `cover` may have moved it.
        if Instant::now() >= shared.deadline() {
            break;
        }
        let Some(token) = shared.token.lock().ok().and_then(|t| t.clone()) else {
            return;
        };
        if gate.renew(&token, None).await == Some(false) {
            // The gate has forgotten the hold: take it again, briefly waiting for the engine.
            match gate.hold(&reason, RETAKE_WAIT).await {
                Ok(Some(g)) => {
                    if shared.released.load(Ordering::Acquire) {
                        gate.release(&g.token).await;
                        return;
                    }
                    if let Ok(mut t) = shared.token.lock() {
                        *t = Some(g.token);
                    }
                    eprintln!("[inference-gate] the gate had lost this process's maintenance hold; it was taken again");
                }
                _ => {
                    if let Ok(mut t) = shared.token.lock() {
                        *t = None;
                    }
                    shared.lost.store(true, Ordering::Release);
                    eprintln!("[inference-gate] the gate lost this process's maintenance hold and it could not be taken again; this work goes on without it, so other clients may use the engine until it ends");
                    return;
                }
            }
        }
    }
    // The deadline: the work is overdue. The hold is given back so it cannot outlive its work.
    shared.lost.store(true, Ordering::Release);
    let token = shared.token.lock().ok().and_then(|mut t| t.take());
    if let Some(token) = token {
        eprintln!("[inference-gate] a maintenance hold passed its deadline and was given back");
        gate.release(&token).await;
    }
}

async fn lease_loop(shared: Arc<Shared>, node: Arc<NodeLeases>, every: Duration) {
    loop {
        if !shared.pause(every).await {
            return;
        }
        if Instant::now() >= shared.deadline() {
            break;
        }
        let Some(id) = shared.token.lock().ok().and_then(|t| t.clone()) else {
            return;
        };
        if let Some((a, g)) = shared.auth.as_ref() {
            if !a.renew(&g.id, Some(g.generation)) {
                shared.lost.store(true, Ordering::Release);
                eprintln!("[models] the maintenance lease authority ended this front's lease; the work stops before its next step");
                break;
            }
        }
        let asked = Instant::now();
        match node.renew(&id).await {
            Some(true) => {
                if let Ok(mut at) = shared.confirmed.lock() {
                    *at = asked;
                }
            }
            Some(false) => {
                shared.lost.store(true, Ordering::Release);
                eprintln!("[models] Node ended this front's maintenance lease; the work stops before its next step");
                return;
            }
            // Not answered: `Hold::may_start` refuses once the last confirmation cannot cover a step.
            None => {}
        }
    }
    shared.lost.store(true, Ordering::Release);
    shared.start_release(node);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // REVIEW_RELEASE_TESTS_BEGIN
    async fn review_release_hold(
        hard: Duration,
        deadline: Duration,
    ) -> (
        Hold,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        Arc<AtomicBool>,
    ) {
        use axum::{
            body::Body,
            http::{Request, Response},
        };
        let seen = Arc::new(tokio::sync::Notify::new());
        let proceed = Arc::new(tokio::sync::Notify::new());
        let ack = Arc::new(AtomicBool::new(false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (a, b, c) = (Arc::clone(&seen), Arc::clone(&proceed), Arc::clone(&ack));
        let app = axum::Router::new().fallback(move |r: Request<Body>| {
            let (seen, proceed, ack) = (Arc::clone(&a), Arc::clone(&b), Arc::clone(&c));
            async move {
                if r.uri().path().contains("/recovery/") {
                    Response::builder().status(204).body(Body::empty()).unwrap()
                } else if r.method() == "DELETE" {
                    seen.notify_one();
                    proceed.notified().await;
                    ack.store(true, Ordering::Release);
                    Response::builder().status(204).body(Body::empty()).unwrap()
                } else {
                    Response::builder()
                        .status(200)
                        .body(Body::from("{}"))
                        .unwrap()
                }
            }
        });
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let node = Arc::new(
            NodeLeases::new(Some(&format!("http://127.0.0.1:{port}")), Some("synthetic")).unwrap(),
        );
        let hold = Hold::leased(
            Arc::new(AtomicBool::new(true)),
            node,
            LeaseGrant {
                id: "fedcba9876543210fedcba9876543210".into(),
                generation: 1,
                ttl: hard,
                renew_every: Duration::from_millis(5),
                hard,
                asked: Instant::now(),
            },
            None,
            "synthetic",
            Instant::now() + deadline,
        );
        (hold, seen, proceed, ack)
    }

    #[tokio::test]
    async fn review_deadline_delete_is_joined_before_completion() {
        let (mut hold, seen, proceed, ack) =
            review_release_hold(Duration::from_secs(5), Duration::from_millis(10)).await;
        tokio::time::timeout(Duration::from_secs(1), seen.notified())
            .await
            .unwrap();
        let join = tokio::spawn(async move {
            hold.release_node_lease().await;
            hold
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!join.is_finished(), "deadline DELETE is still in flight");
        proceed.notify_one();
        let _hold = tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .unwrap()
            .unwrap();
        assert!(ack.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn review_confirmed_hard_expiry_ends_delete_wait() {
        let (mut hold, _seen, _proceed, ack) =
            review_release_hold(Duration::from_millis(30), Duration::from_secs(1)).await;
        let started = Instant::now();
        tokio::time::timeout(Duration::from_millis(500), hold.release_node_lease())
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(!ack.load(Ordering::Acquire));
    }
    // REVIEW_RELEASE_TESTS_END

    #[test]
    fn tokens_are_32_lowercase_hex() {
        assert!(is_token("0123456789abcdef0123456789abcdef"));
        assert!(!is_token("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_token("short"));
    }

    #[test]
    fn a_gate_refusal_is_read_from_its_error_shape() {
        let body = Some(
            serde_json::json!({"error": {"type": "gate_error", "code": "engine_busy", "message": "x", "retryable": true}}),
        );
        let r = Gate::refusal(409, &body).unwrap();
        assert!(r.busy());
        assert_eq!(r.status(), 409);
        assert!(Gate::refusal(409, &Some(serde_json::json!({"error": "plain"}))).is_none());
        let held = Gate::refusal(
            503,
            &Some(serde_json::json!({"error": {"type": "gate_error", "code": "maintenance_held"}})),
        )
        .unwrap();
        assert!(held.busy());
        assert_eq!(held.message(), "The inference gate refused the request.");
        let admin = Gate::refusal(
            403,
            &Some(serde_json::json!({"error": {"type": "gate_error", "code": "admin_required", "message": "m"}})),
        )
        .unwrap();
        assert!(!admin.busy());
    }
}
