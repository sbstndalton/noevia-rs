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
//! Each of the three mutations also takes an optional `origin`, who asked (journalled on the
//! change's entry): one of `history` (the user's own save), `import`, `turn`, `remove`, `other`;
//! absent or null is `other`, anything else is 400 and changes nothing. Other unknown fields are
//! ignored, as they always were.
//!
//! A user's chat change journal (sbstndalton/noevia#2153; crates/server-projects `journal`), one
//! entry per transcript mutation, which Node tails to run the chat mirror and brain:
//!
//! `POST /v1/changes/read { user, after? }`         -> 200 `{ entries, oldestSeq, latestSeq, more, start }`
//!   Each entry is `{ seq, chatId, revision, op, origin, start, at }` (`origin` missing on
//!   entries journalled before it existed).
//!   `entries` those with `seq > after` (default 0), oldest first, at most 1,000 (`more`: read
//!   again after the last). `oldestSeq`/`latestSeq` the journal's bounds (null: empty): a cursor
//!   below `oldestSeq - 1` fell behind a rotation, one above `latestSeq` predates a reset; either
//!   way the reader rescans. `start` is this process's NOEVIA_START_ID: a different one than the
//!   reader last saw means the store restarted (rescan too).
//!
//! A chat's delete, the whole cascade in one hold of the transcript lock (sbstndalton/noevia#1071
//! slice 6; crates/server-projects `chat_delete`): the tombstone, the list without the chat (written
//! here, under its file's lock), then the chat's transcript, context meter, brain note and saved
//! partial reply. `kind` is `free` (free-chats.json), `project` (with `projectId`) or `unlisted`
//! (a deleted project's chat, in no list any more); `origin` as above, absent or null `remove`:
//!
//! `POST /v1/chat/delete { user, chat, kind, projectId?, origin? }`
//!                                                  -> 200 `{ deleted, swept, leftovers }`
//!                                                   | 409 `{ error, code: "CHAT_LISTED" }` (`unlisted`
//!                                                     for a chat a list still holds: nothing changed)
//!   `deleted`: taken out of its list by this call; `swept`: already deleted and in no list, its
//!   leftover files removed (a delete a crash cut short); neither: no such chat. A failed list save
//!   takes back the tombstone and removes nothing (the error below). The attachments project is
//!   Node's to free (its orphan reaper, once the chat is tombstoned and in no list).
//!
//! Errors use Node's shapes: 503 `{ error, code: "PROJECTS_BUSY" }` (the lock was held past its
//! wait), 410 `account no longer exists`, 400 a bad request, 413 a body over the limit, 500 else.
//! The body limit is [`BODY_LIMIT`]; a request's file work runs on the blocking pool.

use axum::body::{Body, Bytes};
// Slice7 foundation adds secret-authenticated POST /v1/chat/grant-context {user,chat}
// and POST /v1/chat/move {user,chat,projectId:null|string,frame?}. Holders are tagged
// free/project(id)/absent; generations are canonical decimal strings, bound to the start id.
// A prepared move fences list writers until strict recovery completes. These internal operations
// do NOT change public route ownership or make older Node grants generation-aware; that paired
// migration must be reviewed before the public move switch is enabled.

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::post;
use axum::Router;
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use server_projects::chat_delete::{self, DeleteError, Kind};
use server_projects::chat_move::{self, Holder, MoveError};
use server_projects::journal;
use server_projects::store::StoreError;
use server_projects::store_api::{self, StoreFile, WriteError};
use server_projects::transcript::{self, Appended, Base, Origin, Saved, TranscriptError};
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

/// The request's `origin` (absent or null: `other`), or the 400 to answer.
fn origin(body: &JValue) -> Result<Origin, Box<Response<Body>>> {
    match body.get("origin") {
        JValue::Undefined | JValue::Null => Ok(Origin::Other),
        JValue::Str(s) => Origin::parse(s).ok_or(()),
        _ => Err(()),
    }
    .map_err(|()| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            "origin must be one of history, import, turn, remove, other",
        ))
    })
}

