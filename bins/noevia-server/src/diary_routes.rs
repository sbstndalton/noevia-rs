//! The Diary routes the front answers (sbstndalton/noevia#2185, #2206), permanent and with no
//! switch. crates/server-diary holds the sidecar client and the proxy routes' answers; this file is
//! the front's side of it: the request gate (session or token, origin, CSRF, browser only), the
//! person's store, and the reply's headers.
//!
//! * Proxies (#2185): `/api/diary/source`, `/today`, `/storage-import`, `/workspace-export`,
//!   `/workspace-import`.
//! * `/api/diary/storage-status`: the sidecar's status plus the storage-login latch, a durable file
//!   both processes use (crates/server-projects `storage_latch`).
//! * `/api/diary/exchanges`: the recovery records of the Diary tab, with Node's published set of
//!   running exchanges (crates/server-diary `exchanges`).
//! * `/api/diary/context`: the Diary attachments project in the person's `projects.json`
//!   (crates/server-projects `diary_context`), under the project routes' account claim.
//! * `/api/diary-connector` and `/api/profile/diary-connectors[/{id}]`: the connector credentials
//!   and the file bridge they open (crates/server-diary `connector`). The first takes no session:
//!   the connector's credential is the authentication.
//! * `/api/diary`: nothing, as in Node: the gate, then its not-found.
//!
//! Order follows core: the gate first (401/403), then a legacy claim still to come (503, which core
//! makes before routing), then the add-on (404), the method (405), the body, and only then the
//! sidecar. The sidecar stays the single writer of the Diary: the front passes on the request the
//! signed-in person made, signed for that person, and nothing else.
//!
//! Not moved: `POST /api/diary/local-exchange`. It takes the 60 per minute model throttle that chat
//! shares and, from index.cjs `handleRequest`, the inference maintenance gate for the whole life of
//! its response, so that a preset apply or a tune waits for it; both live in Node's memory
//! (llamacpp-manager `maintenance`, `llmRateLimited`) beside the chat turn that takes them.

use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use server_diary::connector::{self, Fault, Verified};
use server_diary::{Ctx, Held, ReplyBody, Route, StorageIdentity, StorageLatch};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `connectorRate`: unknown credentials share a bucket per address (or one, without TRUST_PROXY); a
/// credential has its own.
const INVALID_LIMIT: u64 = 300;
const CREDENTIAL_LIMIT: u64 = 120;
/// `readObject(req, 4 * 1024 * 1024)` of the connector endpoint, and readJson's 1 MiB default.
const CONNECTOR_BODY: usize = 4 * 1024 * 1024;
const PROFILE_BODY: usize = 1024 * 1024;
/// How long these small JSON bodies may take to arrive (core had only the server-wide 20 minutes).
const BODY_DEADLINE: Duration = Duration::from_secs(120);

const CONNECTORS: &str = "/api/profile/diary-connectors";

fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Diary storage is unavailable right now. Try again shortly.",
        true,
    )
}

fn json_text(status: u16, text: String) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        text,
        true,
    )
}

fn error(status: u16, message: &str) -> Response<Body> {
    reply::error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        message,
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

/// The storage-login latch (core `storage-latch.cjs`) as the status route reads it: the shared file in
/// the data directory, the same rows and the same transitions as Node's.
pub struct NativeLatch {
    pub data_dir: PathBuf,
}

impl StorageLatch for NativeLatch {
    fn held(&self, user_id: &str, storage: &StorageIdentity<'_>) -> Option<Held> {
        let fingerprint = server_projects::storage_latch::fingerprint(
            storage.kind,
            storage.base_url,
            storage.bucket,
            storage.username,
            &storage.secret,
        );
        match server_projects::storage_latch::status(
            &self.data_dir,
            user_id,
            &fingerprint,
            server_projects::wall_ms(),
        ) {
            Ok(Some(s)) => Some(Held {
                kind: s.kind,
                next_attempt_at: s.next_attempt_at,
            }),
            Ok(None) | Err(_) => None,
        }
    }
}

/// The request gate for a session or token route, in Node's order. `Err` is the answer to send.
async fn gate(
    app: &Arc<App>,
    parts: &Parts,
    path: &str,
) -> Result<(server_auth::Identity, PathBuf, i64), Box<Response<Body>>> {
    let Some(data_dir) = app.config.data_dir.clone() else {
        return Err(Box::new(identity::Rejection::Unavailable.into_response()));
    };
    let creds = identity::creds_of(parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.to_owned();
    match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now)).await {
        Ok(Ok(id)) => Ok((id, data_dir, now)),
        Ok(Err(rejection)) => Err(Box::new(rejection.into_response())),
        Err(_) => Err(Box::new(identity::Rejection::Unavailable.into_response())),
    }
}

