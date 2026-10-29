//! The gate's own endpoints: health, status, metrics, leases, and the maintenance hold.

use crate::respond;
use crate::{
    forget_lease, hold_from_headers, log, token_hash, valid_token, wait_for_hold_end, Gate,
    Outcome, WaitGuard,
};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use http_body_util::BodyExt;
use inference_admission::{Admission, Arrival, Grant, HoldKey, LeaseKey, Refusal, WaitKind};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const LEASE_BODY_LIMIT: usize = 4096;

pub(crate) async fn gate_endpoint(gate: Arc<Gate>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    match (method, path.as_str()) {
        (Method::GET, "/gate/v1/health") => {
            let router_up = !gate.lock().sched.router_down();
            respond::json(
                200,
                &serde_json::json!({ "ok": true, "router_up": router_up }),
            )
        }
        (Method::GET, "/gate/v1/status") => {
            let now = gate.now();
            let inner = gate.lock();
            let status = inner.sched.status(now);
            let hold = inner.sched.hold_status(now);
            respond::json(
                200,
                &serde_json::json!({
                    "scheduler": status,
                    "gate_refusals": inner.gate_refusals,
                    "buffer_free_kib": gate.buffer.available_permits(),
                    "queued_kib": inner.queued_kib,
                    "sessions": inner.sessions.len(),
                    // The holder's own text for people (never request content) and its times.
                    "maintenance": {
                        "held": hold.is_some(),
                        "reason": hold.as_ref().map(|h| h.reason.clone()),
                        "held_ms": hold.as_ref().map(|h| h.held_ms),
                        "idle_ttl_ms": hold.as_ref().map(|h| h.idle_ttl_ms),
                    },
                }),
            )
        }
        (Method::GET, "/gate/v1/metrics") => metrics(&gate),
        (Method::POST, "/gate/v1/leases") => acquire(gate, request).await,
        (Method::DELETE, p) if p.starts_with("/gate/v1/leases/") => {
            release(&gate, p.trim_start_matches("/gate/v1/leases/"))
        }
        (Method::POST, "/gate/v1/maintenance") => take_hold(gate, request).await,
        (Method::POST, p) if p.starts_with("/gate/v1/maintenance/") && p.ends_with("/renew") => {
            let token = p
                .trim_start_matches("/gate/v1/maintenance/")
                .trim_end_matches("/renew")
                .to_owned();
            renew_hold(gate, request, &token).await
        }
        (Method::DELETE, p) if p.starts_with("/gate/v1/maintenance/") => {
            let token = p.trim_start_matches("/gate/v1/maintenance/").to_owned();
            end_hold(&gate, request.headers(), &token)
        }
        _ => respond::json(
            404,
            &serde_json::json!({ "error": { "code": "not_found", "message": "No such gate endpoint.", "type": "gate_error", "retryable": false } }),
        ),
    }
}

fn release(gate: &Arc<Gate>, token: &str) -> Response {
    if !valid_token(token) {
        return respond::refusal(Refusal::InvalidLease);
    }
    let hash = token_hash(token);
    let released = gate.with(|inner, _| match inner.lease_by_hash.get(&hash).copied() {
        Some(lease) => {
            inner.sched.release_lease(lease);
            forget_lease(inner, lease);
            true
        }
        None => false,
    });
    if released {
        log("gate.lease_end", &[("end", "released".into())]);
        respond::empty(StatusCode::NO_CONTENT)
    } else {
        respond::json(
            404,
            &serde_json::json!({ "error": { "code": Refusal::LeaseUnknown.code(), "message": Refusal::LeaseUnknown.message(), "type": "gate_error", "retryable": false } }),
        )
    }
}

