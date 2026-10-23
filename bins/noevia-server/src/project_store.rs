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
//!
//! A chat's transcript, `history-<id>.json` (sbstndalton/noevia#1071 slice 4; Rust is its only
//! writer, crates/server-projects `transcript`). `chat` is the chat id as Node holds it (reduced to
//! `[A-Za-z0-9_-]` here); `history` and `entry` are JSON values, read losslessly (a lone surrogate
//! survives) and answered the same way:
//!
//! `POST /v1/history/read   { user, chat }`           -> 200 `{ history, revision, exists }`
//! `POST /v1/history/write  { user, chat, history, baseRevision?, ifAbsent?, supersedePartial? }`
//!                                                  -> 200 `{ revision }`
//!                                                   | 409 `{ history, revision, exists, proposedRevision }`
//!                                                   | 410 `{ error, code: "CHAT_DELETED" }` (tombstoned)
//! `POST /v1/history/append-if-last { user, chat, message, entry }`
//!                                                  -> 200 `{ outcome, revision? }` (`saved`,
//!                                                     `not-last`, `no-user-message`, `deleted`)
//! `POST /v1/history/remove-if { user, chat, history }` -> 200 `{ removed }` (only that exact transcript)
//!
//! Errors use Node's shapes: 503 `{ error, code: "PROJECTS_BUSY" }` (the lock was held past its
//! wait), 410 `account no longer exists`, 400 a bad request, 413 a body over the limit, 500 else.
//! The body limit is [`BODY_LIMIT`]; a request's file work runs on the blocking pool.

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use axum::Router;
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use server_projects::store::StoreError;
use server_projects::store_api::{self, StoreFile, WriteError};
use server_projects::transcript::{self, Appended, Base, Saved, TranscriptError};
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
    /// This container start's NOEVIA_START_ID: a client save leaves the partial of a reply still
    /// running in this start alone (empty: none can be running).
    pub start_id: Arc<str>,
}

fn reply(status: StatusCode, body: &Value) -> Response<Body> {
    reply_text(status, body.to_string())
}

fn reply_text(status: StatusCode, text: String) -> Response<Body> {
    let mut res = Response::new(Body::from(text));
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

/// A transcript op's answer, written losslessly.
fn reply_js(status: StatusCode, body: JValue) -> Response<Body> {
    match wtf16::stringify(&body) {
        Some(text) => reply_text(status, text),
        None => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
    }
}

fn transcript_error(e: &TranscriptError) -> Response<Body> {
    match e {
        TranscriptError::InvalidChat => error(StatusCode::BAD_REQUEST, "invalid chat id"),
        TranscriptError::Unsanitisable => error(StatusCode::BAD_REQUEST, "invalid history"),
        TranscriptError::Store(e) => store_error(e),
    }
}

fn current_js(c: transcript::Current) -> Vec<(String, JValue)> {
    vec![
        ("history".into(), c.history),
        ("revision".into(), JValue::Str(c.revision)),
        ("exists".into(), JValue::Bool(c.exists)),
    ]
}

#[derive(Clone, Copy)]
enum Op {
    Read,
    Write,
    Append,
    RemoveIf,
}

/// The JSON list at `key`, or the 400 to answer.
fn list<'a>(body: &'a JValue, key: &str) -> Result<&'a [JValue], Box<Response<Body>>> {
    match body.get(key) {
        JValue::Arr(items) => Ok(items),
        _ => Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            &format!("{key} must be a list"),
        ))),
    }
}

fn history_op(store: &Store, op: Op, body: &JValue) -> Response<Body> {
    let (Some(user), Some(chat)) = (body.get("user").as_str(), body.get("chat").as_str()) else {
        return error(StatusCode::BAD_REQUEST, "user and chat required");
    };
    let data_dir = store.data_dir.as_path();
    match op {
        Op::Read => match transcript::read(data_dir, user, chat) {
            Ok(c) => reply_js(StatusCode::OK, JValue::Obj(current_js(c))),
            Err(e) => transcript_error(&e),
        },
        Op::Write => {
            let history = match list(body, "history") {
                Ok(h) => h,
                Err(r) => return *r,
            };
            let base = match (body.get("baseRevision"), body.get("ifAbsent")) {
                (JValue::Str(r), JValue::Undefined | JValue::Null | JValue::Bool(false)) => {
                    Base::Revision(r.clone())
                }
                (JValue::Undefined | JValue::Null, JValue::Bool(true)) => Base::Absent,
                (
                    JValue::Undefined | JValue::Null,
                    JValue::Undefined | JValue::Null | JValue::Bool(false),
                ) => Base::Any,
                _ => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "baseRevision must be a string, ifAbsent true, not both",
                    )
                }
            };
            let supersede =
                (body.get("supersedePartial").as_bool() == Some(true)).then_some(&*store.start_id);
            match transcript::write(data_dir, user, chat, history, &base, supersede) {
                Ok(Saved::Saved { revision }) => reply_js(
                    StatusCode::OK,
                    JValue::obj([("revision", JValue::Str(revision))]),
                ),
                Ok(Saved::Conflict { current, proposed }) => {
                    let mut items = current_js(current);
                    items.push(("proposedRevision".into(), JValue::Str(proposed)));
                    reply_js(StatusCode::CONFLICT, JValue::Obj(items))
                }
                Ok(Saved::Deleted) => reply(
                    StatusCode::GONE,
                    &json!({ "error": "This chat was deleted.", "code": "CHAT_DELETED" }),
                ),
                Err(e) => transcript_error(&e),
            }
        }
        Op::Append => {
            let Some(message) = body.get("message").as_str() else {
                return error(StatusCode::BAD_REQUEST, "message required");
            };
            let entry = body.get("entry");
            if !matches!(entry, JValue::Obj(_)) {
                return error(StatusCode::BAD_REQUEST, "entry must be an object");
            }
            match transcript::append_if_last(data_dir, user, chat, message, entry) {
                Ok(done) => {
                    let mut items =
                        vec![("outcome".to_string(), JValue::Str(done.outcome().into()))];
                    if let Appended::Saved { revision } = done {
                        items.push(("revision".into(), JValue::Str(revision)));
                    }
                    reply_js(StatusCode::OK, JValue::Obj(items))
                }
                Err(e) => transcript_error(&e),
            }
        }
        Op::RemoveIf => {
            let history = match list(body, "history") {
                Ok(h) => h,
                Err(r) => return *r,
            };
            match transcript::remove_if(data_dir, user, chat, history) {
                Ok(removed) => reply(StatusCode::OK, &json!({ "removed": removed })),
                Err(e) => transcript_error(&e),
            }
        }
    }
}

async fn handle_history(store: Store, headers: HeaderMap, raw: Bytes, op: Op) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let done = tokio::task::spawn_blocking(move || {
        let Ok(text) = std::str::from_utf8(&raw) else {
            return error(StatusCode::BAD_REQUEST, "invalid JSON");
        };
        match wtf16::parse(text) {
            Ok(body @ JValue::Obj(_)) => history_op(&store, op, &body),
            _ => error(StatusCode::BAD_REQUEST, "invalid JSON"),
        }
    })
    .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

fn history_route(op: Op) -> axum::routing::MethodRouter<Store> {
    post(move |State(s): State<Store>, h: HeaderMap, b: Bytes| handle_history(s, h, b, op))
}

pub fn router(store: Store) -> Router {
    Router::new()
        .route("/v1/history/read", history_route(Op::Read))
        .route("/v1/history/write", history_route(Op::Write))
        .route("/v1/history/append-if-last", history_route(Op::Append))
        .route("/v1/history/remove-if", history_route(Op::RemoveIf))
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