/// Node claims a pre-multi-user data dir for the administrator before any route (core index.cjs
/// handleRequestInner); the project routes make that claim, the Diary proxies do not. Until it has
/// happened the workspace (its migration.json marker, its storage row) is not the user's yet, so
/// nothing goes to the sidecar: the project routes' 503.
async fn claim_pending(data_dir: &std::path::Path, user: &str) -> Option<Response<Body>> {
    let (dir, user) = (data_dir.to_path_buf(), user.to_owned());
    match tokio::task::spawn_blocking(move || server_projects::store::legacy_pending(&dir, &user))
        .await
    {
        Ok(Ok(false)) => None,
        Ok(Ok(true)) => Some(reply::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Your account is still being set up; try again in a moment.",
            true,
        )),
        _ => Some(reply::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Internal error",
            true,
        )),
    }
}

/// `exact` pathnames only: core compares `p === '/api/diary/...'`, and the route table also matches
/// a trailing slash.
pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    match path.as_str() {
        "/api/diary-connector" => return connector_endpoint(app, conn, req).await,
        CONNECTORS => return connectors(app, conn, req, path, None).await,
        "/api/diary" => return bare(app, req, path).await,
        "/api/diary/context" => return context(app, conn, req, path).await,
        _ => {}
    }
    if let Some(id) = path
        .strip_prefix("/api/profile/diary-connectors/")
        .filter(|id| !id.contains('/') && !id.is_empty())
    {
        let id = id.to_owned();
        return connectors(app, conn, req, path, Some(id)).await;
    }
    // The route table matches a trailing slash; core's `p === '/api/diary/...'` does not.
    let Some(route) = Route::from_path(&path) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    proxied(app, req, path, route).await
}

/// `/api/diary`: no handler in Node. Past the gate it is the unknown-path answer.
async fn bare(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, _body) = req.into_parts();
    match gate(app, &parts, &path).await {
        Ok(_) => error(404, "not found"),
        Err(rejection) => *rejection,
    }
}

/// The routes crates/server-diary answers from the sidecar and the person's files.
async fn proxied(app: &Arc<App>, req: Request<Body>, path: String, route: Route) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let (id, data_dir, _now) = match gate(app, &parts, &path).await {
        Ok(g) => g,
        Err(rejection) => return *rejection,
    };
    if let Some(refused) = claim_pending(&data_dir, &id.user.id).await {
        return refused;
    }
    let Some(diary) = app.diary.clone() else {
        return unavailable();
    };
    let layer = Arc::clone(&app.identity);
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
        latch: Some(Arc::new(NativeLatch {
            data_dir: data_dir.clone(),
        })),
        start_id: app.config.start_id.clone(),
        data_dir,
    };
    into_response(diary.serve(route, ctx, request, body).await)
}