/// `POST /gate/v1/leases` `{model, holder?, max_wait_ms?}`. A refusal is a plain JSON error; an
/// admission is an event stream: `queued` (position, waited time) every few seconds while it
/// waits, then `granted` (the lease token and its limits) or `error`. Closing the stream while
/// queued leaves the queue.
async fn acquire(gate: Arc<Gate>, request: Request) -> Response {
    let headers = request.headers().clone();
    let body = match tokio::time::timeout(
        gate.cfg.body_timeout,
        http_body_util::Limited::new(request.into_body(), LEASE_BODY_LIMIT).collect(),
    )
    .await
    {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(_)) => return respond::refusal(Refusal::BodyTooLarge),
        Err(_) => return respond::refusal(Refusal::BodyTimeout),
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return respond::refusal(Refusal::InvalidJson);
    };
    let Some(model) = value
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| inference_admission::config::valid_model_id(m))
        .map(str::to_owned)
    else {
        return respond::refusal(Refusal::InvalidModel);
    };
    if !gate.known(&model).await {
        return respond::refusal(Refusal::ModelNotFound);
    }
    let holder = value
        .get("holder")
        .and_then(|h| h.as_str())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 24
                && h.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        })
        .unwrap_or("client")
        .to_owned();
    let max_wait = match value.get("max_wait_ms") {
        None => gate.cfg.admission.max_wait_ms,
        Some(v) => match v.as_u64().filter(|v| (1..=86_400_000).contains(v)) {
            Some(v) => v.min(gate.cfg.admission.max_wait_ms),
            None => return respond::refusal(Refusal::InvalidWait),
        },
    };
    let by_hold = match hold_from_headers(&gate, &headers) {
        Ok(by_hold) => by_hold,
        Err(refusal) => return respond::refusal(refusal),
    };
    let (arrival, rx) = arrive_lease(&gate, &model, max_wait, by_hold);
    match arrival {
        // Another client holds the engine for maintenance: the stream reports the wait (as a
        // queue position of 1) until the hold ends, then asks again.
        Arrival::Refused(Refusal::MaintenanceHeld) => {
            log(
                "gate.queue",
                &[
                    ("model", model.as_str().into()),
                    ("holder", holder.as_str().into()),
                    ("position", 1.into()),
                    ("lease", true.into()),
                    ("maintenance", true.into()),
                ],
            );
            let (tx, events) = mpsc::channel::<Bytes>(4);
            tokio::spawn(lease_after_hold(gate, tx, model, holder, max_wait, by_hold));
            stream_response(events)
        }
        arrival => answer_lease(gate, arrival, rx, model, holder, max_wait),
    }
}

/// One lease arrival, with its waiter channel when it queues.
fn arrive_lease(
    gate: &Arc<Gate>,
    model: &str,
    max_wait: u64,
    by_hold: bool,
) -> (Arrival, Option<oneshot::Receiver<Outcome>>) {
    gate.with(|inner, now| {
        let arrival = inner
            .sched
            .arrive_by(now, model, WaitKind::Lease, None, max_wait, by_hold);
        let rx = match &arrival {
            Arrival::Queued { waiter, .. } => {
                let (tx, rx) = oneshot::channel();
                inner.waiters.insert(*waiter, tx);
                Some(rx)
            }
            _ => None,
        };
        (arrival, rx)
    })
}

fn answer_lease(
    gate: Arc<Gate>,
    arrival: Arrival,
    rx: Option<oneshot::Receiver<Outcome>>,
    model: String,
    holder: String,
    max_wait: u64,
) -> Response {
    match arrival {
        Arrival::Refused(refusal) => respond::refusal(refusal),
        Arrival::Admitted(Admission::Request(_)) | Arrival::Pass => {
            respond::refusal(Refusal::InvalidLease)
        }
        Arrival::Admitted(Admission::Lease(lease)) => {
            let token = issue_token(&gate, lease);
            log(
                "gate.lease",
                &[
                    ("model", model.as_str().into()),
                    ("holder", holder.as_str().into()),
                    ("waited_ms", 0.into()),
                ],
            );
            granted_now(&gate, lease, granted_event(&gate, &model, &token, 0))
        }
        Arrival::Queued { waiter, position } => {
            log(
                "gate.queue",
                &[
                    ("model", model.as_str().into()),
                    ("holder", holder.as_str().into()),
                    ("position", position.into()),
                    ("lease", true.into()),
                ],
            );
            let (tx, events) = mpsc::channel::<Bytes>(4);
            let guard = WaitGuard {
                gate: Arc::clone(&gate),
                key: waiter,
                rx,
            };
            tokio::spawn(wait_for_lease(
                gate, guard, tx, model, holder, position, max_wait,
            ));
            stream_response(events)
        }
    }
}

