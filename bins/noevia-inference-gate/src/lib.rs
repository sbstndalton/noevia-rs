//! sbstndalton/noevia#2115, #2117: the inference gate.
//!
//! The gate is the only published entry to the chat engine router. Web, Diary and every other
//! client of the router reach it through the gate, which forwards each request unchanged but first
//! asks the admission scheduler (`inference-admission`) whether a request that may load or run on a
//! model can go now, must wait, or is refused. It counts every forwarded generation until its
//! response body ends, so it knows exactly what is in flight; it unloads only an idle model, and
//! only while another model waits; and it is the only process that asks the router to unload.
//!
//! Clients that make several requests for one chat turn take a lease first
//! (`POST /gate/v1/leases`, an event stream that reports the queue position until the lease is
//! granted) and send it in `X-Noevia-Lease` on every request of the turn; the lease keeps their
//! model from being switched away between requests. Clients without a lease are admitted per
//! request with a short residency grace.
//!
//! Gate endpoints (everything else is the router's API):
//! - `GET /gate/v1/health`, `GET /gate/v1/status` (model ids, counts and times; never request
//!   content), `GET /gate/v1/metrics` (text exposition format);
//! - `POST /gate/v1/leases` `{model, holder?, max_wait_ms?}` and `DELETE /gate/v1/leases/{lease}`;
//! - the maintenance hold (sbstndalton/noevia#2187), admin only: `POST /gate/v1/maintenance`
//!   `{reason?, wait_ms?, idle_ttl_ms?}`, `POST /gate/v1/maintenance/{hold}/renew` `{reason?}` and
//!   `DELETE /gate/v1/maintenance/{hold}` (see leases.rs).
//!
//! The maintenance hold is the one exclusion a preset write, a calibration, an auto-tune or a
//! throughput sweep takes: the gate is its only authority, so every client of the router (web,
//! Diary, the Rust front) respects the same hold. It is granted only while nothing runs (no
//! request in flight, no lease, nobody queued, no switch or reload); while it is held, a request
//! or lease without it waits for its end (bounded by its wait limit, then `maintenance_held`), and
//! unloads (but an admin force) and preset reloads without it are refused `engine_busy`; exempt
//! work and `autoload=false` reads, which never take the slot, run as before. The holder's own
//! requests carry it in `X-Noevia-Maintenance` and are scheduled as if there were no hold. A hold
//! ends when it is released, or once its holder has neither renewed nor used it for its idle limit
//! (60 s by default), so a holder that died pauses the engine for at most that long.
//!
//! Names are resolved as the router resolves them (an alias is its model). A request that asks the
//! router not to load a model that is not resident cannot use the slot and is passed on. A
//! session generation (`x-conversation-id`) whose client leaves may go on in the router for a
//! later resume, so its model is held for `GATE_SESSION_HOLD_SECONDS`, and a resume
//! (`GET /v1/stream`) counts as work on the session's model. The hold ends early when the session
//! is stopped (`DELETE /v1/stream`) or its generation ends (a resume reaches its end, or the
//! router's session lookup reports it done or gone), and is renewed while the router reports the
//! generation still running. A preset reload holds every new
//! admission until the router has answered it. Unloads and reloads run to their end on their own
//! task, whatever happens to the client that asked.
//!
//! Request headers the gate reads and removes: `X-Noevia-Lease`, `X-Noevia-Maintenance`, `X-Noevia-Max-Wait-Ms` (a shorter
//! wait than the configured maximum), `X-Noevia-Client` (a short label for logs) and
//! `X-Noevia-Gate-Admin` (the admin token: needed to unload a model that is in use, which is
//! logged).

pub mod env;
mod forward;
mod leases;
mod respond;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use axum::Router;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use inference_admission::{
    Action, Arrival, Config, Grant, HoldKey, LeaseKey, Refusal, Scheduler, Ticket, UnloadOutcome,
    WaitKind, WaiterKey,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Notify, Semaphore};

/// The gate's configuration.
#[derive(Clone)]
pub struct GateConfig {
    pub listen: SocketAddr,
    /// The router's base URL, `http://host:port` without a trailing slash.
    pub upstream: String,
    /// Needed to unload a model in use or force a preset reload while busy. `None`: never allowed.
    pub admin_token: Option<String>,
    pub admission: Config,
    /// Largest request body read (bytes).
    pub max_body_bytes: usize,
    /// Most request-body bytes buffered at once across all requests.
    pub max_buffered_bytes: usize,
    /// Longest a client may take to send its body.
    pub body_timeout: Duration,
    /// Longest the router may take to answer a forwarded request's headers (it loads models
    /// before answering).
    pub header_timeout: Duration,
    /// How often the router's listing is read while nothing changes.
    pub listing_idle_interval: Duration,
    /// How often it is read while a switch runs or work waits.
    pub listing_busy_interval: Duration,
    /// How often the scheduler's clock advances on its own.
    pub tick: Duration,
    /// How often a lease request's event stream reports its position.
    pub lease_event_interval: Duration,
    /// Longest a client may take to send a request's headers.
    pub header_read_timeout: Duration,
    /// Most client connections served at once; more are closed at once.
    pub max_connections: usize,
    /// Longest the router may take to answer a preset reload (no new work is admitted meanwhile).
    pub reload_timeout: Duration,
    /// The period of the timer that asks the router whether held detached session generations
    /// still run (the timer always ticks; a tick does nothing while no hold exists).
    pub session_check_interval: Duration,
    /// A maintenance hold ends this long after its holder last renewed or used it, unless the
    /// holder asked for another limit (1 s to 10 min).
    pub maintenance_idle: Duration,
}

