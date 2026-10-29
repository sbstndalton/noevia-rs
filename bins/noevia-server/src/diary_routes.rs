//! The Diary routes that are only a proxy and a policy (sbstndalton/noevia#2185), permanent and
//! with no switch: `/api/diary/source`, `/today`, `/storage-import`, `/workspace-export` and
//! `/workspace-import`. crates/server-diary holds the sidecar client and every route's answers;
//! this file is the front's side of it: the request gate (session or token, origin, CSRF, browser
//! only), the person's store, and the reply's headers.
//!
//! Order follows core: the gate first (401/403), then the add-on (404), the method (405), the body,
//! and only then the sidecar. The sidecar stays the single writer: the front passes on the request
//! the signed-in person made, signed for that person, and nothing else.
//!
//! Not moved, and why (each reads Node's memory or its own table): `/api/diary/context` (the
//! project list), `/api/diary/exchanges` (the set of running jobs), `/api/diary/local-exchange`
//! (the model throttle and the inference gate), `/api/diary/storage-status` (the storage-login
//! latch), `/api/diary-connector` and `/api/profile/diary-connectors`.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{header, HeaderValue, Request, Response, StatusCode};
use axum::response::IntoResponse;
use server_diary::{Ctx, ReplyBody, Route};
use std::sync::Arc;

fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Diary storage is unavailable right now. Try again shortly.",
        true,
    )
}

/// core `json()` for the JSON answers; the archive keeps its own headers (workspace-export.cjs).
fn into_response(answer: server_diary::Reply) -> Response<Body> {
    let status = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::BAD_GATEWAY);
    match answer.body {
        ReplyBody::Json(text) => reply::json_text(status, text, true),
        ReplyBody::Zip(stream) => {
            let mut res = Response::new(Body::new(stream));
            *res.status_mut() = status;
            let h = res.headers_mut();
            reply::security_headers(h, true);
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"noevia-workspace.zip\""),
            );
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
    }
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    // The route table matches a trailing slash; core's `p === '/api/diary/...'` does not.
    let Some(route) = Route::from_path(&path) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    let (parts, body) = req.into_parts();
    let Some(data_dir) = app.config.data_dir.clone() else {
        return identity::Rejection::Unavailable.into_response();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let gate_layer = Arc::clone(&layer);
    let id = match tokio::task::spawn_blocking(move || {
        gate_layer.gate_blocking(&creds, &gate_path, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let Some(diary) = app.diary.clone() else {
        return unavailable();
    };
    let store = match tokio::task::spawn_blocking(move || layer.store_blocking()).await {
        Ok(Ok(store)) => store,
        _ => return unavailable(),
    };
    let request = server_diary::Request {
        method: parts.method.as_str().to_owned(),
        target: parts
            .uri
            .path_and_query()
            .map_or_else(|| path.clone(), |pq| pq.as_str().to_owned()),
        content_length: parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    };
    let ctx = Ctx {
        identity: id,
        store,
        data_dir,
    };
    into_response(diary.serve(route, ctx, request, body).await)
}
