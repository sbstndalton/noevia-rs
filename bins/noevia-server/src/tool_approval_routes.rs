//! Native public approval decisions (#2282), answered from the front's own approval registry
//! (approval_registry.rs, port contract 2026-11-04 S9): no Node bridge.
use crate::approval_registry::DecideError;
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use server_account::tool_approvals as policy;
use std::{sync::Arc, time::Duration};
const UNAVAILABLE: &str = "Tool approval state is unavailable right now. Try again shortly.";
/// GET /api/tool-approvals?chat=<chatId> (#2580): the signed-in account's own open questions in
/// that chat, so another tab of that chat, while the turn that asked still runs, knows a card is
/// waiting. Only ids, kinds and expiry: the card's content stays in the asking tab's
/// stream, and answering still takes POST /api/tool-approvals/{id} with the owner's session.
pub const LIST: &str = "/api/tool-approvals";
/// The longest chat id the read accepts.
const MAX_CHAT: usize = 128;
fn error(status: StatusCode, message: &str) -> Response<Body> {
    reply::error(status, message, true)
}
async fn gate(
    app: &Arc<App>,
    creds: server_auth::Creds,
    path: String,
) -> Result<server_auth::Identity, identity::Rejection> {
    let layer = Arc::clone(&app.identity);
    match tokio::task::spawn_blocking(move || layer.gate_current_blocking(&creds, &path)).await {
        Ok(result) => result,
        Err(_) => Err(identity::Rejection::Unavailable),
    }
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path == LIST && req.method() == Method::GET {
        return list(app, req, path).await;
    }
    let raw = path
        .strip_prefix("/api/tool-approvals/")
        .filter(|s| !s.is_empty() && !s.contains('/'));
    if req.method() != Method::POST || raw.is_none() {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let authn = match gate(app, creds.clone(), path.clone()).await {
        Ok(id) => id,
        Err(r) => return r.into_response(),
    };
    let Some(id) = server_projects::js::decode_uri_component(raw.unwrap_or_default()) else {
        return error(StatusCode::BAD_REQUEST, "invalid approval id");
    };
    let body = match tokio::time::timeout(
        Duration::from_secs(30),
        crate::account_routes::read_body(body, policy::BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(body)) if body.over => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "Request exceeds size limit")
        }
        Ok(Ok(body)) => body.bytes,
        _ => return error(StatusCode::BAD_REQUEST, "invalid JSON"),
    };
    let input = match policy::parse(&body) {
        Ok(d) => d,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    let user = authn.user.id.as_str();
    // The owner's lookup comes before the decision's coercion, as Node's facts call did.
    let kind = match app.approvals.kind(user, &id) {
        Ok(Some(kind)) => kind.name(),
        Ok(None) => return error(StatusCode::NOT_FOUND, policy::MISSING),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    };
    let decision = match policy::decision(&input) {
        Ok(d) => d,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if let Err(e) = decision.validate(kind) {
        return error(StatusCode::BAD_REQUEST, e);
    }
    let fresh = match gate(app, creds, path).await {
        Ok(id) => id,
        Err(r) => return r.into_response(),
    };
    if fresh.user.id != authn.user.id {
        return error(StatusCode::NOT_FOUND, policy::MISSING);
    }
    // One blocking step a client disconnect cannot cut in half: the live grant context is read
    // first (file locks, never under the account lock), then the account is checked live under
    // cowork.db's IMMEDIATE writer lock and the answer is consumed inside that same lock, so a
    // disable or delete cannot interleave with it.
    let app = Arc::clone(app);
    let user = user.to_owned();
    let kind = kind.to_owned();
    let done = tokio::task::spawn_blocking(move || -> Result<(), Decided> {
        let live = if decision.value == "approve_all" {
            app.approvals.grant_context(&user, &id)
        } else {
            None
        };
        let writer = app.writes.writer().map_err(|_| Decided::Unavailable)?;
        writer
            .write(|tx| {
                let active = tx.reader().row(
                    "SELECT id FROM users WHERE id=?1 AND (disabled_at IS NULL OR disabled_at=0)",
                    [&user],
                    |row| row.get::<_, String>(0),
                )?;
                if active.is_none() {
                    return Ok(Err(Decided::Missing));
                }
                Ok(
                    match app.approvals.decide(
                        &user,
                        &id,
                        &decision.value,
                        decision.remember,
                        live.as_ref(),
                    ) {
                        Ok(()) => Ok(()),
                        Err(DecideError::Missing) => Err(Decided::Missing),
                        Err(DecideError::Choice) => Err(Decided::Choice),
                        Err(DecideError::Unavailable) => Err(Decided::Unavailable),
                    },
                )
            })
            .map_err(|_| Decided::Unavailable)?
    })
    .await;
    match done {
        Ok(Ok(())) => reply::json_text(StatusCode::OK, "{\"ok\":true}".into(), true),
        Ok(Err(Decided::Missing)) => error(StatusCode::NOT_FOUND, policy::MISSING),
        Ok(Err(Decided::Choice)) => error(
            StatusCode::BAD_REQUEST,
            if kind == "route" {
                policy::ROUTE_CHOICES
            } else {
                policy::WRITE_CHOICES
            },
        ),
        Ok(Err(Decided::Unavailable)) | Err(_) => {
            error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE)
        }
    }
}

/// [`LIST`]: identity first, then the chat id, then the owner's own open questions. Nothing is
/// logged; the answer is never cached.
async fn list(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, _body) = req.into_parts();
    let authn = match gate(app, identity::creds_of(&parts), path).await {
        Ok(id) => id,
        Err(r) => return r.into_response(),
    };
    let chat = url::form_urlencoded::parse(parts.uri.query().unwrap_or("").as_bytes())
        .find(|(k, _)| k == "chat")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if chat.is_empty() {
        return error(StatusCode::BAD_REQUEST, "chat required");
    }
    if chat.len() > MAX_CHAT || crate::approval_registry::safe_id(&chat) != chat {
        return error(StatusCode::BAD_REQUEST, "invalid chat id");
    }
    let open = match app.approvals.open_in(&authn.user.id, &chat) {
        Ok(open) => open,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
    };
    let pending: Vec<serde_json::Value> = open
        .into_iter()
        .map(|q| {
            serde_json::json!({
                "id": q.id,
                "kind": q.kind.name(),
                "once": q.once,
                "expiresInMs": u64::try_from(q.expires_in.as_millis()).unwrap_or(u64::MAX),
            })
        })
        .collect();
    reply::json(
        StatusCode::OK,
        &serde_json::json!({ "pending": pending }),
        true,
    )
}

enum Decided {
    Missing,
    Choice,
    Unavailable,
}
