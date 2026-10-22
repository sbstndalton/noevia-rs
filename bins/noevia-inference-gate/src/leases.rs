//! The gate's own endpoints: health, status, metrics, and leases.

use crate::respond;
use crate::{forget_lease, log, token_hash, valid_token, Gate, Outcome, WaitGuard};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use http_body_util::BodyExt;
use inference_admission::{Admission, Arrival, Grant, LeaseKey, Refusal, WaitKind};
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
            respond::json(
                200,
                &serde_json::json!({
                    "scheduler": status,
                    "gate_refusals": inner.gate_refusals,
                    "buffer_free_kib": gate.buffer.available_permits(),
                }),
            )
        }
        (Method::GET, "/gate/v1/metrics") => metrics(&gate),
        (Method::POST, "/gate/v1/leases") => acquire(gate, request).await,
        (Method::DELETE, p) if p.starts_with("/gate/v1/leases/") => {
            release(&gate, p.trim_start_matches("/gate/v1/leases/"))
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
    let (arrival, rx) = gate.with(|inner, now| {
        let arrival = inner
            .sched
            .arrive(now, &model, WaitKind::Lease, None, max_wait);
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
        Arrival::Refused(refusal) => respond::refusal(refusal),
        Arrival::Admitted(Admission::Request(_)) => respond::refusal(Refusal::InvalidLease),
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
            sse(vec![granted_event(&gate, &model, &token, 0)])
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

fn sse(events: Vec<Bytes>) -> Response {
    let stream = futures_util::stream::iter(events.into_iter().map(Ok::<Bytes, std::io::Error>));
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