async fn wait_for_lease(
    gate: Arc<Gate>,
    mut guard: WaitGuard,
    tx: mpsc::Sender<Bytes>,
    model: String,
    holder: String,
    first_position: usize,
    max_wait: u64,
) {
    let started = tokio::time::Instant::now();
    let queued = |position: usize, waited: u64| {
        event(
            "queued",
            &serde_json::json!({ "model": model, "position": position, "waited_ms": waited }),
        )
    };
    if tx.send(queued(first_position, 0)).await.is_err() {
        return;
    }
    let limit = Duration::from_millis(max_wait) + Duration::from_secs(1);
    let key = guard.key;
    let mut ticker = tokio::time::interval(gate.cfg.lease_event_interval);
    ticker.tick().await;
    loop {
        let remaining = limit.saturating_sub(started.elapsed());
        tokio::select! {
            outcome = guard.wait(remaining) => {
                match outcome {
                    Some(Outcome::Granted(Grant::Lease(lease), waited)) => {
                        let token = issue_token(&gate, lease);
                        if tx.send(granted_event(&gate, &model, &token, waited)).await.is_err() {
                            // The client left as it was granted: give the lease back.
                            gate.with(|inner, _| {
                                inner.sched.release_lease(lease);
                                forget_lease(inner, lease);
                            });
                        } else {
                            log("gate.lease", &[("model", model.as_str().into()), ("holder", holder.as_str().into()), ("waited_ms", waited.into())]);
                        }
                    }
                    Some(Outcome::Granted(Grant::Request(ticket), _)) => {
                        gate.with(|inner, now| inner.sched.finish(now, &ticket));
                        let _ = tx.send(error_event(Refusal::InvalidLease)).await;
                    }
                    Some(Outcome::Rejected(refusal)) => {
                        let _ = tx.send(error_event(refusal)).await;
                    }
                    None => {
                        let _ = tx.send(error_event(Refusal::QueueTimeout)).await;
                    }
                }
                return;
            }
            _ = ticker.tick() => {
                let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let position = gate.lock().sched.position(key).unwrap_or(1);
                if tx.send(queued(position, waited)).await.is_err() {
                    return;
                }
            }
            _ = tx.closed() => return,
        }
    }
}

/// A lease request that arrived while another client held the engine for maintenance: reports
/// `queued` (position 1) every few seconds until the hold ends, then arrives again (it may be
/// granted, queue for its model, or be refused); `maintenance_held` once `max_wait` has passed.
async fn lease_after_hold(
    gate: Arc<Gate>,
    tx: mpsc::Sender<Bytes>,
    model: String,
    holder: String,
    max_wait: u64,
    by_hold: bool,
) {
    let started = tokio::time::Instant::now();
    let limit = Duration::from_millis(max_wait);
    let queued = |waited: u64| {
        event(
            "queued",
            &serde_json::json!({ "model": model, "position": 1, "waited_ms": waited }),
        )
    };
    if tx.send(queued(0)).await.is_err() {
        return;
    }
    loop {
        let left = limit.saturating_sub(started.elapsed());
        let slice = left.min(gate.cfg.lease_event_interval);
        tokio::select! {
            ended = wait_for_hold_end(&gate, slice) => {
                if !ended {
                    if started.elapsed() >= limit {
                        gate.count_refusal(Refusal::MaintenanceHeld);
                        let _ = tx.send(error_event(Refusal::MaintenanceHeld)).await;
                        return;
                    }
                    let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    if tx.send(queued(waited)).await.is_err() {
                        return;
                    }
                    continue;
                }
            }
            () = tx.closed() => return,
        }
        let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let left_ms = max_wait.saturating_sub(waited).max(1);
        let (arrival, rx) = arrive_lease(&gate, &model, left_ms, by_hold);
        match arrival {
            // Taken again at once by another holder: keep waiting.
            Arrival::Refused(Refusal::MaintenanceHeld) => {}
            Arrival::Refused(refusal) => {
                let _ = tx.send(error_event(refusal)).await;
                return;
            }
            Arrival::Admitted(Admission::Request(ticket)) => {
                gate.with(|inner, now| inner.sched.finish(now, &ticket));
                let _ = tx.send(error_event(Refusal::InvalidLease)).await;
                return;
            }
            Arrival::Pass => {
                let _ = tx.send(error_event(Refusal::InvalidLease)).await;
                return;
            }
            Arrival::Admitted(Admission::Lease(lease)) => {
                let token = issue_token(&gate, lease);
                if tx
                    .send(granted_event(&gate, &model, &token, waited))
                    .await
                    .is_err()
                {
                    gate.with(|inner, _| {
                        inner.sched.release_lease(lease);
                        forget_lease(inner, lease);
                    });
                } else {
                    log(
                        "gate.lease",
                        &[
                            ("model", model.as_str().into()),
                            ("holder", holder.as_str().into()),
                            ("waited_ms", waited.into()),
                        ],
                    );
                }
                return;
            }
            Arrival::Queued { waiter, position } => {
                let guard = WaitGuard {
                    gate: Arc::clone(&gate),
                    key: waiter,
                    rx,
                };
                wait_for_lease(gate, guard, tx, model, holder, position, left_ms).await;
                return;
            }
        }
    }
}

