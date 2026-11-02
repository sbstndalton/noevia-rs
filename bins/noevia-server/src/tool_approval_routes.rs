//! Native public approval decisions. Node owns only pending chat Promise resolution (#2282).
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use serde_json::{json, Value};
use server_account::tool_approvals as policy;
use std::{sync::Arc, time::Duration};
const UNAVAILABLE: &str = "Tool approval state is unavailable right now. Try again shortly.";
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
async fn bridge(app: &App, path: &str, body: &Value) -> Option<Value> {
    let (status, bytes) = app
        .config
        .research
        .node_call(path, body, Duration::from_secs(5))
        .await?;
    if status != 200 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
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
    let Some(facts) = bridge(
        app,
        "/internal/tool-approvals/facts",
        &json!({"user":user,"id":id}),
    )
    .await
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    };
    if facts.get("found") == Some(&Value::Bool(false)) {
        return error(StatusCode::NOT_FOUND, policy::MISSING);
    }
    let Some(kind) = facts
        .get("kind")
        .and_then(Value::as_str)
        .filter(|k| ["write", "route"].contains(k))
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    };
    let Some(generation) = facts
        .get("generation")
        .and_then(Value::as_str)
        .filter(|g| g.len() == 32 && g.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    };
    if facts.get("found") != Some(&Value::Bool(true)) {
        return error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE);
    }
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
    // A client disconnect cannot interrupt a half-submitted resolution. The bridge atomically rechecks
    // the active account, owner and pending-entry generation before calling the synchronous resolver.
    let app = Arc::clone(app);
    let input = json!({"user":user,"id":id,"kind":kind,"generation":generation,"decision":decision.value,"remember":decision.remember});
    let kind = kind.to_owned();
    tokio::spawn(async move {
        match bridge(&app, "/internal/tool-approvals/resolve", &input).await {
            Some(v) if v.get("found") == Some(&Value::Bool(false)) => {
                error(StatusCode::NOT_FOUND, policy::MISSING)
            }
            Some(v)
                if v.get("found") == Some(&Value::Bool(true))
                    && v.get("accepted") == Some(&Value::Bool(true)) =>
            {
                reply::json_text(StatusCode::OK, "{\"ok\":true}".into(), true)
            }
            Some(v)
                if v.get("found") == Some(&Value::Bool(true))
                    && v.get("accepted") == Some(&Value::Bool(false)) =>
            {
                error(
                    StatusCode::BAD_REQUEST,
                    if kind == "route" {
                        policy::ROUTE_CHOICES
                    } else {
                        policy::WRITE_CHOICES
                    },
                )
            }
            _ => error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
        }
    })
    .await
    .unwrap_or_else(|_| error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE))
}