impl std::fmt::Debug for GateConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateConfig")
            .field("listen", &self.listen)
            .field("upstream", &self.upstream)
            .field("admin_token", &self.admin_token.as_ref().map(|_| "<set>"))
            .field("admission", &self.admission)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_buffered_bytes", &self.max_buffered_bytes)
            .finish_non_exhaustive()
    }
}

impl GateConfig {
    /// Production defaults around a listen address and an upstream.
    pub fn defaults(listen: SocketAddr, upstream: String) -> Self {
        Self {
            listen,
            upstream,
            admin_token: None,
            admission: Config::default(),
            max_body_bytes: 64 * 1024 * 1024,
            max_buffered_bytes: 256 * 1024 * 1024,
            body_timeout: Duration::from_secs(60),
            header_timeout: Duration::from_secs(900),
            listing_idle_interval: Duration::from_secs(5),
            listing_busy_interval: Duration::from_millis(500),
            tick: Duration::from_millis(100),
            lease_event_interval: Duration::from_secs(3),
            header_read_timeout: Duration::from_secs(30),
            max_connections: 512,
            reload_timeout: Duration::from_secs(120),
            session_check_interval: Duration::from_secs(5),
            maintenance_idle: Duration::from_secs(60),
        }
    }
}

/// `http://host:port` (no credentials, path, query or fragment), normalised without a trailing
/// slash; `None` for anything else.
pub fn upstream_base(raw: &str) -> Option<String> {
    let uri: axum::http::Uri = raw.parse().ok()?;
    if uri.scheme_str() != Some("http") {
        return None;
    }
    let authority = uri.authority()?;
    if authority.as_str().contains('@') || authority.port_u16().is_none() {
        return None;
    }
    let path = uri.path();
    if !(path.is_empty() || path == "/") || uri.query().is_some() || raw.contains('#') {
        return None;
    }
    Some(format!("http://{}", authority.as_str()))
}

/// How a waiter's wait ended.
#[derive(Debug)]
enum Outcome {
    Granted(Grant, u64),
    Rejected(Refusal),
}

struct Inner {
    sched: Scheduler,
    waiters: HashMap<WaiterKey, oneshot::Sender<Outcome>>,
    lease_by_hash: HashMap<[u8; 32], LeaseKey>,
    hash_by_lease: HashMap<LeaseKey, [u8; 32]>,
    gate_refusals: BTreeMap<&'static str, u64>,
    router_was_up: Option<bool>,
    /// Conversation id to the model its session runs on, as admitted (bounded, oldest first out).
    sessions: Sessions,
    /// Buffered-body KiB held by requests waiting in the queue.
    queued_kib: usize,
    /// The token hash of the maintenance hold, while one is held (the token itself is never kept).
    hold: Option<([u8; 32], HoldKey)>,
}

/// Most conversation ids remembered.
const MAX_SESSIONS: usize = 4096;

/// The conversation-id-to-model map: bounded, the oldest entry is forgotten first.
#[derive(Default)]
pub(crate) struct Sessions {
    map: HashMap<String, String>,
    order: std::collections::VecDeque<String>,
}

impl Sessions {
    pub(crate) fn remember(&mut self, conv_id: &str, model: &str) {
        if self
            .map
            .insert(conv_id.to_owned(), model.to_owned())
            .is_none()
        {
            self.order.push_back(conv_id.to_owned());
            while self.order.len() > MAX_SESSIONS {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }

    pub(crate) fn model(&self, conv_id: &str) -> Option<&str> {
        self.map.get(conv_id).map(String::as_str)
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

pub(crate) struct Gate {
    cfg: GateConfig,
    start: Instant,
    inner: Mutex<Inner>,
    client: Client<HttpConnector, Body>,
    buffer: Arc<Semaphore>,
    buffer_unit: usize,
    buffer_permits: usize,
    kick: Notify,
    /// Woken when the maintenance hold ends (released or expired).
    pub(crate) hold_ended: Notify,
}

impl Gate {
    fn new(cfg: GateConfig) -> Arc<Self> {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        connector.set_nodelay(true);
        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector);
        // Buffered bytes are counted in KiB permits (a semaphore holds at most u32::MAX), rounded
        // up as each body's need is, so a body of the full buffer size always fits.
        let buffer_unit = 1024;
        let permits = cfg
            .max_buffered_bytes
            .div_ceil(buffer_unit)
            .clamp(1, Semaphore::MAX_PERMITS);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                sched: Scheduler::new(cfg.admission.clone()),
                waiters: HashMap::new(),
                lease_by_hash: HashMap::new(),
                hash_by_lease: HashMap::new(),
                gate_refusals: BTreeMap::new(),
                router_was_up: None,
                sessions: Sessions::default(),
                queued_kib: 0,
                hold: None,
            }),
            cfg,
            start: Instant::now(),
            client,
            buffer: Arc::new(Semaphore::new(permits)),
            buffer_unit,
            buffer_permits: permits,
            kick: Notify::new(),
            hold_ended: Notify::new(),
        })
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `f` on the scheduler, then advances it and carries out its actions.
    fn with<R>(self: &Arc<Self>, f: impl FnOnce(&mut Inner, u64) -> R) -> R {
        let now = self.now();
        let mut inner = self.lock();
        let result = f(&mut inner, now);
        self.pump(&mut inner, now);
        result
    }