fn history_op(store: &Store, op: Op, body: &JValue) -> Response<Body> {
    let (Some(user), Some(chat)) = (body.get("user").as_str(), body.get("chat").as_str()) else {
        return error(StatusCode::BAD_REQUEST, "user and chat required");
    };
    let origin = match op {
        Op::Read => Origin::Other,
        _ => match origin(body) {
            Ok(o) => o,
            Err(r) => return *r,
        },
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
            match transcript::write_as(data_dir, user, chat, history, &base, supersede, origin) {
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
            match transcript::append_if_last_as(data_dir, user, chat, message, entry, origin) {
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
            match transcript::remove_if_as(data_dir, user, chat, history, origin) {
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

fn chat_delete_op(store: &Store, body: &JValue) -> Response<Body> {
    let (Some(user), Some(chat)) = (body.get("user").as_str(), body.get("chat").as_str()) else {
        return error(StatusCode::BAD_REQUEST, "user and chat required");
    };
    let kind = match (body.get("kind").as_str(), body.get("projectId")) {
        (Some("free"), JValue::Undefined | JValue::Null) => Kind::Free,
        (Some("unlisted"), JValue::Undefined | JValue::Null) => Kind::Unlisted,
        (Some("project"), JValue::Str(p)) => Kind::Project(p),
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "kind must be free, unlisted, or project with a projectId",
            )
        }
    };
    let origin = match body.get("origin") {
        JValue::Undefined | JValue::Null => Origin::Remove,
        _ => match origin(body) {
            Ok(o) => o,
            Err(r) => return *r,
        },
    };
    match chat_delete::delete(&store.data_dir, user, chat, kind, origin, store.switch) {
        Ok(o) => reply(
            StatusCode::OK,
            &json!({ "deleted": o.deleted, "swept": o.swept, "leftovers": o.leftovers }),
        ),
        Err(DeleteError::InvalidChat) => error(StatusCode::BAD_REQUEST, "invalid chat id"),
        Err(DeleteError::Listed) => reply(
            StatusCode::CONFLICT,
            &json!({ "error": "a list still holds this chat", "code": "CHAT_LISTED" }),
        ),
        Err(DeleteError::Unsupported) => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
        Err(DeleteError::Store(e)) => store_error(&e),
    }
}

async fn handle_chat_delete(store: Store, headers: HeaderMap, raw: Bytes) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let done = tokio::task::spawn_blocking(move || {
        let Ok(text) = std::str::from_utf8(&raw) else {
            return error(StatusCode::BAD_REQUEST, "invalid JSON");
        };
        match wtf16::parse(text) {
            Ok(body @ JValue::Obj(_)) => chat_delete_op(&store, &body),
            _ => error(StatusCode::BAD_REQUEST, "invalid JSON"),
        }
    })
    .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

fn move_error(problem: MoveError) -> Response<Body> {
    match problem {
        MoveError::Store(StoreError::Busy) => {
            let mut response = store_error(&StoreError::Busy);
            response
                .headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("1"));
            response
        }
        MoveError::Store(e) => store_error(&e),
        MoveError::Invalid => error(StatusCode::BAD_REQUEST, "invalid chat move or start id"),
        MoveError::NotFound => error(StatusCode::NOT_FOUND, "no such chat or project"),
        MoveError::Ambiguous => reply(
            StatusCode::CONFLICT,
            &json!({"error":"ambiguous canonical chat identity","code":"CHAT_ID_AMBIGUOUS"}),
        ),
        MoveError::Capacity => reply(
            StatusCode::CONFLICT,
            &json!({"error":"the move would evict chat metadata","code":"CHAT_DESTINATION_FULL"}),
        ),
    }
}

fn chat_move_op(store: &Store, body: &JValue, moving: bool) -> Response<Body> {
    let (Some(user), Some(chat)) = (body.get("user").as_str(), body.get("chat").as_str()) else {
        return error(StatusCode::BAD_REQUEST, "user and chat required");
    };
    if !moving {
        return match chat_move::grant_context(&store.data_dir, user, chat, &store.start_id) {
            Ok(context) => reply(StatusCode::OK, &context.json()),
            Err(e) => move_error(e),
        };
    }
    let destination = match body.get("projectId") {
        JValue::Null => Holder::Free,
        JValue::Str(id) => Holder::Project(id.clone()),
        _ => return error(StatusCode::BAD_REQUEST, "projectId (or null) required"),
    };
    let patch = match body.get("frame") {
        JValue::Undefined => None,
        value => Some(value),
    };
    match chat_move::move_chat(
        &store.data_dir,
        user,
        chat,
        destination,
        patch,
        &store.start_id,
    ) {
        Ok(moved) => reply(
            StatusCode::OK,
            &json!({"from":moved.from.json(),"to":moved.to.json(),"generation":moved.generation}),
        ),
        Err(e) => move_error(e),
    }
}

async fn handle_chat_move(
    store: Store,
    headers: HeaderMap,
    raw: Bytes,
    moving: bool,
) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    tokio::task::spawn_blocking(move || {
        let Ok(text) = std::str::from_utf8(&raw) else {
            return error(StatusCode::BAD_REQUEST, "invalid JSON");
        };
        match wtf16::parse(text) {
            Ok(body @ JValue::Obj(_)) => chat_move_op(&store, &body, moving),
            _ => error(StatusCode::BAD_REQUEST, "invalid JSON"),
        }
    })
    .await
    .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

async fn handle_changes(store: Store, headers: HeaderMap, raw: Bytes) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let Some(user) = body.get("user").and_then(Value::as_str).map(str::to_string) else {
        return error(StatusCode::BAD_REQUEST, "user required");
    };
    let after = match body.get("after") {
        None | Some(Value::Null) => 0,
        Some(v) => match v.as_u64() {
            Some(n) => n,
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "after must be a non-negative integer",
                )
            }
        },
    };
    let done =
        tokio::task::spawn_blocking(move || match journal::read(&store.data_dir, &user, after) {
            Ok(c) => reply(
                StatusCode::OK,
                &json!({
                    "entries": c.entries,
                    "oldestSeq": c.oldest_seq,
                    "latestSeq": c.latest_seq,
                    "more": c.more,
                    "start": &*store.start_id,
                }),
            ),
            Err(e) => store_error(&e),
        })
        .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

