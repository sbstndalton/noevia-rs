//! Forwarding to the router: classification, admission, the bounded body read, and the response
//! body that keeps a generation counted in flight until it ends.

use crate::respond;
use crate::{admit_request, lease_from_headers, log, max_wait_from_headers, Gate, InflightGuard};
use axum::body::Body;
use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use inference_admission::{classify, Class, Refusal, UnloadOutcome};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers addressed to the gate itself; never forwarded.
const GATE_HEADERS: &[&str] = &[
    "x-noevia-lease",
    "x-noevia-max-wait-ms",
    "x-noevia-client",
    "x-noevia-gate-admin",
];

enum Outgoing {
    Buffered(Bytes),
    Streamed(Body),
}

pub(crate) async fn proxy(gate: Arc<Gate>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let query = parts.uri.query().map(str::to_owned);
    if !inference_admission::classify::valid_path(&path) {
        return refuse(&gate, Refusal::InvalidPath);
    }
    let buffered = matches!(parts.method, Method::POST | Method::PUT | Method::PATCH);
    let (outgoing, permit) = if buffered {
        match read_body(&gate, body, &parts.headers).await {
            Ok((bytes, permit)) => (Outgoing::Buffered(bytes), permit),
            Err(refusal) => return refuse(&gate, refusal),
        }
    } else {
        (Outgoing::Streamed(body), None)
    };
    let body_bytes = match &outgoing {
        Outgoing::Buffered(bytes) => Some(bytes.as_ref()),
        Outgoing::Streamed(_) => None,
    };
    let body_kib = body_bytes.map_or(0, |b| b.len().div_ceil(gate.buffer_unit));
    let class = classify(parts.method.as_str(), &path, query.as_deref(), body_bytes);
    match class {
        Class::Invalid(refusal) => refuse(&gate, refusal),
        Class::Passthrough => send(&gate, &parts, outgoing, None, permit).await,
        Class::Model {
            model, autoload, ..
        } => {
            let lease = match lease_from_headers(&gate, &parts.headers) {
                Ok(lease) => lease,
                Err(refusal) => return refuse(&gate, refusal),
            };
            let max_wait = match max_wait_from_headers(&gate, &parts.headers) {
                Ok(wait) => wait,
                Err(refusal) => return refuse(&gate, refusal),
            };
            match admit_request(&gate, &model, autoload, lease, max_wait, body_kib).await {
                Ok(Some(mut guard)) => {
                    // A session request: the router maps its conversation id to the model and
                    // may keep generating for a resume after the client leaves.
                    if let Some(conv_id) = conversation_id(&parts.headers) {
                        if let Some(model) = guard.model().map(str::to_owned) {
                            gate.lock().sessions.remember(&conv_id, &model);
                        }
                        guard.set_session(conv_id);
                    }
                    send(&gate, &parts, outgoing, Some(guard), permit).await
                }
                Ok(None) => send(&gate, &parts, outgoing, None, permit).await,
                Err(refusal) => refuse(&gate, refusal),
            }
        }
        Class::Resume { conv_id } => {
            // Counted on the session's model (the holder when the session is not known here),
            // so no switch unloads it while the resumed answer streams.
            let ticket = gate.with(|inner, _| {
                let model = conv_id
                    .as_deref()
                    .and_then(|c| inner.sessions.model(c))
                    .map(str::to_owned);
                inner.sched.attach(model.as_deref())
            });
            let guard = ticket.map(|ticket| {
                let mut guard = InflightGuard::new(Arc::clone(&gate), ticket);
                if let Some(conv_id) = conv_id {
                    guard.set_resume_of(conv_id);
                }
                guard
            });
            send(&gate, &parts, outgoing, guard, permit).await
        }
        Class::Stop { conv_id } => {
            // The router cancels the session's generation: once it accepted the stop, the
            // session no longer holds its model.
            match forward(&gate, &parts, outgoing, permit, gate.cfg.header_timeout).await {
                Ok(upstream) => {
                    if upstream.status().is_success() {
                        gate.end_session(&conv_id, "stopped");
                    }
                    respond_with(upstream, None)
                }
                Err(refusal) => refuse(&gate, refusal),
            }
        }
        Class::Unload { model, force, .. } => {
            let admin = gate.admin(&parts.headers);
            if force && !admin {
                log(
                    "gate.unload_refused",
                    &[
                        ("model", model.as_str().into()),
                        ("code", Refusal::AdminRequired.code().into()),
                    ],
                );
                return refuse(&gate, Refusal::AdminRequired);
            }
            let canonical = gate.lock().sched.canon(&model);
            let allowed = gate.with(|inner, now| inner.sched.admin_unload(now, &model, force));
            if let Err(refusal) = allowed {
                log(
                    "gate.unload_refused",
                    &[
                        ("model", canonical.as_str().into()),
                        ("code", refusal.code().into()),
                        ("client", crate::client_label(&parts.headers).into()),
                    ],
                );
                return respond::refusal(refusal);
            }
            log(
                if force {
                    "gate.force_unload"
                } else {
                    "gate.unload"
                },
                &[
                    ("model", canonical.as_str().into()),
                    ("client", crate::client_label(&parts.headers).into()),
                ],
            );
            // The unload and the report of its answer run on their own task: a client that
            // leaves meanwhile must not leave the scheduler waiting for an answer forever.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task_gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let answer = forward(&task_gate, &parts, outgoing, permit, UNLOAD_TIMEOUT).await;
                let outcome = match &answer {
                    Ok(response) => crate::unload_outcome(response.status().as_u16()),
                    Err(_) => UnloadOutcome::Failed,
                };
                task_gate.with(|inner, now| inner.sched.unload_result(now, &canonical, outcome));
                task_gate.kick();
                let response = match answer {
                    Ok(upstream) => respond_with(upstream, None),
                    Err(refusal) => refuse(&task_gate, refusal),
                };
                let _ = tx.send(response);
            });
            rx.await
                .unwrap_or_else(|_| respond::refusal(Refusal::EngineUnavailable))
        }
        Class::Reload => {
            let force = gate.admin(&parts.headers);
            if let Err(refusal) = gate.with(|inner, now| inner.sched.begin_maintenance(now, force))
            {
                return respond::refusal(refusal);
            }
            log("gate.reload", &[("forced", force.into())]);
            // The reload runs on its own task and holds the maintenance state until the router
            // has answered (bounded), whatever happens to the client: the router unloads changed
            // models during the reload, so nothing new may be admitted meanwhile.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task_gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let maintenance = Maintenance {
                    gate: Arc::clone(&task_gate),
                };
                let timeout = task_gate.cfg.reload_timeout;
                let response = match forward(&task_gate, &parts, outgoing, permit, timeout).await {
                    Ok(upstream) => buffer_response(upstream, timeout).await,
                    Err(refusal) => refuse(&task_gate, refusal),
                };
                drop(maintenance);
                let _ = tx.send(response);
            });
            rx.await
                .unwrap_or_else(|_| respond::refusal(Refusal::EngineUnavailable))
        }
    }
}