    fn pump(self: &Arc<Self>, inner: &mut Inner, now: u64) {
        let router_up = !inner.sched.router_down();
        if inner.router_was_up != Some(router_up) {
            if inner.router_was_up.is_some() {
                log(
                    if router_up {
                        "gate.router_up"
                    } else {
                        "gate.router_down"
                    },
                    &[],
                );
            }
            inner.router_was_up = Some(router_up);
        }
        for action in inner.sched.poll(now) {
            match action {
                Action::Unload { model } => {
                    log("gate.switch", &[("unload", model.as_str().into())]);
                    let gate = Arc::clone(self);
                    tokio::spawn(async move { gate.unload_for_switch(model).await });
                }
                Action::Granted {
                    waiter,
                    grant,
                    waited_ms,
                } => {
                    let model = match &grant {
                        Grant::Request(t) => Some(t.model.clone()),
                        Grant::Lease(k) => inner.sched.lease_model(*k).map(str::to_owned),
                    };
                    log(
                        "gate.grant",
                        &[
                            ("model", model.unwrap_or_default().into()),
                            ("waited_ms", waited_ms.into()),
                        ],
                    );
                    let undeliverable = match inner.waiters.remove(&waiter) {
                        Some(tx) => tx.send(Outcome::Granted(grant, waited_ms)).err(),
                        None => Some(Outcome::Granted(grant, waited_ms)),
                    };
                    // The waiter's client is gone: give back what it was granted.
                    if let Some(Outcome::Granted(grant, _)) = undeliverable {
                        match grant {
                            Grant::Request(ticket) => inner.sched.finish(now, &ticket),
                            Grant::Lease(lease) => {
                                inner.sched.release_lease(lease);
                                forget_lease(inner, lease);
                            }
                        }
                    }
                }
                Action::Rejected {
                    waiter,
                    refusal,
                    waited_ms,
                } => {
                    log(
                        "gate.refuse",
                        &[
                            ("code", refusal.code().into()),
                            ("waited_ms", waited_ms.into()),
                        ],
                    );
                    if let Some(tx) = inner.waiters.remove(&waiter) {
                        let _ = tx.send(Outcome::Rejected(refusal));
                    }
                }
                Action::LeaseEnded { lease, end } => {
                    log(
                        "gate.lease_end",
                        &[("end", format!("{end:?}").to_lowercase().into())],
                    );
                    forget_lease(inner, lease);
                }
                Action::HoldEnded { hold, end } => {
                    log(
                        "gate.maintenance_end",
                        &[("end", format!("{end:?}").to_lowercase().into())],
                    );
                    if inner.hold.is_some_and(|(_, key)| key == hold) {
                        inner.hold = None;
                    }
                    self.hold_ended.notify_waiters();
                }
            }
        }
    }

    fn count_refusal(self: &Arc<Self>, refusal: Refusal) {
        let mut inner = self.lock();
        *inner.gate_refusals.entry(refusal.code()).or_insert(0) += 1;
    }

    /// Asks the router to unload an idle model for a switch, and reports the answer.
    async fn unload_for_switch(self: Arc<Self>, model: String) {
        let outcome = self.post_unload(&model).await;
        self.with(|inner, now| inner.sched.unload_result(now, &model, outcome));
        self.kick.notify_one();
    }