/// `GET` and `POST /api/diary/context`.
async fn context(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let create = match *req.method() {
        Method::GET => false,
        Method::POST => true,
        // Neither: core's route matches nothing and the unknown-path answer follows the gate, which
        // routes.toml (GET and POST only) keeps from reaching here.
        _ => return app.proxy.forward(conn.peer, req).await,
    };
    let (parts, _body) = req.into_parts();
    let (id, data_dir, now) = match gate(app, &parts, &path).await {
        Ok(g) => g,
        Err(rejection) => return *rejection,
    };
    // The project routes' account claim: Node makes the person's directory on their first request.
    let creds = identity::creds_of(&parts);
    if let Err(response) = crate::project_routes::ensure_account(
        app,
        &data_dir,
        &creds,
        &path,
        now,
        &id.user.id,
        id.account_role == server_auth::Role::Admin,
    )
    .await
    {
        return *response;
    }
    if !id.user.diary_enabled {
        return error(404, "Diary add-on is disabled");
    }
    let switch = app.config.rust_projects;
    let user = id.user.id.clone();
    let answered = tokio::task::spawn_blocking(move || {
        server_projects::diary_context::serve(
            server_projects::Ctx {
                data_dir: &data_dir,
                user_id: &user,
                switch,
                now_ms: now,
            },
            create,
        )
    })
    .await;
    match answered {
        Ok(r) => crate::project_routes::respond(r),
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

enum Read {
    TooLarge,
    Broken,
}

/// A request body of at most `limit` bytes, before the deadline.
async fn read_limited(body: Body, limit: usize) -> Result<Vec<u8>, Read> {
    match tokio::time::timeout(BODY_DEADLINE, crate::account_routes::read_body(body, limit)).await {
        Ok(Ok(b)) if b.over => Err(Read::TooLarge),
        Ok(Ok(b)) => Ok(b.bytes),
        _ => Err(Read::Broken),
    }
}

/// `requireJsonObject(readJson)`: an empty body is `{}`; anything but an object is a 400.
async fn read_object(body: Body, limit: usize) -> Result<JValue, Box<Response<Body>>> {
    let bytes = match read_limited(body, limit).await {
        Ok(b) => b,
        Err(Read::TooLarge) => return Err(Box::new(error(413, "Request exceeds size limit"))),
        Err(Read::Broken) => return Err(Box::new(error(500, "Internal error"))),
    };
    let text = String::from_utf8_lossy(&bytes);
    let parsed = if text.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        match wtf16::parse(&text) {
            Ok(v) => v,
            Err(wtf16::Refusal::Invalid) => return Err(Box::new(error(400, "invalid JSON"))),
            // JSON.parse accepts it; this port cannot hold it.
            Err(wtf16::Refusal::Unsupported) => return Err(Box::new(error(500, "Internal error"))),
        }
    };
    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(Box::new(error(400, "request body must be a JSON object")))
    }
}

fn fault_response(fault: &Fault) -> Response<Body> {
    let (status, text) = fault.reply();
    json_text(status, text)
}

/// `/api/profile/diary-connectors[/{id}]`: list, create and revoke the credentials.
async fn connectors(
    app: &Arc<App>,
    _conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
    id: Option<String>,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let (identity, _data_dir, now) = match gate(app, &parts, &path).await {
        Ok(g) => g,
        Err(rejection) => return *rejection,
    };
    let user = identity.user.id.clone();
    let layer = Arc::clone(&app.identity);
    let method = parts.method.clone();
    match (&method, id) {
        (&Method::GET, None) => {
            let listed = tokio::task::spawn_blocking(move || {
                layer
                    .store_blocking()
                    .and_then(|store| connector::list(&store, &user))
            })
            .await;
            match listed {
                Ok(Ok(rows)) => {
                    let body = JValue::obj([(
                        "connectors",
                        JValue::Arr(rows.iter().map(connector::Listed::json).collect()),
                    )]);
                    json_text(200, wtf16::stringify(&body).unwrap_or_default())
                }
                _ => error(500, "Internal error"),
            }
        }
        (&Method::POST, None) => {
            let parsed = match read_object(body, PROFILE_BODY).await {
                Ok(v) => v,
                Err(response) => return *response,
            };
            let writes = Arc::clone(&app.writes);
            let created = tokio::task::spawn_blocking(move || {
                let writer = writes
                    .writer()
                    .map_err(|_| Fault::new(500, "Internal error"))?;
                connector::create(&writer, &user, parsed.get("name"), now)
            })
            .await;
            match created {
                Ok(Ok(Ok(made))) => {
                    let body = JValue::obj([
                        ("id", JValue::Str(made.id.clone())),
                        ("token", JValue::Str(made.token.to_string())),
                    ]);
                    json_text(201, wtf16::stringify(&body).unwrap_or_default())
                }
                Ok(Ok(Err(refusal))) => fault_response(&refusal.fault()),
                Ok(Err(fault)) => fault_response(&fault),
                Err(_) => error(500, "Internal error"),
            }
        }
        (&Method::DELETE, Some(id)) if connector::id_shaped(&id) => {
            let writes = Arc::clone(&app.writes);
            let revoked = tokio::task::spawn_blocking(move || {
                let writer = writes.writer()?;
                connector::revoke(&writer, &user, &id, now)
            })
            .await;
            match revoked {
                Ok(Ok(revoked)) => json_text(
                    200,
                    wtf16::stringify(&JValue::obj([("revoked", JValue::Bool(revoked))]))
                        .unwrap_or_default(),
                ),
                _ => error(500, "Internal error"),
            }
        }
        // core's mount matches nothing else: the unknown-path answer.
        _ => error(404, "not found"),
    }
}

