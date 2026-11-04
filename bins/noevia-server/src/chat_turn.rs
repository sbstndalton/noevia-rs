//! `POST /api/chat` (chat port S5, capability `rust-chat-turn`; its edge since S6, `rust-chat-edge`; its
//! event stream since S7, `rust-chat-wire`):
//! the front owns the route, answers everything routes/chat.cjs answered before the loop
//! (chat_edge.rs), and relays a turn that passes to Core's loopback `POST /internal/chat/turn` (core
//! rust-chat-turn.cjs), which runs the rest of the unchanged pipeline. Core refuses a POST /api/chat
//! that did not come through here.
//!
//! In order:
//! 1. Node's request gate, as identity.rs answers it: 401 signed out, 403 `invalid CSRF token`, 403
//!    `browser_session_required`.
//! 2. The edge (chat_edge.rs): the shared `llm:` budget (429), the body cap (413), JSON and the
//!    object (400), the request shape (400), Cowork (403/409) and the Diary add-on (404).
//! 3. Node's inference gate answered before all of these, so every refusal of 1 and 2 is first
//!    offered to Core's `POST /internal/chat/gate`: when the gate is held its 503 (`Retry-After: 3`
//!    and the retryable body) is the answer, as Node gave it, and a call counted on the budget is
//!    given back; otherwise the refusal.
//! 4. The request, its body as read, to `/internal/chat/turn` (a Cowork start to
//!    `/internal/chat/cowork`) with the per-start secret and the signed-in user (legacy_proxy.rs
//!    `relay`): the client's headers and credentials ride along, Core authenticates them again and
//!    must find the same user.
//! 5. Core's answer: JSON and the 503 with `Retry-After` byte for byte; a turn's events, which Core
//!    writes as NDJSON for the front since chat port S7, as the event stream the front writes itself
//!    (chat_wire.rs: the headers, one frame per event, the `: keep-alive` comment every 5 s, the end).
//!    The client leaving drops the response, which closes Core's socket (the turn stops and keeps its
//!    interrupted reply, #679). A turn Core's gate refused gives its budget call back.
//!
//! When Core fails before its headers the answer is the proxy's 502 JSON, and so is an event stream
//! Core wrote itself (a Core that is not this front's pair). When the events break after the headers,
//! the client gets the frame index.cjs writes for a handler that throws mid-stream, and the end:
//! `data: {"type":"error","text":"The request could not be completed. Please retry."}`, never half
//! a frame.
use crate::{chat_edge, chat_wire, identity, reply, serve::Conn, App};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;

