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
//! - `POST /gate/v1/leases` `{model, holder?, max_wait_ms?}` and `DELETE /gate/v1/leases/{lease}`.
//!
//! Request headers the gate reads and removes: `X-Noevia-Lease`, `X-Noevia-Max-Wait-Ms` (a shorter
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
    Action, Arrival, Config, Grant, LeaseKey, Refusal, Scheduler, Ticket, UnloadOutcome, WaitKind,
    WaiterKey,
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
}

pub(crate) struct Gate {
    cfg: GateConfig,
    start: Instant,
    inner: Mutex<Inner>,
    client: Client<HttpConnector, Body>,
    buffer: Arc<Semaphore>,
    buffer_unit: usize,
    kick: Notify,
}

impl Gate {
    fn new(cfg: GateConfig) -> Arc<Self> {
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(Duration::from_secs(5)));
        connector.set_nodelay(true);
        let client = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(connector);
        // Buffered bytes are counted in KiB permits (a semaphore holds at most u32::MAX).
        let buffer_unit = 1024;
        let permits = (cfg.max_buffered_bytes / buffer_unit).clamp(1, Semaphore::MAX_PERMITS);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                sched: Scheduler::new(cfg.admission.clone()),
                waiters: HashMap::new(),
                lease_by_hash: HashMap::new(),
                hash_by_lease: HashMap::new(),
                gate_refusals: BTreeMap::new(),
                router_was_up: None,
            }),
            cfg,
            start: Instant::now(),
            client,
            buffer: Arc::new(Semaphore::new(permits)),
            buffer_unit,
            kick: Notify::new(),
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

    /// Reads the router's listing once and feeds it to the scheduler.
    async fn refresh_listing(self: &Arc<Self>) {
        let uri = format!("{}/models", self.cfg.upstream);
        let request = axum::http::Request::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Body::empty());
        let listing = match request {
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
                                inference_admission::parse_listing(&collected.to_bytes())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
            Err(_) => None,
        };
        self.with(|inner, now| match listing {
            Some(listing) => inner.sched.observe(listing),
            None => inner.sched.router_unreachable(now),
        });
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

fn unload_outcome(status: u16) -> UnloadOutcome {
    match status {
        200..=299 => UnloadOutcome::Accepted,
        // The router answers 400 "model is not running" (or not found) when nothing is there.
        400 | 404 => UnloadOutcome::NotRunning,
        _ => UnloadOutcome::Failed,
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
pub(crate) struct InflightGuard {
    gate: Arc<Gate>,
    ticket: Option<Ticket>,
}

impl InflightGuard {
    fn new(gate: Arc<Gate>, ticket: Ticket) -> Self {
        Self {
            gate,
            ticket: Some(ticket),
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            self.gate
                .with(|inner, now| inner.sched.finish(now, &ticket));
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
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    ticker.abort();
    poller.abort();
    served
}

async fn handle(State(gate): State<Arc<Gate>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/gate/v1" || path.starts_with("/gate/v1/") {
        return leases::gate_endpoint(gate, request).await;
    }
    forward::proxy(gate, request).await
}

/// Admits a request that may load or run on `model` (waiting if it must), or refuses it.
pub(crate) async fn admit_request(
    gate: &Arc<Gate>,
    model: &str,
    lease: Option<LeaseKey>,
    max_wait_ms: u64,
) -> Result<InflightGuard, Refusal> {
    let (arrival, rx) = gate.with(|inner, now| {
        let arrival = inner
            .sched
            .arrive(now, model, WaitKind::Request, lease, max_wait_ms);
        let rx = match &arrival {
            Arrival::Queued { waiter, .. } => {
                let (tx, rx) = oneshot::channel();
                inner.waiters.insert(*waiter, tx);
                Some(rx)
            }
            _ => None,
        };
        (arrival, rx)
    });
    match arrival {
        Arrival::Admitted(inference_admission::Admission::Request(ticket)) => {
            Ok(InflightGuard::new(Arc::clone(gate), ticket))
        }
        Arrival::Admitted(inference_admission::Admission::Lease(_)) => Err(Refusal::InvalidLease),
        Arrival::Refused(refusal) => Err(refusal),
        Arrival::Queued { waiter, position } => {
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
                    Ok(InflightGuard::new(Arc::clone(gate), ticket))
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
        assert_eq!(unload_outcome(500), UnloadOutcome::Failed);
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
