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
use super::recovery::{Registry, SETTLE};
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

/// `X-Noevia-Lease`: the header a request carries to run under a lease.
pub const LEASE_HEADER: &str = "x-noevia-lease";
/// The gate's event stream is not larger than this per event (`MAX_EVENT_BYTES`); a longer one is not the gate's.
const MAX_EVENT_BYTES: usize = 16 * 1024;
/// The longest a lease request is left open (the gate's own queue limit ends it sooner).
const LEASE_WAIT: Duration = Duration::from_secs(15 * 60);

/// A refusal of a lease (`GateError`): the gate's own, or the engine not answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRefusal {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl LeaseRefusal {
    fn unavailable() -> Self {
        LeaseRefusal {
            status: 502,
            code: "engine_unavailable".into(),
            message: "The local model engine is not answering. Try again shortly.".into(),
            retryable: true,
        }
    }
}

/// A lease the gate granted on a model: requests under it carry [`LEASE_HEADER`]. It is given back by
/// [`Lease::release`], or in the background when it is dropped (a client that left, a failed step).
pub struct Lease {
    gate: Arc<Gate>,
    token: String,
    released: bool,
}

impl Lease {
    pub fn header(&self) -> (HeaderName, HeaderValue) {
        (
            HeaderName::from_static(LEASE_HEADER),
            HeaderValue::from_str(&self.token).unwrap_or_else(|_| HeaderValue::from_static("")),
        )
    }

    /// Gives the lease back (best effort: the gate ends an idle lease on its own).
    pub async fn release(mut self) {
        self.released = true;
        self.gate.release_lease(&self.token).await;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (gate, token) = (Arc::clone(&self.gate), self.token.clone());
            rt.spawn(async move { gate.release_lease(&token).await });
        }
    }
}

impl Gate {
    /// `acquire(model)`: waits in the gate's queue for a lease on `model`. `Ok(None)`: no gate answers at the engine
    /// address (404 without a refusal), so the work runs without a lease. Dropping this future leaves the queue (the
    /// stream is closed), which is how a client that goes away starts no switch.
    pub async fn lease(self: &Arc<Self>, model: &str) -> Result<Option<Lease>, LeaseRefusal> {
        let mut res = self
            .client
            .post(format!("{}/gate/v1/leases", self.base))
            .header(CONTENT_TYPE, "application/json")
            .header(CLIENT_HEADER, "front")
            .timeout(LEASE_WAIT)
            .body(serde_json::json!({ "model": model, "holder": "front" }).to_string())
            .send()
            .await
            .map_err(|_| LeaseRefusal::unavailable())?;
        let status = res.status().as_u16();
        let stream = res.status().is_success()
            && res
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| t.starts_with("text/event-stream"));
        if !stream {
            let mut bytes: Vec<u8> = Vec::new();
            while let Ok(Some(chunk)) = res.chunk().await {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_EVENT_BYTES {
                    break;
                }
            }
            let body: Option<Value> = serde_json::from_slice(&bytes).ok();
            if let Some(e) = body
                .as_ref()
                .and_then(|b| b.get("error"))
                .filter(|e| e.get("type").and_then(Value::as_str) == Some("gate_error"))
            {
                if let Some(code) = e.get("code").and_then(Value::as_str) {
                    return Err(LeaseRefusal {
                        status,
                        code: code.to_owned(),
                        message: e
                            .get("message")
                            .and_then(Value::as_str)
                            .filter(|m| !m.is_empty())
                            .map(|m| clip(m, 300))
                            .unwrap_or_else(|| "The inference gate refused the request.".into()),
                        retryable: e.get("retryable") == Some(&Value::Bool(true)),
                    });
                }
            }
            return if status == 404 {
                Ok(None)
            } else {
                Err(LeaseRefusal::unavailable())
            };
        }
        let mut buffer = String::new();
        loop {
            let chunk = match res.chunk().await {
                Ok(Some(c)) => c,
                _ => return Err(LeaseRefusal::unavailable()),
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                let (mut event, mut data) = ("message", String::new());
                for line in block.split('\n') {
                    if let Some(e) = line.strip_prefix("event:") {
                        event = e.trim();
                    } else if let Some(d) = line.strip_prefix("data:") {
                        data.push_str(d.trim());
                    }
                }
                let data: Option<Value> = serde_json::from_str(&data).ok();
                match event {
                    "granted" => {
                        let token = data
                            .as_ref()
                            .and_then(|d| d.get("lease"))
                            .and_then(Value::as_str)
                            .filter(|t| is_token(t))
                            .ok_or_else(LeaseRefusal::unavailable)?;
                        return Ok(Some(Lease {
                            gate: Arc::clone(self),
                            token: token.to_owned(),
                            released: false,
                        }));
                    }
                    "error" => {
                        let d = data.as_ref();
                        return Err(LeaseRefusal {
                            status: 503,
                            code: d
                                .and_then(|d| d.get("code"))
                                .and_then(Value::as_str)
                                .unwrap_or("gate_error")
                                .to_owned(),
                            message: d
                                .and_then(|d| d.get("message"))
                                .and_then(Value::as_str)
                                .filter(|m| !m.is_empty())
                                .map(|m| clip(m, 300))
                                .unwrap_or_else(|| {
                                    "The inference gate refused the request.".into()
                                }),
                            retryable: d.and_then(|d| d.get("retryable"))
                                == Some(&Value::Bool(true)),
                        });
                    }
                    _ => {}
                }
            }
            if buffer.len() > MAX_EVENT_BYTES {
                return Err(LeaseRefusal::unavailable());
            }
        }
    }

    async fn release_lease(&self, token: &str) {
        let _ = self
            .client
            .delete(format!("{}/gate/v1/leases/{token}", self.base))
            .timeout(CALL_TIMEOUT)
            .send()
            .await;
    }
}