fn history_route(op: Op) -> axum::routing::MethodRouter<Store> {
    post(move |State(s): State<Store>, h: HeaderMap, b: Bytes| handle_history(s, h, b, op))
}

pub fn router(store: Store) -> Router {
    Router::new()
        .route("/v1/source-jobs/recover", source_route(SourceOp::Recover))
        .route("/v1/source-jobs/start", source_route(SourceOp::Start))
        .route("/v1/source-jobs/abandon", source_route(SourceOp::Abandon))
        .route("/v1/source-jobs/progress", source_route(SourceOp::Progress))
        .route("/v1/source-jobs/finish", source_route(SourceOp::Finish))
        .route("/v1/source-jobs/read", source_route(SourceOp::Read))
        .route(
            "/v1/research-jobs/recover",
            research_route(ResearchOp::Recover),
        )
        .route("/v1/research-jobs/start", research_route(ResearchOp::Start))
        .route(
            "/v1/research-jobs/abandon",
            research_route(ResearchOp::Abandon),
        )
        .route("/v1/research-jobs/event", research_route(ResearchOp::Event))
        .route(
            "/v1/research-jobs/cancel",
            research_route(ResearchOp::Cancel),
        )
        .route(
            "/v1/research-jobs/finish",
            research_route(ResearchOp::Finish),
        )
        .route(
            "/v1/research-jobs/report",
            research_route(ResearchOp::Report),
        )
        .route(
            "/v1/research-jobs/prepare",
            research_route(ResearchOp::Prepare),
        )
        .route(
            "/v1/research-jobs/artifact",
            research_route(ResearchOp::Artifact),
        )
        .route("/v1/research-jobs/clear", research_route(ResearchOp::Clear))
        .route("/v1/research-jobs/read", research_route(ResearchOp::Read))
        .route("/v1/research-jobs/list", research_route(ResearchOp::List))
        .route(
            "/v1/chat/grant-context",
            post(|State(s): State<Store>, h: HeaderMap, b: Bytes| handle_chat_move(s, h, b, false)),
        )
        .route(
            "/v1/chat/move",
            post(|State(s): State<Store>, h: HeaderMap, b: Bytes| handle_chat_move(s, h, b, true)),
        )
        .route("/v1/history/read", history_route(Op::Read))
        .route("/v1/history/write", history_route(Op::Write))
        .route("/v1/history/append-if-last", history_route(Op::Append))
        .route("/v1/history/remove-if", history_route(Op::RemoveIf))
        .route(
            "/v1/chat/delete",
            post(|State(s): State<Store>, h: HeaderMap, b: Bytes| handle_chat_delete(s, h, b)),
        )
        .route(
            "/v1/changes/read",
            post(|State(s): State<Store>, h: HeaderMap, b: Bytes| handle_changes(s, h, b)),
        )
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

// noevia-source-jobs-store/1: secret-authenticated internal calls only. Node executes work;
// Rust alone persists source-kind journals.
#[derive(Clone, Copy)]
enum SourceOp {
    Recover,
    Start,
    Abandon,
    Progress,
    Finish,
    Read,
}

fn source_error(e: server_projects::source_jobs::Error) -> Response<Body> {
    use server_projects::source_jobs::Error as E;
    match e {
        E::BadRequest => error(StatusCode::BAD_REQUEST, "invalid source job request"),
        E::Gone => error(StatusCode::GONE, "account no longer exists"),
        E::NotFound => error(StatusCode::NOT_FOUND, "source job or project not found"),
        E::Busy => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error":"The source job store is busy.","code":"PROJECTS_BUSY"}),
        ),
        E::Stale => error(StatusCode::CONFLICT, "stale source job owner or run"),
        E::TooMany => error(
            StatusCode::TOO_MANY_REQUESTS,
            "Source processing is busy; wait for the current operation and retry.",
        ),
        E::Corrupt => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error":"Source job state unknown; review required","code":"JOBS_UNKNOWN"}),
        ),
        E::Io => error(StatusCode::INTERNAL_SERVER_ERROR, "source job store failed"),
    }
}

