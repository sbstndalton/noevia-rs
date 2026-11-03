//! GET-only idle timer. Node owns the live reload baseline and the write workflow.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use serde_json::json;
use std::{sync::Arc, time::Duration};

pub const PATH: &str = "/api/models/idle-unload";
const FACTS: &str = "/internal/idle-unload-read/facts";
const UNAVAILABLE: &str = "idleUnloadUnavailable";
const ENGINE: &str = "idleUnloadEngine";

fn error(status: StatusCode, message: &str, id: &str) -> Response<Body> {
    reply::json(status, &json!({"error": message, "errorId": id}), true)
}

fn seconds(value: &JValue) -> Option<f64> {
    match value {
        JValue::Num(n) if n.is_finite() && *n >= 0.0 && n.fract() == 0.0 => Some(*n),
        _ => None,
    }
}

fn source(value: &JValue, allowed: &[&str]) -> JValue {
    match value.as_str() {
        Some(s) if allowed.contains(&s) => value.clone(),
        _ => JValue::Null,
    }
}

async fn effective_seconds(app: &App) -> Option<f64> {
    // Node's current router listing has an 8s timeout. Allow for its loopback transport.
    let (status, bytes) = app
        .config
        .research
        .node_call(FACTS, &json!({}), Duration::from_secs(10))
        .await?;
    if status != 200 || bytes.len() > 256 {
        return None;
    }
    let facts = wtf16::parse(std::str::from_utf8(&bytes).ok()?).ok()?;
    seconds(facts.get("seconds"))
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path != PATH || req.method() != Method::GET {
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
        return reply::error(
            StatusCode::FORBIDDEN,
            "Administrator required for shared model profiles",
            true,
        );
    }
    let loader = &app.config.model_manager_scan;
    if !loader.configured() {
        return error(
            StatusCode::NOT_FOUND,
            "The idle unload setting is unavailable",
            UNAVAILABLE,
        );
    }
    let body = match loader.idle_unload().await {
        Some((StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED, _)) => {
            return error(
                StatusCode::NOT_FOUND,
                "This model manager cannot change the idle unload time yet.",
                UNAVAILABLE,
            );
        }
        Some((StatusCode::BAD_REQUEST, _)) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Choose Never or a time from 5 minutes to 24 hours.",
                "idleUnloadInvalid",
            );
        }
        Some((StatusCode::SERVICE_UNAVAILABLE, _)) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The model manager has no place to keep this setting.",
                UNAVAILABLE,
            );
        }
        Some((status, Some(body))) if status.is_success() => body,
        _ => {
            return error(
                StatusCode::BAD_GATEWAY,
                "The model manager did not answer. Try again in a moment.",
                ENGINE,
            )
        }
    };
    let saved = seconds(body.get("seconds"));
    let origin = source(body.get("source"), &["setting", "env", "default"]);
    let Some(saved) = saved.filter(|_| !matches!(origin, JValue::Null)) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "The model manager gave an unexpected answer.",
            ENGINE,
        );
    };
    let applied = effective_seconds(app).await == Some(saved);
    let view = JValue::obj([
        ("seconds", JValue::Num(saved)),
        ("source", origin),
        (
            "fallbackSeconds",
            seconds(body.get("fallbackSeconds")).map_or(JValue::Null, JValue::Num),
        ),
        (
            "fallbackSource",
            source(body.get("fallbackSource"), &["env", "default"]),
        ),
        (
            "defaultSeconds",
            JValue::Num(seconds(body.get("defaultSeconds")).unwrap_or(1800.0)),
        ),
        (
            "minSeconds",
            JValue::Num(seconds(body.get("minSeconds")).unwrap_or(300.0)),
        ),
        (
            "maxSeconds",
            JValue::Num(seconds(body.get("maxSeconds")).unwrap_or(86400.0)),
        ),
        ("applied", JValue::Bool(applied)),
    ]);
    match wtf16::stringify(&view) {
        Some(text) => reply::json_text(StatusCode::OK, text, true),
        None => error(
            StatusCode::BAD_GATEWAY,
            "The model manager gave an unexpected answer.",
            ENGINE,
        ),
    }
}