    async fn post_unload(&self, model: &str) -> UnloadOutcome {
        let body = serde_json::json!({ "model": model }).to_string();
        let uri = format!("{}/models/unload", self.cfg.upstream);
        let request = axum::http::Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body));
        let Ok(request) = request else {
            return UnloadOutcome::Failed;
        };
        match tokio::time::timeout(Duration::from_secs(30), self.client.request(request)).await {
            Ok(Ok(response)) => unload_outcome(response.status().as_u16()),
            _ => UnloadOutcome::Failed,
        }
    }

    /// The router's listing poller runs at once.
    pub(crate) fn kick(&self) {
        self.kick.notify_one();
    }

    /// Asks the router which held detached session generations still run. Only an explicit
    /// answer counts: a generation reported running has its hold renewed, one reported done
    /// (`is_done: true`, which the router keeps listed for minutes) releases its hold. An id the
    /// answer leaves out, an empty answer or a failed read change nothing: the router leaves out
    /// sessions it could not ask about, so absence is not proof of an end, and each such hold
    /// ends on its own. Answers are applied only to the hold they were asked about (its number),
    /// never to one made since. Called on every tick of the check timer; it returns at once
    /// while no hold exists.
    async fn check_sessions(self: &Arc<Self>) {
        let held = {
            let now = self.now();
            self.lock().sched.held_sessions(now)
        };
        if held.is_empty() {
            return;
        }
        let ids: Vec<String> = held.iter().map(|(c, _)| c.clone()).collect();
        let body = serde_json::json!({ "conversation_ids": ids }).to_string();
        let request = axum::http::Request::builder()
            .method(Method::POST)
            .uri(format!("{}/v1/streams/lookup", self.cfg.upstream))
            .header("content-type", "application/json")
            .body(Body::from(body));
        let Ok(request) = request else {
            return;
        };
        let Ok(Ok(response)) =
            tokio::time::timeout(Duration::from_secs(5), self.client.request(request)).await
        else {
            return;
        };
        if !response.status().is_success() {
            return;
        }
        use http_body_util::BodyExt;
        let limited = http_body_util::Limited::new(response.into_body(), 1024 * 1024);
        let Ok(Ok(collected)) =
            tokio::time::timeout(Duration::from_secs(5), limited.collect()).await
        else {
            return;
        };
        let Some(states) = session_states(&collected.to_bytes(), &ids) else {
            return;
        };
        let mut released = Vec::new();
        self.with(|inner, now| {
            for ((conv_id, generation), state) in held.iter().zip(states) {
                match state {
                    SessionState::Running => {
                        inner.sched.extend_session(now, conv_id, *generation);
                    }
                    SessionState::Done => {
                        if inner.sched.release_session_generation(conv_id, *generation) {
                            if let Some(model) = inner.sessions.model(conv_id) {
                                released.push(model.to_owned());
                            }
                        }
                    }
                    SessionState::Unknown => {}
                }
            }
        });
        for model in released {
            log(
                "gate.session_release",
                &[("model", model.into()), ("why", "ended".into())],
            );
        }
        self.kick.notify_one();
    }

    /// The session `conv_id` was stopped or its generation ended: its hold ends now.
    pub(crate) fn end_session(self: &Arc<Self>, conv_id: &str, why: &'static str) {
        let model = self.with(|inner, _| {
            if inner.sched.release_session(conv_id) {
                inner.sessions.model(conv_id).map(str::to_owned)
            } else {
                None
            }
        });
        if let Some(model) = model {
            log(
                "gate.session_release",
                &[("model", model.into()), ("why", why.into())],
            );
            self.kick.notify_one();
        }
    }

    /// Reads the router's listing once and feeds it to the scheduler. A refused or failed
    /// connection marks the router unreachable at once; a slow or malformed answer counts as one
    /// failed listing (several in a row mark it unreachable).
    async fn refresh_listing(self: &Arc<Self>) {
        enum Read {
            Listing(inference_admission::Listing),
            Unreachable,
            Failed,
        }
        // Numbered before the read starts, so an answer that arrives late is judged by when it
        // was asked for.
        let ticket = self.lock().sched.listing_requested();
        let uri = format!("{}/models", self.cfg.upstream);
        let request = axum::http::Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty());
        let read = match request {
            Ok(request) => {
                match tokio::time::timeout(Duration::from_secs(5), self.client.request(request))
                    .await
                {
                    Ok(Ok(response)) if response.status().is_success() => {
                        use http_body_util::BodyExt;
                        let limited =
                            http_body_util::Limited::new(response.into_body(), 8 * 1024 * 1024);
                        match tokio::time::timeout(Duration::from_secs(5), limited.collect()).await
                        {
                            Ok(Ok(collected)) => {
                                match inference_admission::parse_listing(&collected.to_bytes()) {
                                    Some(listing) => Read::Listing(listing),
                                    None => Read::Failed,
                                }
                            }
                            _ => Read::Failed,
                        }
                    }
                    Ok(Err(err)) if err.is_connect() => Read::Unreachable,
                    _ => Read::Failed,
                }
            }
            Err(_) => Read::Failed,
        };
        self.with(|inner, now| match read {
            Read::Listing(listing) => inner.sched.observe_at(ticket, listing),
            Read::Unreachable => inner.sched.router_unreachable(now),
            Read::Failed => inner.sched.listing_failed(now),
        });
    }

    /// `model` names a model or alias the router lists; reads the listing again once when the
    /// last one does not know it (a model added since).
    pub(crate) async fn known(self: &Arc<Self>, model: &str) -> bool {
        if self.lock().sched.knows(model) {
            return true;
        }
        self.refresh_listing().await;
        self.lock().sched.knows(model)
    }

    fn admin(&self, headers: &HeaderMap) -> bool {
        let Some(token) = self.cfg.admin_token.as_deref() else {
            return false;
        };
        let mut values = headers.get_all("x-noevia-gate-admin").iter();
        let (Some(given), None) = (values.next(), values.next()) else {
            return false;
        };
        use subtle::ConstantTimeEq;
        let a = Sha256::digest(given.as_bytes());
        let b = Sha256::digest(token.as_bytes());
        a.as_slice().ct_eq(b.as_slice()).into()
    }
}

