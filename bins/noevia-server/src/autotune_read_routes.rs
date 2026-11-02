//! GET-only status. POST start and every other verb stay with Node.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{header, HeaderValue, Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use server_projects::autotune_read as policy;
use std::{sync::Arc, time::Duration};
pub const PATH: &str = "/api/models/autotune";
const FACTS: &str = "/internal/autotune-read/facts";
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
    let model = crate::model_sizing::model_param(&parts.uri).unwrap_or_default();
    let Some((status, bytes)) = app
        .config
        .research
        .node_call(
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