/// `Authorization: Bearer <token>` as core reads it: the first header, one leading `Bearer ` taken
/// off, anything else left as it is (and then not a credential).
fn bearer(parts: &Parts) -> String {
    let raw = parts
        .headers
        .get(header::AUTHORIZATION)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    raw.strip_prefix("Bearer ").unwrap_or(&raw).to_owned()
}

/// `POST /api/diary-connector`: a connected app's credential, checked here. It takes no session and
/// refuses any browser origin; core mounted it before the session and CSRF checks.
async fn connector_endpoint(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    if parts.method != Method::POST {
        return error(405, "POST required");
    }
    // `if (req.headers.origin)`: a header with no value is no origin.
    if parts
        .headers
        .get(header::ORIGIN)
        .is_some_and(|v| !v.is_empty())
    {
        return error(403, "Use the authenticated connector client");
    }
    let Some(data_dir) = app.config.data_dir.clone() else {
        return identity::Rejection::Unavailable.into_response();
    };
    let Some(diary) = app.diary.clone() else {
        return unavailable();
    };
    let token = bearer(&parts);
    let layer = Arc::clone(&app.identity);
    let (verify_token, verify_layer) = (token.clone(), Arc::clone(&layer));
    let verified = tokio::task::spawn_blocking(move || {
        verify_layer
            .store_blocking()
            .and_then(|store| connector::verify(&store, &verify_token).map(|v| (store, v)))
    })
    .await;
    let (store, who) = match verified {
        Ok(Ok((store, Some(who)))) => (store, who),
        Ok(Ok((_, None))) => {
            // Behind the tunnel with TRUST_PROXY off every client shares one address, so the address
            // alone is no bucket key (#796): unknown or malformed credentials go to a bucket of
            // their own (per address only when the address is real) that can never lock out a
            // working connector.
            let key = if app.config.trust_proxy {
                format!(
                    "diary-connector:invalid:{}",
                    crate::legacy_proxy::client_address(true, conn.peer.ip(), &parts.headers)
                )
            } else {
                "diary-connector:invalid".to_owned()
            };
            if app.connector_rate(&key, INVALID_LIMIT) {
                return error(429, "Try later");
            }
            return error(401, "Diary connector credential required");
        }
        _ => return error(500, "Internal error"),
    };
    if app.connector_rate(
        &format!("diary-connector:credential:{}", who.id),
        CREDENTIAL_LIMIT,
    ) {
        return error(429, "Try later");
    }
    let parsed = match read_object(body, CONNECTOR_BODY).await {
        Ok(v) => v,
        Err(response) => return *response,
    };
    // The audit entry travels with the write into the detached task that makes it, not after it
    // here: a client that disconnects once the sidecar has the write drops this handler's future,
    // and an entry made after the await would never be made.
    let audit: Option<Box<dyn FnOnce() + Send>> = (parsed.get("action").as_str() == Some("write"))
        .then(|| {
            let (writes, who, path) = (
                Arc::clone(&app.writes),
                who.clone(),
                parsed.get("path").clone(),
            );
            let bytes = parsed
                .get("content")
                .as_str()
                .map_or(0, connector::byte_length);
            Box::new(move || audit_write(&writes, &who, &path, bytes)) as Box<dyn FnOnce() + Send>
        });
    match diary
        .operate_connector(store, data_dir, token, who.clone(), parsed, audit)
        .await
    {
        Ok(result) => json_text(
            200,
            wtf16::stringify(&result).unwrap_or_else(|| "null".to_owned()),
        ),
        Err(fault) => fault_response(&fault),
    }
}

/// The connector's write, in the audit trail: who, which path, how many bytes; never the content. The
/// write has happened by now, so a failure here is logged, not turned into an error answer that would
/// make the connector retry a write that went through. Blocking (SQLite): run it off the async threads.
fn audit_write(
    writes: &Arc<crate::upkeep::WriterLayer>,
    who: &Verified,
    path: &JValue,
    bytes: usize,
) {
    let done = writes
        .writer()
        .and_then(|writer| connector::audit_write(&writer, who, path, bytes, identity::now_ms()));
    if done.is_err() {
        eprintln!(
            "noevia-server: a Diary connector write could not be recorded in the audit trail"
        );
    }
}