/// What a session lookup says about one conversation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionState {
    /// A row for it is not done (a row without `is_done` counts as running).
    Running,
    /// Every row for it is done.
    Done,
    /// No row for it: the router may not have been able to ask (no change).
    Unknown,
}

/// Reads the router's session lookup answer (an array of `{conversation_id, is_done}`) for each
/// of `asked` (the router keys per-model variants as `<id>::<model>`). `None`: not a usable
/// answer.
pub(crate) fn session_states(answer: &[u8], asked: &[String]) -> Option<Vec<SessionState>> {
    let rows = serde_json::from_slice::<serde_json::Value>(answer).ok()?;
    let rows: Vec<(&str, bool)> = rows
        .as_array()?
        .iter()
        .filter_map(|row| {
            let id = row.get("conversation_id")?.as_str()?;
            let done = row
                .get("is_done")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Some((id, done))
        })
        .collect();
    Some(
        asked
            .iter()
            .map(|id| {
                let mine: Vec<bool> = rows
                    .iter()
                    .filter(|(row_id, _)| {
                        *row_id == id.as_str()
                            || row_id
                                .strip_prefix(id.as_str())
                                .is_some_and(|rest| rest.starts_with("::"))
                    })
                    .map(|(_, done)| *done)
                    .collect();
                if mine.is_empty() {
                    SessionState::Unknown
                } else if mine.iter().all(|done| *done) {
                    SessionState::Done
                } else {
                    SessionState::Running
                }
            })
            .collect(),
    )
}

/// The scheduler's reading of the router's answer to an unload. Any answer means the router is
/// up; only a missing answer (`UnloadOutcome::Failed`, decided by the caller) suggests otherwise.
pub(crate) fn unload_outcome(status: u16) -> UnloadOutcome {
    match status {
        200..=299 => UnloadOutcome::Accepted,
        // The router answers 400 "model is not running" (or not found) when nothing is there.
        400 | 404 => UnloadOutcome::NotRunning,
        _ => UnloadOutcome::Refused,
    }
}

fn forget_lease(inner: &mut Inner, lease: LeaseKey) {
    if let Some(hash) = inner.hash_by_lease.remove(&lease) {
        inner.lease_by_hash.remove(&hash);
    }
}

/// One JSON log line. Model ids, codes, counts and times only: never request content.
pub(crate) fn log(event: &str, fields: &[(&str, serde_json::Value)]) {
    let mut line = serde_json::Map::new();
    line.insert("event".into(), event.into());
    for (key, value) in fields {
        line.insert((*key).to_owned(), value.clone());
    }
    println!("{}", serde_json::Value::Object(line));
}

/// A request in flight: counted until it is dropped (its response body ended or its client left).
/// A session request (one with a conversation id) whose client left before its answer ended may
/// go on generating in the router for a later resume, so its model is then held for the session
/// hold.
pub(crate) struct InflightGuard {
    gate: Arc<Gate>,
    ticket: Option<Ticket>,
    /// The conversation id of a session generation.
    session: Option<String>,
    /// The conversation id of a resume: its end means the generation ended.
    resume_of: Option<String>,
    completed: bool,
}

impl InflightGuard {
    pub(crate) fn new(gate: Arc<Gate>, ticket: Ticket) -> Self {
        Self {
            gate,
            ticket: Some(ticket),
            session: None,
            resume_of: None,
            completed: false,
        }
    }

    /// The model the request runs on.
    pub(crate) fn model(&self) -> Option<&str> {
        self.ticket.as_ref().map(|t| t.model.as_str())
    }

    /// Marks the request as a generation of the resumable session `conv_id`.
    pub(crate) fn set_session(&mut self, conv_id: String) {
        self.session = Some(conv_id);
    }

    /// Marks the request as a resume of session `conv_id`.
    pub(crate) fn set_resume_of(&mut self, conv_id: String) {
        self.resume_of = Some(conv_id);
    }

    /// The router did not answer the resume with the session's event stream (for example the
    /// offset was lost, or the owning model is loading): its end says nothing about the
    /// generation.
    pub(crate) fn clear_resume_of(&mut self) {
        self.resume_of = None;
    }