/// Added to a step's limit for what it does around its router call (reading models.ini).
const STEP_MARGIN: Duration = Duration::from_secs(15);

const RELEASE_LIMIT: Duration = Duration::from_secs(30);
const EXPIRY_MARGIN: Duration = Duration::from_millis(100);

/// sbstndalton/noevia#2641: how a lease on Node's gate is covered. With `node`
/// (`NOEVIA_RUST_LEASE_COVERAGE=node`) a lease follows Node's own lifecycle for one
/// (inference-maintenance.cjs `nativeHolding`/`lose`, `assertCovered`, `dispatchSignal`): a renewal
/// lapsing within `margin`, the hard limit within it, or an ended lease is the loss, which ends the
/// calls in flight ([`Hold::lost_wait`]) and gives the lease back owing the recovery; a step starts
/// while it is covered by renewal ([`Hold::assert_covered`]), not only when one confirmation outlasts
/// it. Off (the default) every hold behaves exactly as before.
#[derive(Debug, Clone)]
pub struct Coverage {
    pub node: bool,
    /// Node's `coverageMarginMs`.
    pub margin: Duration,
    /// Where a lost hold without a repair worker of its own owes its settle (Node's `recoveries`).
    pub recoveries: Arc<Registry>,
}

impl Default for Coverage {
    fn default() -> Self {
        Coverage {
            node: false,
            margin: STEP_MARGIN,
            recoveries: Arc::new(Registry::default()),
        }
    }
}

/// Node's reasons a lease is lost or does not cover a step (`lose(why)`, `coverageError(why)`).
pub(crate) const PASSED: &str = "passed its limit";
pub(crate) const NOT_RENEWED: &str = "could not be renewed";
pub(crate) const ENDED: &str = "was ended by the lease authority";
const CANNOT_COVER: &str = "cannot cover the next step";

/// Node's `coverageError(why)` text (503, `maintenanceCoverageLost`).
pub fn coverage_message(why: &str) -> String {
    format!("The maintenance lease {why}, so this work stopped before changing anything else.")
}

