//! The project store Node writes `projects.json` and `free-chats.json` through
//! (sbstndalton/noevia#1264; crates/server-projects `store_api`); permanent (the NOEVIA_RUST_PROJECT_STORE switch is retired).
//!
//! Internal only: its own listener on 127.0.0.1 (NOEVIA_PROJECT_STORE_PORT), never the public
//! front's router, so nothing proxied from outside reaches it. Every request carries the per-start
//! secret the web supervisor hands both processes (NOEVIA_PROJECT_STORE_TOKEN, header
//! `x-noevia-store-token`, compared in constant time); anything else is 401 and touches nothing.
//!
//! `POST /v1/read  { user, file }`                 -> 200 `{ version, text }` (`text` null: no file)
//! `POST /v1/write { user, file, version, text }`  -> 200 `{ version }`
//!                                                  | 409 `{ version, text }` (changed: merge again)
//! Errors use Node's shapes: 503 `{ error, code: "PROJECTS_BUSY" }` (the lock was held past its
//! wait), 410 `account no longer exists`, 400 a bad request, 413 a body over the limit, 500 else.
//! The body limit is [`BODY_LIMIT`]; a request's file work runs on the blocking pool.

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use server_projects::store::StoreError;
use server_projects::store_api::{self, StoreFile, WriteError};
use std::path::PathBuf;
use std::sync::Arc;

/// A text of [`store_api::TEXT_LIMIT`], JSON-escaped, plus the envelope.
pub const BODY_LIMIT: usize = 2 * store_api::TEXT_LIMIT + 64 * 1024;
pub const TOKEN_HEADER: &str = "x-noevia-store-token";
/// The shortest secret accepted (config.rs).
pub const TOKEN_MIN: usize = 32;

#[derive(Clone)]
pub struct Store {
    pub data_dir: PathBuf,
    pub token: Arc<str>,
    pub switch: server_store::RustProjects,
}

fn reply(status: StatusCode, body: &Value) -> Response<Body> {
    let mut res = Response::new(Body::from(body.to_string()));
    *res.status_mut() = status;
    res.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    res.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    reply(status, &json!({ "error": message }))
}

fn store_error(e: &StoreError) -> Response<Body> {
    match e {
        StoreError::Busy => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({ "error": "The project store is busy. Try again shortly.", "code": "PROJECTS_BUSY" }),
        ),
        StoreError::Gone => error(StatusCode::GONE, "account no longer exists"),
        StoreError::InvalidUser => error(StatusCode::BAD_REQUEST, "invalid user"),
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorised(store: &Store, headers: &HeaderMap) -> bool {
    headers
        .get(TOKEN_HEADER)
        .is_some_and(|v| same(v.as_bytes(), store.token.as_bytes()))
}

/// The request's `user` and `file`, or why not (the 400's message).
fn target(body: &Value) -> Result<(String, StoreFile), &'static str> {
    let user = body
        .get("user")
        .and_then(Value::as_str)
        .ok_or("user required")?;
    let file = body
        .get("file")
        .and_then(Value::as_str)
        .and_then(StoreFile::parse)
        .ok_or("file must be projects.json or free-chats.json")?;
    Ok((user.to_string(), file))
}

async fn handle(store: Store, headers: HeaderMap, raw: Bytes, write: bool) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let (user, file) = match target(&body) {
        Ok(t) => t,
        Err(why) => return error(StatusCode::BAD_REQUEST, why),
    };
    let done = tokio::task::spawn_blocking(move || {
        if !write {
            return match store_api::read(&store.data_dir, &user, file) {
                Ok(s) => reply(
                    StatusCode::OK,
                    &json!({ "version": s.version, "text": s.text }),
                ),
                Err(e) => store_error(&e),
            };
        }
        let (Some(version), Some(text)) = (
            body.get("version").and_then(Value::as_str),
            body.get("text").and_then(Value::as_str),
        ) else {
            return error(StatusCode::BAD_REQUEST, "version and text required");
        };
        match store_api::write(&store.data_dir, &user, file, version, text, store.switch) {
            Ok(v) => reply(StatusCode::OK, &json!({ "version": v })),
            Err(WriteError::Conflict(now)) => reply(
                StatusCode::CONFLICT,
                &json!({ "version": now.version, "text": now.text }),
            ),
            Err(WriteError::Invalid(why)) => error(StatusCode::BAD_REQUEST, why),
            Err(WriteError::Store(e)) => store_error(&e),
        }
    })
    .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

pub fn router(store: Store) -> Router {
    Router::new()
        .route(
            "/v1/read",
            post(|State(s): State<Store>, h: HeaderMap, b: Bytes| handle(s, h, b, false)),
        )
        .route(
            "/v1/write",
            post(|State(s): State<Store>, h: HeaderMap, b: Bytes| handle(s, h, b, true)),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(store)
}

/// Serves the store on `listener` until `shutdown`.
pub async fn serve(
    listener: tokio::net::TcpListener,
    store: Store,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let _ = axum::serve(listener, router(store))
        .with_graceful_shutdown(shutdown)
        .await;
}