    /// The response body reached its end.
    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            let detached = if self.completed {
                None
            } else {
                self.session.take()
            };
            self.gate.with(|inner, now| {
                inner.sched.finish(now, &ticket);
                if let Some(conv_id) = &detached {
                    inner.sched.hold_session(now, conv_id, &ticket.model);
                }
            });
            // The generation reached its end (directly or through a resume): no hold is needed.
            if self.completed {
                for conv_id in [self.session.take(), self.resume_of.take()]
                    .into_iter()
                    .flatten()
                {
                    self.gate.end_session(&conv_id, "ended");
                }
            }
            if detached.is_some() {
                log(
                    "gate.session_hold",
                    &[("model", ticket.model.as_str().into())],
                );
            }
        }
    }
}

/// A waiter: cancelled when dropped before its answer arrived (its client went away). An answer
/// that arrived but was never read is given back.
pub(crate) struct WaitGuard {
    gate: Arc<Gate>,
    key: WaiterKey,
    rx: Option<oneshot::Receiver<Outcome>>,
}

impl WaitGuard {
    async fn wait(&mut self, limit: Duration) -> Option<Outcome> {
        let rx = self.rx.as_mut()?;
        let outcome = tokio::time::timeout(limit, rx).await.ok()?.ok();
        if outcome.is_some() {
            self.rx = None;
        }
        outcome
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else {
            return;
        };
        let key = self.key;
        self.gate.with(|inner, now| {
            inner.waiters.remove(&key);
            inner.sched.cancel(key);
            if let Ok(Outcome::Granted(grant, _)) = rx.try_recv() {
                match grant {
                    Grant::Request(ticket) => inner.sched.finish(now, &ticket),
                    Grant::Lease(lease) => {
                        inner.sched.release_lease(lease);
                        forget_lease(inner, lease);
                    }
                }
            }
        });
    }
}

/// Serves the gate on `listener` until `shutdown` completes.
pub async fn run(
    listener: tokio::net::TcpListener,
    config: GateConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let gate = Gate::new(config);
    gate.refresh_listing().await;
    let ticker = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(gate.cfg.tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                gate.with(|_, _| ());
            }
        })
    };
    let session_checker = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(gate.cfg.session_check_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                gate.check_sessions().await;
            }
        })
    };
    let poller = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            loop {
                let busy = {
                    let inner = gate.lock();
                    inner.sched.switching()
                        || inner.sched.waiting() > 0
                        || inner.sched.router_down()
                };
                let wait = if busy {
                    gate.cfg.listing_busy_interval
                } else {
                    gate.cfg.listing_idle_interval
                };
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = gate.kick.notified() => {}
                }
                gate.refresh_listing().await;
            }
        })
    };
    log(
        "gate.start",
        &[
            ("max_wait_ms", gate.cfg.admission.max_wait_ms.into()),
            ("fair_hold_ms", gate.cfg.admission.fair_hold_ms.into()),
            ("max_waiters", gate.cfg.admission.max_waiters.into()),
            ("admin", gate.cfg.admin_token.is_some().into()),
        ],
    );
    let app = Router::new().fallback(handle).with_state(Arc::clone(&gate));
    // Served connection by connection so that a client must send its headers in time and the
    // number of open connections is bounded (one past the limit is closed at once).
    let connections = Arc::new(Semaphore::new(
        gate.cfg.max_connections.clamp(1, Semaphore::MAX_PERMITS),
    ));
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        // Out of descriptors or a transient accept error: back off briefly.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                    *gate.lock().gate_refusals.entry("connection_limit").or_insert(0) += 1;
                    drop(stream);
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let service = hyper_util::service::TowerToHyperService::new(app.clone());
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(gate.cfg.header_read_timeout);
                let connection =
                    builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
                let watched = graceful.watch(connection);
                tokio::spawn(async move {
                    let _ = watched.await;
                    drop(permit);
                });
            }
            () = &mut shutdown => break,
        }
    }
    drop(listener);
    // Open event streams would hold a graceful shutdown forever: bounded.
    let _ = tokio::time::timeout(Duration::from_secs(10), graceful.shutdown()).await;
    ticker.abort();
    poller.abort();
    session_checker.abort();
    Ok(())
}

async fn handle(State(gate): State<Arc<Gate>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/gate/v1" || path.starts_with("/gate/v1/") {
        return leases::gate_endpoint(gate, request).await;
    }
    forward::proxy(gate, request).await
}

/// Buffered-body KiB held by queued requests, given back when the wait ends however it ends.
struct QueuedBytes {
    gate: Arc<Gate>,
    kib: usize,
}

impl Drop for QueuedBytes {
    fn drop(&mut self) {
        let mut inner = self.gate.lock();
        inner.queued_kib = inner.queued_kib.saturating_sub(self.kib);
    }
}

