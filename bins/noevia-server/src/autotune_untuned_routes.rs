//! #2295: public discovery and the runner share autotune-plan's pure scan policy.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/autotune/untuned";
const FACTS: &str = "/internal/autotune-untuned/facts";
fn error(status: StatusCode, message: &str) -> Response<Body> {
    reply::error(status, message, true)
}
fn unavailable() -> Response<Body> {
    error(StatusCode::BAD_GATEWAY, "Could not list untuned models.")
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(
            StatusCode::FORBIDDEN,
            if parts.method == Method::GET || parts.method == Method::HEAD {
                "Administrator required for shared model profiles"
            } else {
                "administrator required"
            },
        );
    }
    // Availability alone precedes method; wrong methods must never scan identities/router rows.
    let Some((status, bytes)) = app
        .config
        .research
        .node_call(
            FACTS,
            &serde_json::json!({"scan":false}),
            Duration::from_secs(5),
        )
        .await
    else {
        return unavailable();
    };
    if status != 200 || bytes.len() > 8192 {
        return unavailable();
    }
    let Some(available) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| wtf16::parse(s).ok())
    else {
        return unavailable();
    };
    match available.get("available").as_bool() {
        Some(false) => return error(StatusCode::NOT_FOUND, "Auto-tune is unavailable"),
        Some(true) => {}
        None => return unavailable(),
    }
    if parts.method != Method::GET {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    let Some((status, bytes)) = app
        .config
        .research
        .node_call(
            FACTS,
            &serde_json::json!({"scan":true}),
            Duration::from_secs(5),
        )
        .await
    else {
        return unavailable();
    };
    if bytes.len() > autotune_plan::scan::MAX_BYTES {
        return unavailable();
    }
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| wtf16::parse(s).ok())
    else {
        return unavailable();
    };
    if status != 200 {
        return error(
            StatusCode::BAD_GATEWAY,
            match facts.get("failure").as_str() {
                Some("router") => "The model server is not responding.",
                Some("identity") => "Model identity could not be read.",
                _ => "Could not list untuned models.",
            },
        );
    }
    if facts.get("available").as_bool() != Some(true) {
        return unavailable();
    }
    let Some(input) = wtf16::stringify(&JValue::obj([
        ("op", JValue::Str("scan".into())),
        ("version", facts.get("version").clone()),
        ("rows", facts.get("rows").clone()),
    ])) else {
        return unavailable();
    };
    let (status, text) = autotune_plan::scan::run_json(&input);
    if status != 0 {
        return unavailable();
    }
    reply::json_text(StatusCode::OK, text, true)
}
