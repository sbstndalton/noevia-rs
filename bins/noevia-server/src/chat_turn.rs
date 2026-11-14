//! `POST /api/chat` (chat port S5, capability `rust-chat-turn`; its edge since S6, `rust-chat-edge`; its
//! event stream since S7, `rust-chat-wire`; its admission since S8, `rust-chat-admission`):
//! the front owns the route, answers everything routes/chat.cjs answered before the loop
//! (chat_edge.rs), and relays a turn that passes to Core's loopback `POST /internal/chat/turn` (core
//! rust-chat-turn.cjs), which runs the rest of the unchanged pipeline. Core refuses a POST /api/chat
//! that did not come through here.
//!
//! In order:
//! 1. Node's request gate, as identity.rs answers it: 401 signed out, 403 `invalid CSRF token`, 403
//!    `browser_session_required`.
//!    Node's inference gate answered before these, so the front first takes the gate
//!    (chat_admission.rs) and gives it straight back: when the gate refuses, its 503 (`Retry-After: 3`
//!    and the retryable body) is the answer, as Node gave it; otherwise the refusal.
//! 2. Node's inference gate, taken for this call with an admission lease (chat_admission.rs): its 503,
//!    or the lease held while the rest is decided and until the answer has ended.
//! 3. The edge (chat_edge.rs), the gate held: the shared `llm:` budget (429), counted only now, the
//!    body cap (413), JSON and the object (400), the request shape (400), Cowork (403/409) and the
//!    Diary add-on (404).
//! 4. The request, its body as read, to `/internal/chat/turn` (a Cowork start to
//!    `/internal/chat/cowork`) with the per-start secret, the signed-in user and the admission lease,
//!    which covers Core's turn so that it does not enter the gate again (legacy_proxy.rs `relay`): the
//!    client's headers and credentials ride along, Core authenticates them again and must find the
//!    same user.
//! 5. Core's answer: JSON and the 503 with `Retry-After` byte for byte; a turn's events, which Core
//!    writes as NDJSON for the front since chat port S7, as the event stream the front writes itself
//!    (chat_wire.rs: the headers, one frame per event, the `: keep-alive` comment every 5 s, the end).
//!    The client leaving drops the response, which closes Core's socket (the turn stops and keeps its
//!    interrupted reply, #679).
//!
//! When Core fails before its headers the answer is the proxy's 502 JSON, and so is an event stream
//! Core wrote itself (a Core that is not this front's pair). When the events break after the headers,
//! the client gets the frame index.cjs writes for a handler that throws mid-stream, and the end:
//! `data: {"type":"error","text":"The request could not be completed. Please retry."}`, never half
//! a frame.
use crate::{chat_admission, chat_edge, chat_wire, identity, reply, serve::Conn, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;

pub const PATH: &str = "/api/chat";
/// Core rust-chat-turn.cjs `TURN_PATH`.
pub const TURN_PATH: &str = "/internal/chat/turn";
/// Core rust-chat-turn.cjs `USER_HEADER`: the user the front authenticated.
pub const USER_HEADER: &str = "x-noevia-chat-user";
/// core index.cjs `STORED_HISTORY_BYTES`: routes/chat.cjs refuses a longer body with its 413.
pub const STORED_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Core reads one byte past its cap before it answers that 413.
pub const RELAY_LIMIT: u64 = STORED_HISTORY_BYTES + 1;
/// index.cjs handleRequestScoped's answer once the headers are out (chat_wire.rs writes it).
pub use chat_wire::FAILED_FRAME;
/// The proxy's 502 (legacy_proxy.rs) for a Core that does not answer.
pub(crate) const UNAVAILABLE: &str = "The server is not answering. Please retry.";

fn unavailable() -> Response<Body> {
    reply::error(StatusCode::BAD_GATEWAY, UNAVAILABLE, true)
}

fn sensitive(value: &str) -> Option<HeaderValue> {
    let mut v = HeaderValue::from_str(value).ok()?;
    v.set_sensitive(true);
    Some(v)
}

/// One POST to exactly [`PATH`] (WHATWG pathname `path`).
pub async fn serve(app: &Arc<App>, conn: Conn, req: Request<Body>, path: String) -> Response<Body> {
    // An assembled native backend has no Node to relay to: its own executor answers, as before.
    if app.proxy.is_in_process() {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (mut parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gated = tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, now)).await;
    let authn = match gated {
        Ok(Ok(id)) => id,
        Ok(Err(rejection @ identity::Rejection::Refused(_))) => {
            return refused(app, chat_admission::Gate::Chat, rejection.into_response()).await;
        }
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let token = app.config.research.front_token().and_then(sensitive);
    let (Some(token), Some(user)) = (token, sensitive(&authn.user.id)) else {
        return unavailable();
    };
    // Node's inference gate first: a call is counted only once the gate let it in.
    let lease = match chat_admission::admit(app, chat_admission::Gate::Chat).await {
        Ok(lease) => lease,
        Err(answer) => return *answer,
    };
    let Some(lease_id) = chat_admission::header_value(&lease) else {
        return unavailable();
    };
    // The edge (chat_edge.rs), in routes/chat.cjs's order, the gate held: the budget before the body.
    let Some(limited) = chat_edge::charge(app, &authn.user.id) else {
        return unavailable();
    };
    if limited {
        return reply::error(
            StatusCode::TOO_MANY_REQUESTS,
            chat_edge::CHAT_TOO_MANY,
            true,
        );
    }
    // Review P2 (S5–S9): every await under the lease watches for its loss, the client's body
    // included: a stalled upload must not keep a lost lease's request (and its body) alive.
    let reading = crate::account_routes::read_body(body, STORED_HISTORY_BYTES as usize);
    let Some(read) = chat_admission::until_lost(&lease, reading).await else {
        return chat_admission::lost_answer(chat_admission::Gate::Chat);
    };
    let bytes = match read {
        Ok(b) if !b.over => b.bytes,
        failed => match chat_edge::unread(failed.is_ok()) {
            chat_edge::Verdict::Refuse(status, text) => {
                return reply::json_text(status, text, true)
            }
            _ => return unavailable(),
        },
    };
    let account = chat_edge::Account {
        admin: authn.user.role == server_auth::Role::Admin,
        diary: authn.user.diary_enabled,
    };
    // The harness flag is read only for a Cowork request that got that far.
    let verdict = match chat_edge::check(&bytes, account, || None) {
        chat_edge::Verdict::Cowork => {
            let a = Arc::clone(app);
            let reading = tokio::task::spawn_blocking(move || chat_edge::harness_blocking(&a));
            let Some(harness) = chat_admission::until_lost(&lease, reading).await else {
                return chat_admission::lost_answer(chat_admission::Gate::Chat);
            };
            let harness = harness.ok().flatten();
            chat_edge::check(&bytes, account, || harness)
        }
        other => other,
    };
    let target = match verdict {
        chat_edge::Verdict::Turn => TURN_PATH,
        chat_edge::Verdict::Cowork => chat_edge::COWORK_PATH,
        chat_edge::Verdict::Refuse(status, text) => return reply::json_text(status, text, true),
    };
    // Track R8: an account the native executor runs (chat_native.rs; NOEVIA_CHAT_EXECUTOR, off by default).
    let native = target == TURN_PATH && crate::chat_native::wanted(app, &authn.user.id);
    // Track R8 P7a: the request body again, for the native preparation in shadow (logs only).
    let shadow = (native && crate::chat_prepare_shadow::wanted(app, &authn.user.id))
        .then(|| bytes::Bytes::from(bytes.clone()));
    crate::chat_native::strip_client_header(&mut parts.headers);
    parts.headers.remove(header::TRANSFER_ENCODING);
    // The wire is the front's to ask for: a turn gets NDJSON (chat_wire.rs), anything else nothing.
    parts.headers.remove(chat_wire::WIRE_HEADER);
    // Only this front names a lease.
    parts.headers.remove(chat_admission::LEASE_HEADER);
    if target == TURN_PATH {
        parts.headers.insert(
            chat_wire::WIRE_HEADER,
            HeaderValue::from_static(chat_wire::WIRE),
        );
    }
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    let req = Request::from_parts(parts, Body::from(bytes));
    let extra = [
        (crate::project_store::TOKEN_HEADER, token),
        (USER_HEADER, user),
        (chat_admission::LEASE_HEADER, lease_id),
    ];
    // Review P2-1: a lease lost while Core works (Node ended it, or stopped hearing its renewals)
    // stops the call: Core no longer counts it, so maintenance may begin.
    // Track R8: the native executor's answer is core's NDJSON up to its hand-off, then the native turn's
    // own lines (chat_native.rs), so everything below (the wire, the lease) is the same for both.
    let relaying = async {
        if native {
            let user_id = authn.user.id.clone();
            crate::chat_native::serve(app, user_id, shadow, |handoff| async move {
                let mut extra = extra.to_vec();
                extra.extend(crate::chat_native::handoff_headers(handoff));
                app.proxy
                    .relay(conn.peer, req, target, &extra, RELAY_LIMIT)
                    .await
            })
            .await
        } else {
            app.proxy
                .relay(conn.peer, req, target, &extra, RELAY_LIMIT)
                .await
        }
    };
    let Some(res) = chat_admission::until_lost(&lease, relaying).await else {
        return chat_admission::lost_answer(chat_admission::Gate::Chat);
    };
    let Some(res) = res else {
        return unavailable();
    };
    // Chat port S7 (chat_wire.rs): the turn's events come as NDJSON and the front writes the event
    // stream. A Core that writes its own event stream is not this front's pair: its answer is dropped
    // (which stops that turn) and the client gets the 502 of a Core that did not answer.
    // The lease is the call's one gate count until its answer has ended (chat_admission.rs).
    if chat_wire::is_ndjson(res.headers()) {
        // The headers wait for Core's first event; a loss before it is the gate's 503.
        let Some((stream, completed)) =
            chat_admission::until_lost(&lease, chat_wire::stream_with_completion(res)).await
        else {
            return chat_admission::lost_answer(chat_admission::Gate::Chat);
        };
        // #2494: a loss after the turn's `done` ends the body cleanly, with no failure event.
        chat_admission::hold_until_end(
            stream,
            lease,
            chat_admission::OnLoss::FrameUnless(FAILED_FRAME, completed),
        )
    } else if chat_wire::is_event_stream(res.headers()) {
        unavailable()
    } else {
        chat_admission::hold_until_end(res, lease, chat_admission::OnLoss::Abort)
    }
}

/// A refusal of the session gate's, after Core's inference gate (taken for `gate` and given straight
/// back): the gate's 503 when it refuses, else `answer` (also when Node does not answer).
pub(crate) async fn refused(
    app: &Arc<App>,
    gate: chat_admission::Gate,
    answer: Response<Body>,
) -> Response<Body> {
    match chat_admission::admit(app, gate).await {
        Ok(lease) => {
            lease.release().await;
            answer
        }
        Err(gate_answer) if gate_answer.status() == StatusCode::SERVICE_UNAVAILABLE => *gate_answer,
        Err(_) => answer,
    }
}