/// The longest a hold request may wait for the engine to become idle.
const MAX_HOLD_WAIT_MS: u64 = 600_000;
/// The idle limits a holder may ask for.
const HOLD_IDLE_RANGE_MS: std::ops::RangeInclusive<u64> = 1_000..=600_000;
/// The longest reason kept (characters).
const MAX_REASON_CHARS: usize = 300;

fn admin_required() -> Response {
    respond::json(
        403,
        &serde_json::json!({ "error": { "code": Refusal::AdminRequired.code(), "message": "The maintenance hold needs the gate's admin token.", "type": "gate_error", "retryable": false } }),
    )
}

fn hold_unknown() -> Response {
    respond::json(
        404,
        &serde_json::json!({ "error": { "code": Refusal::HoldUnknown.code(), "message": Refusal::HoldUnknown.message(), "type": "gate_error", "retryable": false } }),
    )
}

/// The reason as kept: printable text, at most [`MAX_REASON_CHARS`] characters. `None`: not a string.
fn reason_of(value: &serde_json::Value) -> Option<Option<String>> {
    match value.get("reason") {
        None | Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(text)) => Some(Some(
            text.chars()
                .filter(|c| !c.is_control())
                .take(MAX_REASON_CHARS)
                .collect(),
        )),
        Some(_) => None,
    }
}

/// An optional JSON body of a maintenance request (`{}` when empty).
async fn body_of(gate: &Gate, request: Request) -> Result<serde_json::Value, Refusal> {
    let body = match tokio::time::timeout(
        gate.cfg.body_timeout,
        http_body_util::Limited::new(request.into_body(), LEASE_BODY_LIMIT).collect(),
    )
    .await
    {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(_)) => return Err(Refusal::BodyTooLarge),
        Err(_) => return Err(Refusal::BodyTimeout),
    };
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(Refusal::InvalidJson),
    }
}