/// The line Node's `lose(why)` logs.
fn loss_line(why: &str) -> String {
    format!("[models] this process's maintenance lease {why}; its work stops before its next step, and chat stays paused until it ends")
}

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
    /// #2641 ([`Coverage`]): Node's lifecycle for this lease, its margin, why it was lost, the
    /// in-flight calls' wake, and where it owes its settle when let go after a loss.
    node: bool,
    margin: Duration,
    why: Mutex<Option<&'static str>>,
    lost_wake: tokio::sync::Notify,
    settle: Option<Arc<Registry>>,
    /// The release found the recovery required (decided under the release lock): letting the hold
    /// go owes the settle even without a loss of its own (a fenced or already-owed lease).
    owes_settle: AtomicBool,
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
            node: false,
            margin: STEP_MARGIN,
            why: Mutex::new(None),
            lost_wake: tokio::sync::Notify::new(),
            settle: None,
            owes_settle: AtomicBool::new(false),
        })
    }

    /// Whether the authority still holds this lease, covering `step` from now (no authority: true).
    fn fenced(&self, step: Duration) -> bool {
        self.auth
            .as_ref()
            .is_none_or(|(a, g)| a.covers(&g.id, g.generation, step))
    }

    /// inference-maintenance.cjs `assertCovered(step)` (Delta review P2): not lost, the last
    /// confirmation (and the authority's lease) not lapsing within the margin, and the step plus the
    /// margin inside the hard limit. A long step is covered by renewal, not by its whole length
    /// fitting one confirmation. A lapse within the margin or a passed hard limit is the loss itself
    /// (Node's `c.lose()`): sticky, so the release owes the settle; a step that merely does not fit
    /// is refused without it (`lose`: whether this check may mark the loss). Always true for the
    /// gate's hold.
    fn covers(&self, step: Duration, lose: bool) -> bool {
        let Some(ttl) = self.lease_ttl else {
            return true;
        };
        if self.lost.load(Ordering::Acquire) {
            return false;
        }
        let now = Instant::now();
        let alive =
            self.auth.as_ref().is_none_or(|(a, g)| {
                a.covers_step(&g.id, g.generation, STEP_MARGIN, Duration::ZERO)
            }) && self
                .confirmed
                .lock()
                .is_ok_and(|at| now + STEP_MARGIN < *at + ttl)
                && self.lease_end.is_none_or(|end| now + STEP_MARGIN < end);
        if !alive {
            if lose {
                self.lose_at_margin();
            }
            return false;
        }
        self.auth
            .as_ref()
            .is_none_or(|(a, g)| a.covers_step(&g.id, g.generation, STEP_MARGIN, step))
            && self
                .lease_end
                .is_none_or(|end| now + step + STEP_MARGIN < end)
    }

    /// Node's `lose()` from a coverage check: only while the hold is not being given back.
    fn lose_at_margin(&self) {
        let state = self.release.lock().unwrap_or_else(|p| p.into_inner());
        if *state == ReleaseState::Idle && !self.lost.swap(true, Ordering::AcqRel) {
            eprintln!("[models] this front's maintenance lease could not be renewed in time; the work stops before its next step");
        }
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

/// #2641: Node's lifecycle of a native lease (inference-maintenance.cjs `nativeHolding`), for a
/// hold with [`Coverage::node`]. Every decision takes `now`, so the recordings of Node replay on it.
impl Shared {
    fn lost_why(&self) -> Option<&'static str> {
        *self.why.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn confirmed_at(&self) -> Instant {
        *self.confirmed.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Node's `lose(why)`: once, and only while the hold is neither let go nor being given back.
    /// The calls in flight end ([`Self::lost_wait`]) and the renewer gives the lease back, owing the
    /// recovery. The line it logged, if it lost the hold.
    fn lose(&self, why: &'static str) -> Option<String> {
        let state = self.release.lock().unwrap_or_else(|p| p.into_inner());
        if *state != ReleaseState::Idle || self.released.load(Ordering::Acquire) {
            return None;
        }
        let mut slot = self.why.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return None;
        }
        *slot = Some(why);
        self.lost.store(true, Ordering::Release);
        drop(slot);
        drop(state);
        let line = loss_line(why);
        eprintln!("{line}");
        self.lost_wake.notify_waiters();
        self.wake.notify_one();
        Some(line)
    }

    /// Node's `assertCovered(step)` at `now`: `Err(why)`. A lapse within the margin (Node's last
    /// confirmation or the authority's lease), the hard limit within it and an ended authority lease
    /// are the loss itself; a step that merely does not fit before the hard limit is refused without
    /// it. Always covered for the gate's hold.
    fn assert_covered_at(&self, now: Instant, step: Duration) -> Result<(), &'static str> {
        let Some(ttl) = self.lease_ttl else {
            return Ok(());
        };
        if self.lost.load(Ordering::Acquire) {
            return Err(self.lost_why().unwrap_or(ENDED));
        }
        let m = self.margin;
        let lost = |why: &'static str| {
            self.lose(why);
            Err(why)
        };
        if now + m >= self.confirmed_at() + ttl {
            return lost(NOT_RENEWED);
        }
        if self.lease_end.is_some_and(|end| now + m >= end) {
            return lost(PASSED);
        }
        if let Some((a, g)) = self.auth.as_ref() {
            if !a.covers(&g.id, g.generation, Duration::ZERO) {
                return lost(ENDED);
            }
            if !a.covers_step(&g.id, g.generation, m, Duration::ZERO) {
                return lost(NOT_RENEWED);
            }
            if !a.covers_step(&g.id, g.generation, m, step) {
                return Err(CANNOT_COVER);
            }
        }
        if self.lease_end.is_some_and(|end| now + step + m >= end) {
            return Err(CANNOT_COVER);
        }
        Ok(())
    }

    /// Node's renewal tick before it renews: false when the hold has ended, or when its hard limit
    /// is within the margin (`lose('passed its limit')`).
    fn tick_at(&self, now: Instant) -> bool {
        if self.lost.load(Ordering::Acquire) || self.released.load(Ordering::Acquire) {
            return false;
        }
        if self.lease_end.is_some_and(|end| now + self.margin >= end) {
            self.lose(PASSED);
            return false;
        }
        true
    }

    /// The renewal's answer, asked at `asked` and come at `now` (true / false: ended / none): false
    /// when the hold has ended, Node answered that it ended, or the last confirmation lapses within
    /// the margin. A late answer after the loss changes nothing.
    fn renewed_at(&self, asked: Instant, now: Instant, answer: Option<bool>) -> bool {
        if self.lost.load(Ordering::Acquire) || self.released.load(Ordering::Acquire) {
            return false;
        }
        let Some(ttl) = self.lease_ttl else {
            return true;
        };
        match answer {
            Some(true) => {
                *self.confirmed.lock().unwrap_or_else(|p| p.into_inner()) = asked;
                true
            }
            Some(false) => {
                self.lose(ENDED);
                false
            }
            None if now + self.margin >= self.confirmed_at() + ttl => {
                self.lose(NOT_RENEWED);
                false
            }
            None => true,
        }
    }

    /// Node's `release()`: the hold is let go (under the same lock as a loss, so a loss is either
    /// seen here or never happens), and a lost one, or one whose release found the recovery
    /// required, owes `settle`.
    fn let_go(&self) {
        let state = self.release.lock().unwrap_or_else(|p| p.into_inner());
        self.released.store(true, Ordering::Release);
        let owes = self.lost.load(Ordering::Acquire) || self.owes_settle.load(Ordering::Acquire);
        drop(state);
        self.wake.notify_one();
        if let (true, Some(r)) = (owes, self.settle.as_ref()) {
            r.require(SETTLE, None);
        }
    }

    /// The release's `if(recoveries.size)scheduleRecovery(0)`, once this process's admission is back.
    fn kick_owed(&self) {
        if let Some(r) = self.settle.as_ref().filter(|r| r.pending()) {
            r.kick();
        }
    }

    /// Resolves once the hold is lost: the loss itself, or a coverage check at the point its last
    /// confirmation (or hard limit) comes within the margin, which is the loss (Node's
    /// `dispatchSignal`). Checked at least every second, for the authority's own end.
    async fn lost_wait(&self) {
        loop {
            let lost = self.lost_wake.notified();
            tokio::pin!(lost);
            lost.as_mut().enable();
            if self.lost.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            let mut at = now + Duration::from_secs(1);
            if let Some(ttl) = self.lease_ttl {
                let lapse = self.confirmed_at() + ttl;
                at = at.min(lapse.checked_sub(self.margin).unwrap_or(now));
            }
            if let Some(end) = self.lease_end {
                at = at.min(end.checked_sub(self.margin).unwrap_or(now));
            }
            tokio::select! {
                () = lost => {}
                () = tokio::time::sleep_until(at.into()) => {
                    if Instant::now() < at {
                        // The tokio clock ran ahead of the std clock this hold keeps its times on
                        // (a paused test clock): no coverage check can fall due, only the loss
                        // itself ends the wait (no spin). Production runs on the real clock.
                        lost_signal(self).await;
                        return;
                    }
                    if self.assert_covered_at(Instant::now(), Duration::ZERO).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

impl Shared {
    // Serialize renewal/loss decisions with the transition to intentional release.
    // None means release has started; it must not turn a completed run into a loss.
    fn renew_authority(&self) -> Option<bool> {
        let state = self.release.lock().unwrap_or_else(|p| p.into_inner());
        if *state != ReleaseState::Idle {
            return None;
        }
        let overdue = Instant::now() >= self.deadline();
        let live = !overdue
            && self
                .auth
                .as_ref()
                .is_none_or(|(a, g)| a.renew(&g.id, Some(g.generation)));
        if !live && self.node {
            drop(state);
            self.lose(if overdue { PASSED } else { ENDED });
            return Some(false);
        }
        if !live {
            self.lost.store(true, Ordering::Release);
            eprintln!("[models] the maintenance lease authority ended this front's lease; the work stops before its next step");
        }
        Some(live)
    }

    fn node_renewal_refused(&self) {
        if self.node {
            self.lose(ENDED);
            return;
        }
        let state = self.release.lock().unwrap_or_else(|p| p.into_inner());
        if *state == ReleaseState::Idle {
            self.lost.store(true, Ordering::Release);
            eprintln!("[models] Node ended this front's maintenance lease; the work stops before its next step");
        }
    }

    fn start_release(self: &Arc<Self>, node: Arc<NodeLeases>) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let needs = || {
            self.lost.load(Ordering::Acquire)
                || !self.fenced(Duration::ZERO)
                || self
                    .auth
                    .as_ref()
                    .is_some_and(|(a, _)| a.recovery_pending())
        };
        let require = || {
            self.auth.as_ref().map_or_else(
                || self.lease_id.clone().unwrap_or_default(),
                |(a, _)| a.require_recovery(),
            )
        };
        let mut decided = None;
        {
            let mut state = match self.release.lock() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            if *state != ReleaseState::Idle {
                return;
            }
            *state = ReleaseState::Pending;
            // #2641: the recovery is required before anyone can see the hold let go (`let_go` takes
            // this lock), so the settle driver never finds a lost hold's recovery not yet owed.
            if self.node {
                let needs_recovery = needs();
                if needs_recovery {
                    self.owes_settle.store(true, Ordering::Release);
                }
                decided = Some((needs_recovery, needs_recovery.then(require)));
            }
        }
        let (needs_recovery, recovery_ticket) = decided.unwrap_or_else(|| {
            let needs_recovery = needs();
            (needs_recovery, needs_recovery.then(require))
        });
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
pub(crate) struct AdmissionFence {
    local: Arc<AtomicBool>,
}
impl AdmissionFence {
    pub(super) fn reserve(local: Arc<AtomicBool>) -> Option<Self> {
        local
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Self { local })
    }
    pub(crate) fn release(self) {
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
    pub(crate) fn recovery_proof(&self) -> Option<(&str, u64)> {
        self.shared
            .lease_id
            .as_deref()
            .map(|id| (id, self.shared.node_generation))
    }

    pub(crate) fn defer_local_release(&mut self) -> AdmissionFence {
        self.local_finished = true;
        AdmissionFence {
            local: Arc::clone(&self.local),
        }
    }

    /// A lease on Node's gate, renewed until `deadline`. `coverage`: Node's lifecycle when it is on;
    /// then every hold (a recovery's too: Node's release re-adds the settle) owes the settle when it
    /// is let go after a loss.
    pub(super) fn leased(
        local: Arc<AtomicBool>,
        node: Arc<NodeLeases>,
        grant: LeaseGrant,
        auth: Option<(lease::Authority, lease::Grant)>,
        reason: &str,
        deadline: Instant,
        coverage: Option<&Coverage>,
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
            if let Some(c) = coverage.filter(|c| c.node) {
                inner.node = true;
                inner.margin = c.margin;
                inner.settle = Some(Arc::clone(&c.recoveries));
            }
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

    /// Node's `assertCovered(step)` ([`Shared::covers`]): the rule for a step whose calls end when
    /// the hold stops covering even a zero-length step (`!covers(ZERO)`: Node's `lose()` at the
    /// margin), as the native auto-tune runner's calls do. A call that cannot be ended that way
    /// must use the stricter [`Hold::may_start`] (sbstndalton/noevia#2641).
    pub fn covers(&self, step: Duration) -> bool {
        if self.shared.node {
            return self.assert_covered(step).is_ok();
        }
        !self.shared.released.load(Ordering::Acquire) && self.shared.covers(step, true)
    }

    /// #2641, Node's `assertCovered(step)` ([`Coverage::node`]): `Err` with Node's coverage error
    /// text. A lapse within the margin is the loss (the calls in flight end, the release owes the
    /// recovery). Without Node's lifecycle: [`Self::covers`], with the same text on a refusal.
    pub fn assert_covered(&self, step: Duration) -> Result<(), String> {
        if !self.shared.node {
            return if self.covers(step) {
                Ok(())
            } else {
                Err(coverage_message(
                    self.shared.lost_why().unwrap_or(NOT_RENEWED),
                ))
            };
        }
        if self.shared.released.load(Ordering::Acquire) {
            return Err(coverage_message(self.shared.lost_why().unwrap_or(ENDED)));
        }
        self.shared
            .assert_covered_at(Instant::now(), step)
            .map_err(coverage_message)
    }

    /// Whether the hold follows Node's lifecycle ([`Coverage::node`]).
    pub fn node_coverage(&self) -> bool {
        self.shared.node
    }

    /// #2641, Node's `dispatchSignal`: resolves once the hold is lost (with [`Coverage::node`]; never
    /// otherwise), so a call in flight ends with [`Self::coverage_error`].
    pub async fn lost_wait(&self) {
        if self.shared.node {
            self.shared.lost_wait().await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    /// Node's coverage error for a lost hold (`The maintenance lease <why>, so ...`).
    pub fn coverage_error(&self) -> String {
        coverage_message(self.shared.lost_why().unwrap_or(NOT_RENEWED))
    }

    /// Node's rule without marking a loss: for a check whose caller cannot drive the settle a
    /// lost hold owes (the public calibration worker, until sbstndalton/noevia#2641).
    pub fn fits(&self, step: Duration) -> bool {
        // With Node's lifecycle a lost hold's settle is driven, so the check is Node's own.
        if self.shared.node {
            return self.covers(step);
        }
        !self.shared.released.load(Ordering::Acquire) && self.shared.covers(step, false)
    }

    /// Whether the hold still excludes everyone else: not lost, and for Node's lease, Node's last
    /// confirmation still covers now (an unconfirmed lease may already have ended at Node).
    pub fn excluded(&self) -> bool {
        if self.shared.lost.load(Ordering::Acquire) || self.shared.released.load(Ordering::Acquire)
        {
            return false;
        }
        if self.shared.node {
            return self.covers(Duration::ZERO);
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
    pub(crate) fn write_fence(&self) -> super::ini::WriteAllowed {
        let shared = Arc::clone(&self.shared);
        Arc::new(move || {
            if shared.node {
                return !shared.released.load(Ordering::Acquire)
                    && shared
                        .assert_covered_at(Instant::now(), Duration::ZERO)
                        .is_ok();
            }
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
        if self.shared.node {
            // #2641: the recovery is required first, then the hold is let go, then the driver kicked.
            if let Some(node) = self.node.as_ref() {
                self.shared.start_release(Arc::clone(node));
            }
            self.finish();
            self.release_node_lease().await;
            return;
        }
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
        if self.shared.node {
            self.shared.let_go();
        } else {
            self.shared.released.store(true, Ordering::Release);
            self.shared.wake.notify_one();
        }
        if !self.local_finished {
            self.local.store(false, Ordering::Release);
            self.local_finished = true;
        }
        if self.shared.node {
            self.shared.kick_owed();
        }
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if self.shared.node {
            if let Some(node) = self.node.take() {
                self.shared.start_release(node);
            }
            self.finish();
            return;
        }
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
    if shared.node {
        return node_lease_loop(shared, node, every).await;
    }
    loop {
        if !shared.pause(every).await {
            return;
        }
        match shared.renew_authority() {
            None => return,
            Some(false) => break,
            Some(true) => {}
        }
        let Some(id) = shared.token.lock().ok().and_then(|t| t.clone()) else {
            return;
        };
        let asked = Instant::now();
        match node.renew(&id).await {
            Some(true) => {
                if let Ok(mut at) = shared.confirmed.lock() {
                    *at = asked;
                }
            }
            Some(false) => {
                shared.node_renewal_refused();
                return;
            }
            // Not answered: `Hold::may_start` refuses once the last confirmation cannot cover a step.
            None => {}
        }
    }
    shared.start_release(node);
}

/// #2641: Node's renewal tick for a native lease (`nativeHolding` `tick`). A loss (here, or from a
/// coverage check meanwhile) gives the lease back at once, owing the recovery (Node's `lose()` releases
/// its token), while the hold keeps this process's own admission until the work lets it go.
async fn node_lease_loop(shared: Arc<Shared>, node: Arc<NodeLeases>, every: Duration) {
    loop {
        if !shared.pause(every).await {
            return;
        }
        if shared.lost.load(Ordering::Acquire) || !shared.tick_at(Instant::now()) {
            break;
        }
        match shared.renew_authority() {
            None => return,
            Some(false) => break,
            Some(true) => {}
        }
        let Some(id) = shared.token.lock().ok().and_then(|t| t.clone()) else {
            return;
        };
        let asked = Instant::now();
        let answer = tokio::select! {
            answer = node.renew(&id) => answer,
            () = lost_signal(&shared) => break,
        };
        if !shared.renewed_at(asked, Instant::now(), answer) {
            break;
        }
    }
    if shared.released.load(Ordering::Acquire) && !shared.lost.load(Ordering::Acquire) {
        // Let go meanwhile: its own release gives the lease back.
        return;
    }
    shared.start_release(node);
}

/// Resolves once the hold is marked lost (no coverage check of its own).
async fn lost_signal(shared: &Shared) {
    loop {
        let lost = shared.lost_wake.notified();
        tokio::pin!(lost);
        lost.as_mut().enable();
        if shared.lost.load(Ordering::Acquire) {
            return;
        }
        lost.await;
    }
}

#[cfg(test)]
#[path = "gate_replay.rs"]
mod replay;

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
            None,
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
    #[tokio::test]
    async fn intentional_release_cannot_be_reclassified_by_a_late_renewal() {
        let (mut hold, seen, proceed, ack) =
            review_release_hold(Duration::from_secs(5), Duration::from_secs(2)).await;
        let shared = Arc::clone(&hold.shared);
        let join = tokio::spawn(async move {
            hold.release_node_lease().await;
            hold
        });
        tokio::time::timeout(Duration::from_secs(1), seen.notified())
            .await
            .unwrap();
        // DELETE is pending: neither an authority renewal nor an in-flight Node
        // renewal's 404 may classify this intentional release as a lost lease.
        assert_eq!(shared.renew_authority(), None);
        shared.node_renewal_refused();
        assert!(!shared.lost.load(Ordering::Acquire));
        assert!(!ack.load(Ordering::Acquire));
        assert!(*shared.release.lock().unwrap() == ReleaseState::Pending);
        proceed.notify_one();
        let hold = tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .unwrap()
            .unwrap();
        assert!(hold.node_release_confirmed());
        assert!(!hold.lost());
    }

    #[test]
    fn an_active_lease_still_records_authority_and_node_loss() {
        let authority = lease::Authority::default();
        let grant = authority
            .try_acquire(lease::Origin::Front, None, "synthetic")
            .unwrap();
        let mut shared = Shared::new(
            None,
            Instant::now() + Duration::from_secs(5),
            None,
            None,
            None,
        );
        Arc::get_mut(&mut shared).unwrap().auth = Some((authority.clone(), grant.clone()));
        assert_eq!(shared.renew_authority(), Some(true));
        authority.release(&grant.id, Some(grant.generation));
        assert_eq!(shared.renew_authority(), Some(false));
        assert!(shared.lost.load(Ordering::Acquire));
        let shared = Shared::new(
            None,
            Instant::now() + Duration::from_secs(5),
            None,
            None,
            None,
        );
        shared.node_renewal_refused();
        assert!(shared.lost.load(Ordering::Acquire));
    }

    /// #2550: Node's `assertCovered(step)` rule. On a Node lease (TTL 180 s, renewed every 10 s)
    /// the native runner's 600 s load and 180 s smoke steps start, as in Node (the whole-step rule
    /// of `may_start` never could); a step past the hard cap, a lapse within the 15 s margin and a
    /// lost hold do not. The gate's own hold always covers.
    #[test]
    fn covers_is_nodes_assert_covered_rule() {
        let s = |end: u64| {
            let now = Instant::now();
            Shared::new(
                Some("qa-lease".into()),
                now + Duration::from_secs(3600),
                Some(Duration::from_secs(180)),
                Some(now + Duration::from_secs(end)),
                None,
            )
        };
        let hour = s(3600);
        assert!(hour.covers(Duration::from_secs(600), true));
        assert!(hour.covers(Duration::from_secs(180), true));
        let short = s(160);
        assert!(short.covers(Duration::from_secs(140), true));
        assert!(
            !short.covers(Duration::from_secs(150), true),
            "the step past the hard cap"
        );
        assert!(
            !short.lost.load(Ordering::Acquire),
            "a step that does not fit is no loss"
        );
        let lapsing = s(3600);
        if let (Ok(mut at), Some(old)) = (
            lapsing.confirmed.lock(),
            Instant::now().checked_sub(Duration::from_secs(170)),
        ) {
            *at = old;
        }
        assert!(
            !lapsing.covers(Duration::ZERO, true),
            "a lapse within the margin"
        );
        assert!(
            lapsing.lost.load(Ordering::Acquire),
            "the lapse is the loss itself (sticky)"
        );
        let lost = s(3600);
        lost.lost.store(true, Ordering::Release);
        assert!(!lost.covers(Duration::ZERO, true));
        let ending = s(10);
        assert!(
            !ending.covers(Duration::ZERO, false),
            "the hard cap within the margin"
        );
        assert!(
            !ending.lost.load(Ordering::Acquire),
            "a check that may not mark it"
        );
        assert!(!ending.covers(Duration::ZERO, true));
        assert!(
            ending.lost.load(Ordering::Acquire),
            "the hard cap within the margin is a loss"
        );
        let releasing = s(10);
        if let Ok(mut state) = releasing.release.lock() {
            *state = ReleaseState::Pending;
        }
        assert!(!releasing.covers(Duration::ZERO, true));
        assert!(
            !releasing.lost.load(Ordering::Acquire),
            "no loss once the hold is being given back"
        );
        let gate = Shared::new(None, Instant::now(), None, None, None);
        assert!(gate.covers(Duration::from_secs(86_400), true));
    }

    // The same test-only overlay compiles against the published pre-fix source.
    // Drive the actual renewal HTTP response while DELETE is held pending.
    async fn semantic_node_refusal_during_release(releasing: bool) {
        use axum::{
            body::Body,
            http::{Request, Response},
        };
        let renew_seen = Arc::new(tokio::sync::Notify::new());
        let renew_go = Arc::new(tokio::sync::Notify::new());
        let delete_seen = Arc::new(tokio::sync::Notify::new());
        let delete_go = Arc::new(tokio::sync::Notify::new());
        let (a, b, c, d) = (
            renew_seen.clone(),
            renew_go.clone(),
            delete_seen.clone(),
            delete_go.clone(),
        );
        let app = axum::Router::new().fallback(move |r: Request<Body>| {
            let (a, b, c, d) = (a.clone(), b.clone(), c.clone(), d.clone());
            async move {
                let code = if r.uri().path().ends_with("/renew") {
                    a.notify_one();
                    b.notified().await;
                    404
                } else if r.uri().path().contains("/recovery/") {
                    204
                } else {
                    c.notify_one();
                    d.notified().await;
                    204
                };
                Response::builder()
                    .status(code)
                    .body(Body::empty())
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node = Arc::new(
            NodeLeases::new(
                Some(&format!("http://{}", listener.local_addr().unwrap())),
                Some("synthetic"),
            )
            .unwrap(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let shared = Shared::new(
            Some("fedcba9876543210fedcba9876543210".into()),
            Instant::now() + Duration::from_secs(10),
            Some(Duration::from_secs(10)),
            Some(Instant::now() + Duration::from_secs(10)),
            Some(Instant::now() + Duration::from_secs(10)),
        );
        let task = tokio::spawn(lease_loop(
            shared.clone(),
            node.clone(),
            Duration::from_millis(1),
        ));
        tokio::time::timeout(Duration::from_secs(2), renew_seen.notified())
            .await
            .unwrap();
        let mut hold = Hold {
            shared: shared.clone(),
            gate: None,
            node: Some(node),
            local: Arc::new(AtomicBool::new(true)),
            reason: "synthetic".into(),
            local_finished: false,
        };
        if releasing {
            shared.start_release(hold.node.as_ref().unwrap().clone());
            tokio::time::timeout(Duration::from_secs(2), delete_seen.notified())
                .await
                .unwrap();
        }
        renew_go.notify_one();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        let lost = hold.lost();
        let pending = *shared.release.lock().unwrap() == ReleaseState::Pending;
        let confirmed_early = hold.node_release_confirmed();
        delete_go.notify_one();
        tokio::time::timeout(Duration::from_secs(2), hold.release_node_lease())
            .await
            .unwrap();
        let confirmed = hold.node_release_confirmed();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        if releasing {
            assert!(pending, "the real DELETE must still be pending at refusal");
            assert!(
                !confirmed_early,
                "a pending DELETE is not release confirmation"
            );
            assert!(
                !lost,
                "a late Node refusal must not reclassify intentional release"
            );
        } else {
            assert!(
                lost,
                "a still-active lease must detect a genuine Node refusal"
            );
        }
        assert!(
            confirmed,
            "completion requires the actual DELETE acknowledgment"
        );
    }

    #[tokio::test]
    async fn semantic_late_node_refusal_during_actual_pending_delete() {
        semantic_node_refusal_during_release(true).await;
    }

    #[tokio::test]
    async fn semantic_active_node_refusal_is_still_loss() {
        semantic_node_refusal_during_release(false).await;
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