async fn handle_source_jobs(
    store: Store,
    headers: HeaderMap,
    raw: Bytes,
    op: SourceOp,
) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let Ok(body) = server_projects::source_jobs::parse_js(&raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let Some(user) = body.get("user").and_then(Value::as_str).map(str::to_owned) else {
        return error(StatusCode::BAD_REQUEST, "user required");
    };
    let done = tokio::task::spawn_blocking(move || {
        use server_projects::source_jobs as jobs;
        let required = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        let ctx = jobs::Context {
            data_dir: &store.data_dir,
            start_id: &store.start_id,
        };
        if matches!(op, SourceOp::Read) {
            let (Some(project), Some(id)) = (required("projectId"), required("id")) else {
                return error(StatusCode::BAD_REQUEST, "projectId and id required");
            };
            return match jobs::read(ctx, &user, &project, &id) {
                Ok(Some(job)) => match jobs::stringify_js(&job) {
                    Ok(text) => reply_text(StatusCode::OK, text),
                    Err(e) => source_error(e),
                },
                Ok(None) => error(StatusCode::NOT_FOUND, "source job or project not found"),
                Err(e) => source_error(e),
            };
        }
        let (Some(start), Some(born), Some(owner)) =
            (required("start"), required("born"), required("owner"))
        else {
            return error(StatusCode::BAD_REQUEST, "start, born and owner required");
        };
        let result = match op {
            SourceOp::Recover => {
                jobs::recover(ctx, &user, &start, &born, &owner).map(|count| json!({"count":count}))
            }
            SourceOp::Start => match (required("projectId"), required("request")) {
                (Some(project), Some(request)) => {
                    jobs::start(ctx, &user, &start, &born, &owner, &project, &request)
                        .map(|(id, run)| json!({"id":id,"run":run}))
                }
                _ => Err(jobs::Error::BadRequest),
            },
            SourceOp::Abandon => match required("request") {
                Some(request) => jobs::abandon(ctx, &user, &start, &born, &owner, &request)
                    .map(|()| json!({"ok":true})),
                None => Err(jobs::Error::BadRequest),
            },
            SourceOp::Progress => match (
                required("id"),
                required("run"),
                required("stage"),
                body.get("n").and_then(Value::as_u64),
            ) {
                (Some(id), Some(run), Some(stage), Some(n)) => {
                    jobs::progress(ctx, &user, &start, &born, &owner, &id, &run, &stage, n)
                        .map(|()| json!({"ok":true}))
                }
                _ => Err(jobs::Error::BadRequest),
            },
            SourceOp::Finish => match (
                required("id"),
                required("run"),
                body.get("ok").and_then(Value::as_bool),
            ) {
                (Some(id), Some(run), Some(ok)) => jobs::finish(
                    ctx,
                    &user,
                    &start,
                    &born,
                    &owner,
                    &id,
                    &run,
                    ok,
                    body.get("result").unwrap_or(&Value::Null),
                    required("error").as_deref(),
                )
                .map(|()| json!({"ok":true})),
                _ => Err(jobs::Error::BadRequest),
            },
            SourceOp::Read => return error(StatusCode::BAD_REQUEST, "invalid source job request"),
        };
        match result {
            Ok(value) => reply(StatusCode::OK, &value),
            Err(e) => source_error(e),
        }
    })
    .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

fn source_route(op: SourceOp) -> axum::routing::MethodRouter<Store> {
    post(move |State(s): State<Store>, h: HeaderMap, b: Bytes| handle_source_jobs(s, h, b, op))
}

// Internal-only, paired research journal capability. Node remains the model/file executor.
#[derive(Clone, Copy)]
enum ResearchOp {
    Recover,
    Start,
    Abandon,
    Event,
    Cancel,
    Finish,
    Report,
    Prepare,
    Artifact,
    Clear,
    Read,
    List,
}

fn research_error(e: server_projects::research_jobs::Error) -> Response<Body> {
    use server_projects::research_jobs::Error as E;
    match e {
        E::BadRequest => error(StatusCode::BAD_REQUEST, "invalid research job request"),
        E::Gone => error(StatusCode::GONE, "account no longer exists"),
        E::NotFound => error(StatusCode::NOT_FOUND, "research job or project not found"),
        E::Busy => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error":"The research job store is busy.","code":"PROJECTS_BUSY"}),
        ),
        E::Stale => error(
            StatusCode::CONFLICT,
            "stale research job owner, run or operation",
        ),
        E::TooMany => error(
            StatusCode::CONFLICT,
            "A research job is already running in this project. Wait for it or cancel it.",
        ),
        E::Corrupt => reply(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error":"Research job state unknown; review required","code":"JOBS_UNKNOWN"}),
        ),
        E::Io => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "research job store failed",
        ),
    }
}