/// Admits a request that may load or run on `model` (waiting if it must), or refuses it.
/// `Ok(None)`: it cannot load or use the slot and is forwarded without being counted.
/// `body_kib` is its buffered body: waiting requests may hold at most half of the gate's body
/// buffer, so they can never starve the requests already admitted.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_request(
    gate: &Arc<Gate>,
    model: &str,
    autoload: bool,
    lease: Option<LeaseKey>,
    max_wait_ms: u64,
    body_kib: usize,
    by_hold: bool,
) -> Result<Option<InflightGuard>, Refusal> {
    if !gate.known(model).await {
        return Err(Refusal::ModelNotFound);
    }
    let queued_cap = gate.buffer_permits / 2;
    let wait = Duration::from_millis(max_wait_ms.min(gate.cfg.admission.max_wait_ms));
    let started = tokio::time::Instant::now();
    let (arrival, rx, queued_bytes) = loop {
        // Time spent waiting out a maintenance hold counts against the request's wait limit.
        let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let left = max_wait_ms.saturating_sub(waited).max(1);
        let attempt = gate.with(|inner, now| {
            let mut arrival = if autoload {
                inner
                    .sched
                    .arrive_by(now, model, WaitKind::Request, lease, left, by_hold)
            } else {
                inner
                    .sched
                    .arrive_without_autoload_by(now, model, lease, left, by_hold)
            };
            let mut queued_bytes = None;
            if let Arrival::Queued { waiter, .. } = &arrival {
                if inner.queued_kib.saturating_add(body_kib) > queued_cap {
                    inner.sched.cancel(*waiter);
                    arrival = Arrival::Refused(Refusal::GateBusy);
                } else {
                    inner.queued_kib += body_kib;
                    queued_bytes = Some(QueuedBytes {
                        gate: Arc::clone(gate),
                        kib: body_kib,
                    });
                }
            }
            let rx = match &arrival {
                Arrival::Queued { waiter, .. } => {
                    let (tx, rx) = oneshot::channel();
                    inner.waiters.insert(*waiter, tx);
                    Some(rx)
                }
                _ => None,
            };
            (arrival, rx, queued_bytes)
        });
        if !matches!(attempt.0, Arrival::Refused(Refusal::MaintenanceHeld)) {
            break attempt;
        }
        // Another client holds the engine for maintenance: wait for its end, then ask again.
        if !wait_for_hold_end(gate, wait.saturating_sub(started.elapsed())).await {
            gate.count_refusal(Refusal::MaintenanceHeld);
            return Err(Refusal::MaintenanceHeld);
        }
    };
    match arrival {
        Arrival::Pass => Ok(None),
        Arrival::Admitted(inference_admission::Admission::Request(ticket)) => {
            Ok(Some(InflightGuard::new(Arc::clone(gate), ticket)))
        }
        Arrival::Admitted(inference_admission::Admission::Lease(_)) => Err(Refusal::InvalidLease),
        Arrival::Refused(refusal) => Err(refusal),
        Arrival::Queued { waiter, position } => {
            let _queued_bytes = queued_bytes;
            log(
                "gate.queue",
                &[("model", model.into()), ("position", position.into())],
            );
            let mut guard = WaitGuard {
                gate: Arc::clone(gate),
                key: waiter,
                rx,
            };
            // The scheduler answers by the deadline; the extra second only guards against a
            // stalled clock.
            let limit = Duration::from_millis(max_wait_ms.min(gate.cfg.admission.max_wait_ms))
                + Duration::from_secs(1);
            match guard.wait(limit).await {
                Some(Outcome::Granted(Grant::Request(ticket), _)) => {
                    Ok(Some(InflightGuard::new(Arc::clone(gate), ticket)))
                }
                Some(Outcome::Granted(Grant::Lease(lease), _)) => {
                    gate.with(|inner, _| {
                        inner.sched.release_lease(lease);
                        forget_lease(inner, lease);
                    });
                    Err(Refusal::InvalidLease)
                }
                Some(Outcome::Rejected(refusal)) => Err(refusal),
                None => Err(Refusal::QueueTimeout),
            }
        }
    }
}

/// Waits until no maintenance hold is held, for at most `limit`. `false`: one is still held.
pub(crate) async fn wait_for_hold_end(gate: &Arc<Gate>, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        // Registered before the check, so an end between the check and the wait is not missed.
        let ended = gate.hold_ended.notified();
        tokio::pin!(ended);
        ended.as_mut().enable();
        if !gate.lock().sched.held() {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        // The idle expiry is noticed on a tick: wake at least that often.
        let nap = (deadline - now).min(gate.cfg.tick.max(Duration::from_millis(10)) * 5);
        tokio::select! {
            () = &mut ended => {}
            () = tokio::time::sleep(nap) => {}
        }
    }
}

/// Parses `X-Noevia-Maintenance`: absent (`false`), or exactly one 32-character lowercase hex token
/// that names the live maintenance hold (`true`).
pub(crate) fn hold_from_headers(gate: &Arc<Gate>, headers: &HeaderMap) -> Result<bool, Refusal> {
    let mut values = headers.get_all("x-noevia-maintenance").iter();
    let Some(value) = values.next() else {
        return Ok(false);
    };
    if values.next().is_some() {
        return Err(Refusal::InvalidHold);
    }
    let token = value.to_str().map_err(|_| Refusal::InvalidHold)?;
    if !valid_token(token) {
        return Err(Refusal::InvalidHold);
    }
    let hash = token_hash(token);
    let now = gate.now();
    let inner = gate.lock();
    match inner.hold {
        Some((held, key)) if held == hash && inner.sched.hold_live(now, key) => Ok(true),
        _ => Err(Refusal::HoldUnknown),
    }
}