/// The longest the router may take to answer an unload.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest reload answer buffered (it is the model listing).
const MAX_RELOAD_ANSWER: usize = 8 * 1024 * 1024;

/// Ends the maintenance state when dropped (the reload answered, failed, or its task ended).
struct Maintenance {
    gate: Arc<Gate>,
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        self.gate.with(|inner, _| inner.sched.end_maintenance());
        self.gate.kick();
    }
}

/// The conversation id the router keys stream sessions on (`x-conversation-id`), if usable.
fn conversation_id(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all("x-conversation-id").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    value
        .to_str()
        .ok()
        .filter(|v| !v.is_empty() && v.len() <= inference_admission::classify::MAX_CONV_ID_BYTES)
        .map(str::to_owned)
}

/// Reads a whole (small) upstream answer, so it can be answered after the state it guards ended.
async fn buffer_response(upstream: axum::http::Response<Incoming>, limit: Duration) -> Response {
    let (up_parts, up_body) = upstream.into_parts();
    let collected = tokio::time::timeout(
        limit,
        http_body_util::Limited::new(up_body, MAX_RELOAD_ANSWER).collect(),
    )
    .await;
    let Ok(Ok(collected)) = collected else {
        return respond::refusal(Refusal::EngineUnavailable);
    };
    let mut response = Response::new(Body::from(collected.to_bytes()));
    *response.status_mut() = up_parts.status;
    let listed = connection_listed(&up_parts.headers);
    for (name, value) in &up_parts.headers {
        if forwardable(name, &listed) && name != "content-length" {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
}

fn refuse(gate: &Arc<Gate>, refusal: Refusal) -> Response {
    gate.count_refusal(refusal);
    respond::refusal(refusal)
}

/// Reads a request body completely, within the size and time limits and the gate-wide buffer.
async fn read_body(
    gate: &Arc<Gate>,
    mut body: Body,
    headers: &HeaderMap,
) -> Result<(Bytes, Option<OwnedSemaphorePermit>), Refusal> {
    let max = gate.cfg.max_body_bytes;
    if let Some(length) = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if length > max as u64 {
            return Err(Refusal::BodyTooLarge);
        }
    }
    let deadline = tokio::time::Instant::now() + gate.cfg.body_timeout;
    let mut buffer = BytesMut::new();
    let mut permit: Option<OwnedSemaphorePermit> = None;
    let mut held: u32 = 0;
    loop {
        let frame = match tokio::time::timeout_at(deadline, body.frame()).await {
            Err(_) => return Err(Refusal::BodyTimeout),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(Refusal::BodyTimeout),
            Ok(Some(Ok(frame))) => frame,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if buffer.len().saturating_add(data.len()) > max {
            return Err(Refusal::BodyTooLarge);
        }
        buffer.extend_from_slice(&data);
        let needed = u32::try_from(buffer.len().div_ceil(gate.buffer_unit)).unwrap_or(u32::MAX);
        if needed > held {
            let wait = deadline.min(tokio::time::Instant::now() + Duration::from_secs(5));
            let more = tokio::time::timeout_at(
                wait,
                Arc::clone(&gate.buffer).acquire_many_owned(needed - held),
            )
            .await;
            match more {
                Ok(Ok(more)) => {
                    match permit.as_mut() {
                        Some(permit) => permit.merge(more),
                        None => permit = Some(more),
                    }
                    held = needed;
                }
                _ => return Err(Refusal::GateBusy),
            }
        }
    }
    Ok((buffer.freeze(), permit))
}

/// Names listed in a `Connection` header are hop-by-hop as well.
fn connection_listed(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

fn forwardable(name: &HeaderName, listed: &[String]) -> bool {
    let name = name.as_str();
    !HOP_BY_HOP.contains(&name) && !listed.iter().any(|l| l == name)
}

async fn send(
    gate: &Arc<Gate>,
    parts: &Parts,
    outgoing: Outgoing,
    guard: Option<InflightGuard>,
    permit: Option<OwnedSemaphorePermit>,
) -> Response {
    match forward(gate, parts, outgoing, permit, gate.cfg.header_timeout).await {
        Ok(upstream) => respond_with(upstream, guard),
        Err(refusal) => refuse(gate, refusal),
    }
}

/// Sends the request to the router; `Err` when no answer came (the router counts as unreachable
/// when the connection itself failed).
async fn forward(
    gate: &Arc<Gate>,
    parts: &Parts,
    outgoing: Outgoing,
    permit: Option<OwnedSemaphorePermit>,
    timeout: Duration,
) -> Result<axum::http::Response<Incoming>, Refusal> {
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut builder = axum::http::Request::builder()
        .method(parts.method.clone())
        .uri(format!("{}{}", gate.cfg.upstream, path_and_query));
    let listed = connection_listed(&parts.headers);
    if let Some(headers) = builder.headers_mut() {
        for (name, value) in &parts.headers {
            // The gate frames the forwarded body itself (its length or chunking), and answers
            // `expect` itself; neither is passed on.
            let skip = !forwardable(name, &listed)
                || GATE_HEADERS.contains(&name.as_str())
                || name == "host"
                || name == "content-length"
                || name == "expect";
            if !skip {
                headers.append(name.clone(), value.clone());
            }
        }
    }
    let body = match outgoing {
        Outgoing::Buffered(bytes) => Body::from(bytes),
        Outgoing::Streamed(body) => body,
    };
    let Ok(request) = builder.body(body) else {
        return Err(Refusal::InvalidPath);
    };
    let answer = tokio::time::timeout(timeout, gate.client.request(request)).await;
    drop(permit);
    match answer {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(err)) => {
            if err.is_connect() {
                gate.with(|inner, now| inner.sched.router_unreachable(now));
                gate.kick();
            }
            Err(Refusal::EngineUnavailable)
        }
        Err(_) => Err(Refusal::EngineUnavailable),
    }
}

/// The router's answer, passed on; `guard` keeps the request counted until its body ends.
fn respond_with(
    upstream: axum::http::Response<Incoming>,
    guard: Option<InflightGuard>,
) -> Response {
    let (up_parts, up_body) = upstream.into_parts();
    let body = match guard {
        Some(guard) => Body::new(GuardedBody {
            inner: up_body,
            guard: Some(guard),
        }),
        None => Body::new(up_body),
    };
    let mut response = Response::new(body);
    *response.status_mut() = up_parts.status;
    let listed = connection_listed(&up_parts.headers);
    for (name, value) in &up_parts.headers {
        if forwardable(name, &listed) {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        return respond::empty(StatusCode::BAD_GATEWAY);
    }
    response
}

/// The router's response body; the request counts as in flight until it ends or is dropped
/// (the client went away, which also closes the router connection and stops the generation).
struct GuardedBody {
    inner: Incoming,
    guard: Option<InflightGuard>,
}

impl HttpBody for GuardedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(None) => {
                if let Some(mut guard) = self.guard.take() {
                    guard.complete();
                }
            }
            Poll::Ready(Some(Err(_))) => {
                self.guard.take();
            }
            _ => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