async fn handle_research_jobs(
    store: Store,
    headers: HeaderMap,
    raw: Bytes,
    op: ResearchOp,
) -> Response<Body> {
    if !authorised(&store, &headers) {
        return error(StatusCode::UNAUTHORIZED, "unauthorised");
    }
    let Ok(body) = server_projects::research_jobs::parse_js(&raw) else {
        return error(StatusCode::BAD_REQUEST, "invalid JSON");
    };
    let Some(user) = body.get("user").and_then(Value::as_str).map(str::to_owned) else {
        return error(StatusCode::BAD_REQUEST, "user required");
    };
    let done = tokio::task::spawn_blocking(move || {
        use server_projects::research_jobs as jobs;
        let required = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        let ctx = jobs::Context {
            data_dir: &store.data_dir,
            start_id: &store.start_id,
        };
        if matches!(op, ResearchOp::Read | ResearchOp::List) {
            let Some(project) = required("projectId") else {
                return error(StatusCode::BAD_REQUEST, "projectId required");
            };
            let answer = if matches!(op, ResearchOp::Read) {
                let Some(id) = required("id") else {
                    return error(StatusCode::BAD_REQUEST, "id required");
                };
                match jobs::read(ctx, &user, &project, &id) {
                    Ok(Some(v)) => Ok(v),
                    Ok(None) => {
                        return error(StatusCode::NOT_FOUND, "research job or project not found")
                    }
                    Err(e) => Err(e),
                }
            } else {
                jobs::list(ctx, &user, &project).map(|v| json!({"jobs":v}))
            };
            return match answer {
                Ok(v) => match jobs::stringify_js(&v) {
                    Ok(text) => reply_text(StatusCode::OK, text),
                    Err(e) => research_error(e),
                },
                Err(e) => research_error(e),
            };
        }
        let (Some(start), Some(born), Some(owner)) =
            (required("start"), required("born"), required("owner"))
        else {
            return error(StatusCode::BAD_REQUEST, "start, born and owner required");
        };
        let answer = match op {
            ResearchOp::Recover => {
                jobs::recover(ctx, &user, &start, &born, &owner).map(|count| json!({"count":count}))
            }
            ResearchOp::Start => match (
                required("projectId"),
                required("request"),
                required("question"),
                required("mode"),
                body.get("questions"),
            ) {
                (Some(project), Some(request), Some(question), Some(mode), Some(questions)) => {
                    jobs::start(
                        ctx, &user, &start, &born, &owner, &project, &request, &question, &mode,
                        questions,
                    )
                    .map(|(id, run)| json!({"id":id,"run":run}))
                }
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Abandon => match required("request") {
                Some(request) => jobs::abandon(ctx, &user, &start, &born, &owner, &request)
                    .map(|()| json!({"ok":true})),
                None => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Event => match (
                required("id"),
                required("run"),
                body.get("n").and_then(Value::as_u64),
                required("kind"),
                body.get("data"),
            ) {
                (Some(id), Some(run), Some(n), Some(kind), Some(data)) => {
                    jobs::event(ctx, &user, &start, &born, &owner, &id, &run, n, &kind, data)
                        .map(|()| json!({"ok":true}))
                }
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Cancel => match (required("projectId"), required("id"), required("run")) {
                (Some(project), Some(id), Some(run)) => {
                    jobs::cancel(ctx, &user, &start, &born, &owner, &project, &id, &run)
                }
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Finish => match (
                required("id"),
                required("run"),
                body.get("ok").and_then(Value::as_bool),
            ) {
                (Some(id), Some(run), Some(ok)) => jobs::finish(
                    ctx,
                    &user,
                    &start,
                    &born,
                    &owner,
                    &id,
                    &run,
                    ok,
                    body.get("result").unwrap_or(&Value::Null),
                    required("error").as_deref(),
                )
                .map(|()| json!({"ok":true})),
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Report => match (required("id"), required("run"), body.get("result")) {
                (Some(id), Some(run), Some(result)) => {
                    jobs::report(ctx, &user, &start, &born, &owner, &id, &run, result)
                        .map(|()| json!({"ok":true}))
                }
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Prepare | ResearchOp::Artifact => match (
                required("projectId"),
                required("id"),
                required("run"),
                required("name"),
                required("hash"),
                body.get("bytes").and_then(Value::as_u64),
                required("target"),
            ) {
                (
                    Some(project),
                    Some(id),
                    Some(run),
                    Some(name),
                    Some(hash),
                    Some(bytes),
                    target,
                ) => {
                    let answer = if matches!(op, ResearchOp::Prepare) {
                        jobs::prepare_file(
                            ctx,
                            &user,
                            &start,
                            &born,
                            &owner,
                            &project,
                            &id,
                            &run,
                            &name,
                            &hash,
                            bytes,
                            target.as_deref().unwrap_or(""),
                        )
                    } else {
                        jobs::artifact(
                            ctx, &user, &start, &born, &owner, &project, &id, &run, &name, &hash,
                            bytes,
                        )
                    };
                    answer.map(|()| json!({"ok":true}))
                }
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Clear => match (
                required("projectId"),
                required("id"),
                required("run"),
                required("name"),
                required("hash"),
            ) {
                (Some(project), Some(id), Some(run), Some(name), Some(hash)) => jobs::clear_file(
                    ctx, &user, &start, &born, &owner, &project, &id, &run, &name, &hash,
                )
                .map(|()| json!({"ok":true})),
                _ => Err(jobs::Error::BadRequest),
            },
            ResearchOp::Read | ResearchOp::List => {
                return error(StatusCode::BAD_REQUEST, "invalid research job request")
            }
        };
        match answer {
            Ok(v) => match jobs::stringify_js(&v) {
                Ok(text) => reply_text(StatusCode::OK, text),
                Err(e) => research_error(e),
            },
            Err(e) => research_error(e),
        }
    })
    .await;
    done.unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"))
}

fn research_route(op: ResearchOp) -> axum::routing::MethodRouter<Store> {
    post(move |State(s): State<Store>, h: HeaderMap, b: Bytes| handle_research_jobs(s, h, b, op))
}
