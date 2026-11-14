//! GET status. POST start is answered here only by the native runner (#2516, `AUTOTUNE_RUNNER_IMPL=rust`);
//! otherwise it and every other verb stay with Node.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::wtf16;
use server_projects::autotune_read as policy;
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/autotune";
const FACTS: &str = "/internal/autotune-read/facts";

/// Whether `method path` is the auto-tune start's POST while this front runs no native runner:
/// then it goes to Node exactly as before #2516, through the legacy proxy (#2550).
pub fn start_is_nodes(method: &str, path: &str, native_runner: bool) -> bool {
    !native_runner
        && method == "POST"
        && crate::routes::route_for(path).is_some_and(|r| r.path == PATH)
}
fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Auto-tune state is unavailable. Try again shortly.",
        true,
    )
}
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let native_start = req.method() == Method::POST && app.config.autotune.is_some();
    if path != PATH || (req.method() != Method::GET && !native_start) {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, body) = req.into_parts();
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
    // routes/models.cjs ran these for every non-GET of /api/models/* it served (#1461, #2550): the
    // shared model reads are dropped first (whoever asked), then, past the outer gate (its 403 in
    // its own words), the cached folder scan. Best effort: the next read refreshes either way.
    if native_start {
        crate::auto_roles_routes::invalidate_model_reads(app).await;
        if id.user.role != server_auth::Role::Admin {
            return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
        }
        crate::model_manager_scan::clear(app).await;
    }
    if id.user.role != server_auth::Role::Admin {
        return reply::error(
            StatusCode::FORBIDDEN,
            "Administrator required for shared model profiles",
            true,
        );
    }
    if native_start {
        return start(app, body).await;
    }
    let model = crate::model_sizing::model_param(&parts.uri).unwrap_or_default();
    let Some((status, bytes)) = crate::autotune_runner::call(
        app,
        FACTS,
        &serde_json::json!({"keys":policy::keys(&model)}),
        Duration::from_secs(5),
    )
    .await
    else {
        return unavailable();
    };
    if status != 200 || bytes.len() > 8 * 1024 * 1024 {
        return unavailable();
    }
    let Some(facts) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|s| wtf16::parse(s).ok())
    else {
        return unavailable();
    };
    match facts.get("available").as_bool() {
        Some(false) => {
            return reply::error(StatusCode::NOT_FOUND, "Auto-tune is unavailable", true)
        }
        Some(true) => {}
        None => return unavailable(),
    }
    let Ok(body) = policy::response(&model, &facts) else {
        return unavailable();
    };
    let Some(body) = wtf16::stringify(&body) else {
        return unavailable();
    };
    let mut answer = reply::json_text(StatusCode::OK, body, true);
    answer
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    answer
}

/// routes/models.cjs `POST /api/models/autotune` with the native runner: the body (`readJson`, an
/// empty one is `{}`), then the start, answered `no-store`.
async fn start(app: &Arc<App>, body: Body) -> Response<Body> {
    let Some(native) = app.config.autotune.as_ref() else {
        return reply::error(StatusCode::NOT_FOUND, "Auto-tune is unavailable", true);
    };
    let bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return reply::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds size limit",
                true,
            )
        }
    };
    let ask = if bytes.is_empty() {
        js_json::JValue::Obj(Vec::new())
    } else {
        match std::str::from_utf8(&bytes)
            .ok()
            .and_then(|t| wtf16::parse(t).ok())
        {
            Some(v) => v,
            None => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
        }
    };
    let (status, answer) = native.start(&ask).await;
    let Some(text) = wtf16::stringify(&answer) else {
        return unavailable();
    };
    let mut response = reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        text,
        true,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