/// `POST /gate/v1/maintenance` `{reason?, wait_ms?, idle_ttl_ms?}` (admin): takes the
/// maintenance hold. `201 {hold, idle_ttl_ms}`; `409 engine_busy` while work runs and `503
/// maintenance_held` while another hold is held, each after up to `wait_ms` of waiting for it to
/// end. A holder that leaves while it waits leaves nothing behind; one that leaves the moment it
/// is granted leaves a hold that expires after its idle limit.
async fn take_hold(gate: Arc<Gate>, request: Request) -> Response {
    if !gate.admin(request.headers()) {
        log(
            "gate.maintenance_refused",
            &[("code", "admin_required".into())],
        );
        return admin_required();
    }
    let value = match body_of(&gate, request).await {
        Ok(value) => value,
        Err(refusal) => return respond::refusal(refusal),
    };
    let Some(reason) = reason_of(&value) else {
        return respond::refusal(Refusal::InvalidJson);
    };
    let wait_ms = match value.get("wait_ms") {
        None => 0,
        Some(v) => match v.as_u64().filter(|v| *v <= MAX_HOLD_WAIT_MS) {
            Some(v) => v,
            None => return respond::refusal(Refusal::InvalidWait),
        },
    };
    let default_idle = u64::try_from(gate.cfg.maintenance_idle.as_millis()).unwrap_or(u64::MAX);
    let idle_ms = match value.get("idle_ttl_ms") {
        None => default_idle,
        Some(v) => match v.as_u64().filter(|v| HOLD_IDLE_RANGE_MS.contains(v)) {
            Some(v) => v,
            None => return respond::refusal(Refusal::InvalidWait),
        },
    };
    let reason = reason.unwrap_or_default();
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(wait_ms);
    loop {
        let taken = gate.with(|inner, now| inner.sched.begin_hold(now, idle_ms, &reason));
        match taken {
            Ok(key) => {
                let Some(token) = issue_hold_token(&gate, key) else {
                    gate.with(|inner, _| {
                        inner.sched.end_hold(key);
                    });
                    return respond::refusal(Refusal::GateBusy);
                };
                let waited = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                log(
                    "gate.maintenance_hold",
                    &[
                        ("waited_ms", waited.into()),
                        ("idle_ttl_ms", idle_ms.into()),
                    ],
                );
                return respond::json(
                    201,
                    &serde_json::json!({ "hold": token, "idle_ttl_ms": idle_ms }),
                );
            }
            Err(Refusal::EngineBusy | Refusal::MaintenanceHeld)
                if tokio::time::Instant::now() < deadline =>
            {
                // Wait for the work in flight (or another hold) to end; a tick is short against
                // any hold's purpose.
                let nap = (deadline - tokio::time::Instant::now())
                    .min(gate.cfg.tick.max(Duration::from_millis(10)));
                tokio::time::sleep(nap).await;
            }
            Err(refusal) => {
                log(
                    "gate.maintenance_refused",
                    &[("code", refusal.code().into())],
                );
                return respond::refusal(refusal);
            }
        }
    }
}

fn issue_hold_token(gate: &Arc<Gate>, key: HoldKey) -> Option<String> {
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).ok()?;
    let token: String = raw.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    gate.lock().hold = Some((token_hash(&token), key));
    Some(token)
}

/// The live hold `token` names, if any.
fn hold_named(gate: &Arc<Gate>, token: &str) -> Option<HoldKey> {
    if !valid_token(token) {
        return None;
    }
    let hash = token_hash(token);
    let now = gate.now();
    let inner = gate.lock();
    match inner.hold {
        Some((held, key)) if held == hash && inner.sched.hold_live(now, key) => Some(key),
        _ => None,
    }
}

/// `POST /gate/v1/maintenance/{hold}/renew` `{reason?}` (admin): the holder is still at work.
/// `200 {idle_ttl_ms}`; `404 maintenance_hold_unknown` when it has ended.
async fn renew_hold(gate: Arc<Gate>, request: Request, token: &str) -> Response {
    if !gate.admin(request.headers()) {
        return admin_required();
    }
    let value = match body_of(&gate, request).await {
        Ok(value) => value,
        Err(refusal) => return respond::refusal(refusal),
    };
    let Some(reason) = reason_of(&value) else {
        return respond::refusal(Refusal::InvalidJson);
    };
    let Some(key) = hold_named(&gate, token) else {
        return hold_unknown();
    };
    let renewed = gate.with(|inner, now| {
        inner
            .sched
            .renew_hold(now, key, reason.as_deref())
            .then(|| inner.sched.hold_status(now).map(|h| h.idle_ttl_ms))
            .flatten()
    });
    match renewed {
        Some(idle_ttl_ms) => respond::json(200, &serde_json::json!({ "idle_ttl_ms": idle_ttl_ms })),
        None => hold_unknown(),
    }
}

/// `DELETE /gate/v1/maintenance/{hold}` (admin): the holder gives the hold back. `204`; `404
/// maintenance_hold_unknown` when it had already ended.
fn end_hold(gate: &Arc<Gate>, headers: &axum::http::HeaderMap, token: &str) -> Response {
    if !gate.admin(headers) {
        return admin_required();
    }
    let Some(key) = hold_named(gate, token) else {
        return hold_unknown();
    };
    let ended = gate.with(|inner, _| {
        let ended = inner.sched.end_hold(key);
        if ended {
            inner.hold = None;
        }
        ended
    });
    if !ended {
        return hold_unknown();
    }
    log("gate.maintenance_end", &[("end", "released".into())]);
    gate.hold_ended.notify_waiters();
    gate.kick();
    respond::empty(StatusCode::NO_CONTENT)
}