/// Hashes a lease token for lookup (the token itself is never stored).
pub(crate) fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Parses `X-Noevia-Lease`: absent, or exactly one 32-character lowercase hex token that names a
/// live lease.
pub(crate) fn lease_from_headers(
    gate: &Arc<Gate>,
    headers: &HeaderMap,
) -> Result<Option<LeaseKey>, Refusal> {
    let mut values = headers.get_all("x-noevia-lease").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Refusal::InvalidLease);
    }
    let token = value.to_str().map_err(|_| Refusal::InvalidLease)?;
    if !valid_token(token) {
        return Err(Refusal::InvalidLease);
    }
    let hash = token_hash(token);
    let inner = gate.lock();
    inner
        .lease_by_hash
        .get(&hash)
        .copied()
        .map(Some)
        .ok_or(Refusal::LeaseUnknown)
}

pub(crate) fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `X-Noevia-Max-Wait-Ms`: absent (the configured maximum) or 1 to 86 400 000.
pub(crate) fn max_wait_from_headers(gate: &Gate, headers: &HeaderMap) -> Result<u64, Refusal> {
    let mut values = headers.get_all("x-noevia-max-wait-ms").iter();
    let Some(value) = values.next() else {
        return Ok(gate.cfg.admission.max_wait_ms);
    };
    if values.next().is_some() {
        return Err(Refusal::InvalidWait);
    }
    let parsed = value
        .to_str()
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| (1..=86_400_000).contains(v))
        .ok_or(Refusal::InvalidWait)?;
    Ok(parsed.min(gate.cfg.admission.max_wait_ms))
}

/// A short client label for logs (`[a-z0-9-]`, at most 24 characters), or `relay`.
pub(crate) fn client_label(headers: &HeaderMap) -> String {
    headers
        .get("x-noevia-client")
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 24
                && v.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        })
        .unwrap_or("relay")
        .to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn upstream_base_accepts_only_plain_http_host_port() {
        assert_eq!(
            upstream_base("http://router:8080").as_deref(),
            Some("http://router:8080")
        );
        assert_eq!(
            upstream_base("http://router:8080/").as_deref(),
            Some("http://router:8080")
        );
        for bad in [
            "https://router:8080",
            "http://router",
            "http://a:b@router:8080",
            "http://router:8080/v1",
            "http://router:8080/?x=1",
            "router:8080",
            "",
        ] {
            assert!(upstream_base(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn unload_answers_map_to_outcomes() {
        assert_eq!(unload_outcome(200), UnloadOutcome::Accepted);
        assert_eq!(unload_outcome(400), UnloadOutcome::NotRunning);
        assert_eq!(unload_outcome(404), UnloadOutcome::NotRunning);
        assert_eq!(unload_outcome(500), UnloadOutcome::Refused);
        assert_eq!(unload_outcome(503), UnloadOutcome::Refused);
    }

    #[test]
    fn session_lookups_release_only_on_an_explicit_done() {
        use SessionState::{Done, Running, Unknown};
        let asked: Vec<String> = ["c1", "c2", "c3", "c4", "c5", "c6"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let answer = br#"[
            {"conversation_id":"c1","is_done":false},
            {"conversation_id":"c2","is_done":true},
            {"conversation_id":"c3::route-code","is_done":false},
            {"conversation_id":"c40","is_done":true},
            {"conversation_id":"c5"},
            {"conversation_id":"c6::a","is_done":true},
            {"conversation_id":"c6::b","is_done":false}
        ]"#;
        assert_eq!(
            session_states(answer, &asked),
            Some(vec![Running, Done, Running, Unknown, Running, Running])
        );
        assert_eq!(session_states(b"[]", &asked), Some(vec![Unknown; 6]));
        assert_eq!(session_states(b"{}", &asked), None);
        assert_eq!(session_states(b"not json", &asked), None);
    }

    #[test]
    fn tokens_are_32_lowercase_hex() {
        assert!(valid_token("0123456789abcdef0123456789abcdef"));
        assert!(!valid_token("0123456789ABCDEF0123456789abcdef"));
        assert!(!valid_token("0123456789abcdef"));
        assert!(!valid_token("0123456789abcdef0123456789abcdeg"));
    }

    #[test]
    fn client_labels_are_sanitised() {
        let mut h = HeaderMap::new();
        assert_eq!(client_label(&h), "relay");
        h.insert("x-noevia-client", "web".parse().unwrap());
        assert_eq!(client_label(&h), "web");
        h.insert("x-noevia-client", "Web\"x".parse().unwrap());
        assert_eq!(client_label(&h), "relay");
    }
}
