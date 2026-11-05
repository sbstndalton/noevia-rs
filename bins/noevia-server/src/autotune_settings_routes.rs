//! #2287: per-model settings policy and response are native; only tune actor facts and its
//! synchronous conditional map update remain in Node, the one writer of the live tune state.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use js_json::{wtf16, JValue};
use server_projects::autotune_settings::{self as policy, Fault};
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/autotune/settings";
const FACTS: &str = "settings/facts";
const COMMIT: &str = "settings/commit";
const BODY_DEADLINE: Duration = Duration::from_secs(120);
const MAX_BODY: usize = 1024 * 1024;
fn error(status: u16, message: &str) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        message,
        true,
    )
}
fn fault(f: Fault) -> Response<Body> {
    no_store(error(f.status(), f.message()))
}
fn no_store(mut res: Response<Body>) -> Response<Body> {
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}
async fn call(app: &App, path: &str, body: &serde_json::Value) -> Option<(u16, JValue)> {
    app.config.autotune.call(path, body).await
}
fn available(answer: &JValue, settings: bool) -> Result<(), Box<Response<Body>>> {
    match answer.get("available").as_bool() {
        Some(false) => return Err(Box::new(error(404, "Auto-tune is unavailable"))),
        Some(true) => {}
        None => return Err(Box::new(fault(Fault::State))),
    }
    if settings {
        match answer.get("settingsAvailable").as_bool() {
            Some(false) => return Err(Box::new(error(404, "Auto-tune settings are unavailable"))),
            Some(true) => {}
            None => return Err(Box::new(fault(Fault::State))),
        }
    }
    Ok(())
}
async fn body(mut body: Body) -> Result<JValue, Box<Response<Body>>> {
    let read = async {
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| Box::new(fault(Fault::Model)))?;
            if let Ok(data) = frame.into_data() {
                if data.len() > MAX_BODY.saturating_sub(bytes.len()) {
                    return Err(Box::new(no_store(error(413, "Request exceeds size limit"))));
                }
                bytes.extend_from_slice(&data);
            }
        }
        if bytes.is_empty() {
            return Ok(JValue::Obj(Vec::new()));
        }
        // Node readBody decodes broken UTF-8 with replacements before JSON.parse.
        wtf16::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| Box::new(fault(Fault::Model)))
    };
    tokio::time::timeout(BODY_DEADLINE, read)
        .await
        .unwrap_or_else(|_| Err(Box::new(no_store(error(408, "Request timed out")))))
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
    let (parts, input) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let gate = Arc::clone(&app.identity);
    let id = match tokio::task::spawn_blocking(move || {
        gate.gate_blocking(&creds, PATH, identity::now_ms())
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejected)) => return rejected.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    if id.user.role != server_auth::Role::Admin {
        return error(403, "Administrator required for shared model profiles");
    }
    let Some((200, readiness)) = call(app, FACTS, &serde_json::json!({})).await else {
        return fault(Fault::State);
    };
    if let Err(res) = available(&readiness, false) {
        return *res;
    }
    if parts.method != Method::POST {
        return error(405, "Method not allowed");
    }
    if let Err(res) = available(&readiness, true) {
        return *res;
    }
    let parsed = match body(input).await {
        Ok(body) => body,
        Err(res) => return *res,
    };
    let change = match policy::parse(&parsed) {
        Ok(change) => change,
        Err(f) => return fault(f),
    };
    // A settings operation never holds a cross-process lock over an await. Each attempt validates
    // fresh facts; Node compares that snapshot again synchronously with its one state-file write.
    for _ in 0..3 {
        let Some((200, answer)) =
            call(app, FACTS, &serde_json::json!({"model": change.model})).await
        else {
            return fault(Fault::State);
        };
        if let Err(res) = available(&answer, true) {
            return *res;
        }
        let facts = answer.get("facts");
        if let Err(f) = policy::validate(&change, facts) {
            return fault(f);
        }
        let Some(token) = facts.get("token").as_str().filter(|t| {
            t.len() == 64
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) else {
            return fault(Fault::State);
        };
        let Some((status, result)) = call(app, COMMIT, &serde_json::json!({"model": change.model, "allowQ5Kv": change.allow_q5, "token": token})).await else { return fault(Fault::State); };
        if status == 409 && result.get("applied").as_bool() == Some(false) {
            continue;
        }
        if status != 200 || result.get("applied").as_bool() != Some(true) {
            return fault(Fault::State);
        }
        let response = match policy::response(&change, result.get("facts")) {
            Ok(body) => body,
            Err(f) => return fault(f),
        };
        let Some(text) = wtf16::stringify(&response) else {
            return fault(Fault::State);
        };
        return no_store(reply::json_text(StatusCode::OK, text, true));
    }
    no_store(error(
        409,
        "Auto-tune state changed. Retry saving this setting.",
    ))
}