pub const PATH: &str = "/api/chat";
/// Core rust-chat-turn.cjs `TURN_PATH` and `GATE_PATH`.
pub const TURN_PATH: &str = "/internal/chat/turn";
pub const GATE_PATH: &str = "/internal/chat/gate";
/// Core rust-chat-turn.cjs `USER_HEADER`: the user the front authenticated.
pub const USER_HEADER: &str = "x-noevia-chat-user";
/// core index.cjs `STORED_HISTORY_BYTES`: routes/chat.cjs refuses a longer body with its 413.
pub const STORED_HISTORY_BYTES: u64 = 32 * 1024 * 1024;
/// Core reads one byte past its cap before it answers that 413.
pub const RELAY_LIMIT: u64 = STORED_HISTORY_BYTES + 1;
/// index.cjs handleRequestScoped's answer once the headers are out (chat_wire.rs writes it).
pub use chat_wire::FAILED_FRAME;
/// The proxy's 502 (legacy_proxy.rs) for a Core that does not answer.
const UNAVAILABLE: &str = "The server is not answering. Please retry.";

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
            return refused(app, conn, &parts.headers, None, rejection.into_response()).await;
        }
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let token = app.config.research.front_token().and_then(sensitive);
    let (Some(token), Some(user)) = (token, sensitive(&authn.user.id)) else {
        return unavailable();
    };
    // The edge (chat_edge.rs), in routes/chat.cjs's order: the budget before the body is read.
    let Some(counted) = chat_edge::charge(app, &authn.user.id) else {
        return unavailable();
    };
    if counted.limited {
        let over = reply::error(
            StatusCode::TOO_MANY_REQUESTS,
            chat_edge::CHAT_TOO_MANY,
            true,
        );
        return refused(app, conn, &parts.headers, Some(&counted), over).await;
    }
    let bytes = match crate::account_routes::read_body(body, STORED_HISTORY_BYTES as usize).await {
        Ok(b) if !b.over => b.bytes,
        failed => {
            let chat_edge::Verdict::Refuse(status, text) = chat_edge::unread(failed.is_ok()) else {
                return unavailable();
            };
            let answer = reply::json_text(status, text, true);
            return refused(app, conn, &parts.headers, Some(&counted), answer).await;
        }
    };
    let account = chat_edge::Account {
        admin: authn.user.role == server_auth::Role::Admin,
        diary: authn.user.diary_enabled,
    };
    // The harness flag is read only for a Cowork request that got that far.
    let verdict = match chat_edge::check(&bytes, account, || None) {
        chat_edge::Verdict::Cowork => {
            let a = Arc::clone(app);
            let harness = tokio::task::spawn_blocking(move || chat_edge::harness_blocking(&a))
                .await
                .ok()
                .flatten();
            chat_edge::check(&bytes, account, || harness)
        }
        other => other,
    };
    let target = match verdict {
        chat_edge::Verdict::Turn => TURN_PATH,
        chat_edge::Verdict::Cowork => chat_edge::COWORK_PATH,
        chat_edge::Verdict::Refuse(status, text) => {
            let answer = reply::json_text(status, text, true);
            return refused(app, conn, &parts.headers, Some(&counted), answer).await;
        }
    };
    parts.headers.remove(header::TRANSFER_ENCODING);
    // The wire is the front's to ask for: a turn gets NDJSON (chat_wire.rs), anything else nothing.
    parts.headers.remove(chat_wire::WIRE_HEADER);
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
    ];
    let Some(mut res) = app
        .proxy
        .relay(conn.peer, req, target, &extra, RELAY_LIMIT)
        .await
    else {
        // Core never saw the call, so Node would not have counted it.
        chat_edge::release(app, &counted);
        return unavailable();
    };
    if chat_edge::take_gate_refused(res.headers_mut()) {
        chat_edge::release(app, &counted);
    }
    // Chat port S7 (chat_wire.rs): the turn's events come as NDJSON and the front writes the event
    // stream. A Core that writes its own event stream is not this front's pair: its answer is dropped
    // (which stops that turn) and the client gets the 502 of a Core that did not answer.
    if chat_wire::is_ndjson(res.headers()) {
        chat_wire::stream(res).await
    } else if chat_wire::is_event_stream(res.headers()) {
        unavailable()
    } else {
        res
    }
}

/// A refusal of the front's, after Core's inference gate: the gate's 503 when it is held (and the
/// budget call given back), else `answer`.
async fn refused(
    app: &Arc<App>,
    conn: Conn,
    headers: &HeaderMap,
    counted: Option<&chat_edge::Counted>,
    answer: Response<Body>,
) -> Response<Body> {
    match gate_probe(app, conn, headers, GATE_PATH).await {
        Some(mut held) => {
            chat_edge::take_gate_refused(held.headers_mut());
            if let Some(c) = counted {
                chat_edge::release(app, c);
            }
            held
        }
        None => answer,
    }
}

/// Core's inference gate for a request the front refuses, at `target` (the chat gate, or the Diary
/// local exchange's): its 503 when the gate is held, else `None` (a 204, or no answer: the front's
/// refusal stands).
pub(crate) async fn gate_probe(
    app: &Arc<App>,
    conn: Conn,
    headers: &HeaderMap,
    target: &str,
) -> Option<Response<Body>> {
    let token = app.config.research.front_token().and_then(sensitive)?;
    let mut probe = Request::post(GATE_PATH).body(Body::empty()).ok()?;
    if let Some(host) = headers.get(header::HOST) {
        probe.headers_mut().insert(header::HOST, host.clone());
    }
    let extra = [(crate::project_store::TOKEN_HEADER, token)];
    let res = app.proxy.relay(conn.peer, probe, target, &extra, 0).await?;
    (res.status() == StatusCode::SERVICE_UNAVAILABLE).then_some(res)
}