fn issue_token(gate: &Arc<Gate>, lease: LeaseKey) -> String {
    let mut raw = [0u8; 16];
    if getrandom::getrandom(&mut raw).is_err() {
        // No randomness: the lease cannot be handed out safely.
        gate.with(|inner, _| {
            inner.sched.release_lease(lease);
        });
        return String::new();
    }
    let token: String = raw.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    let hash = token_hash(&token);
    let mut inner = gate.lock();
    inner.lease_by_hash.insert(hash, lease);
    inner.hash_by_lease.insert(lease, hash);
    token
}

fn granted_event(gate: &Gate, model: &str, token: &str, waited_ms: u64) -> Bytes {
    if token.is_empty() {
        return error_event(Refusal::GateBusy);
    }
    event(
        "granted",
        &serde_json::json!({
            "lease": token,
            "model": model,
            "waited_ms": waited_ms,
            "idle_ttl_ms": gate.cfg.admission.lease_idle_ms,
            "hard_ttl_ms": gate.cfg.admission.lease_hard_ms,
        }),
    )
}

fn error_event(refusal: Refusal) -> Bytes {
    event(
        "error",
        &serde_json::json!({
            "code": refusal.code(),
            "message": refusal.message(),
            "retryable": refusal.retryable(),
        }),
    )
}

fn event(name: &str, data: &serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {data}\n\n"))
}

/// Gives a lease back unless its grant was handed to the client's connection.
struct Undelivered {
    gate: Arc<Gate>,
    lease: Option<LeaseKey>,
}

impl Drop for Undelivered {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.gate.with(|inner, _| {
                inner.sched.release_lease(lease);
                forget_lease(inner, lease);
            });
            log("gate.lease_end", &[("end", "undelivered".into())]);
        }
    }
}

/// The event stream of a lease granted at once. If the client's connection is gone before the
/// grant is written to it, the lease is given back at once instead of idling out.
fn granted_now(gate: &Arc<Gate>, lease: LeaseKey, granted: Bytes) -> Response {
    let pending = Undelivered {
        gate: Arc::clone(gate),
        lease: Some(lease),
    };
    let stream = futures_util::stream::unfold(Some((pending, granted)), |state| async move {
        let (mut pending, granted) = state?;
        pending.lease = None;
        Some((Ok::<Bytes, std::io::Error>(granted), None))
    });
    with_sse_headers(Response::new(Body::from_stream(stream)))
}

fn stream_response(events: mpsc::Receiver<Bytes>) -> Response {
    let stream = futures_util::stream::unfold(events, |mut events| async move {
        events
            .recv()
            .await
            .map(|chunk| (Ok::<Bytes, std::io::Error>(chunk), events))
    });
    with_sse_headers(Response::new(Body::from_stream(stream)))
}

fn with_sse_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

fn metrics(gate: &Arc<Gate>) -> Response {
    let now = gate.now();
    let inner = gate.lock();
    let status = inner.sched.status(now);
    let c = &status.counters;
    let mut out = String::new();
    let mut line = |name: &str, value: u64| {
        let _ = writeln!(out, "noevia_gate_{name} {value}");
    };
    line("arrivals_total", c.arrivals);
    line("admitted_total", c.admitted);
    line("queued_total", c.queued);
    line("granted_total", c.granted);
    line("switches_total", c.switches);
    line("unloads_refused_total", c.unloads_refused);
    line("unloads_forced_total", c.unloads_forced);
    line("leases_granted_total", c.leases_granted);
    line("leases_ended_total", c.leases_ended);
    line("wait_ms_sum", c.wait_total_ms);
    line("wait_ms_count", c.wait_count);
    line("wait_ms_max", c.wait_max_ms);
    line("leases", status.leases as u64);
    line(
        "queue_waiters",
        status.queue.iter().map(|g| g.waiters as u64).sum(),
    );
    line("router_up", u64::from(status.router_up));
    for (code, count) in c.rejected.iter().chain(inner.gate_refusals.iter()) {
        let _ = writeln!(out, "noevia_gate_refusals_total{{code=\"{code}\"}} {count}");
    }
    for (model, count) in &status.in_flight {
        let _ = writeln!(
            out,
            "noevia_gate_in_flight{{model=\"{}\"}} {count}",
            escape_label(model)
        );
    }
    let mut response = Response::new(Body::from(out));
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    response
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
