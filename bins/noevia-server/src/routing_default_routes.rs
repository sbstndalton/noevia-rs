//! Native `GET /api/routing-default` (M6, sbstndalton/noevia#1305): the signed-in user's default
//! routing, read from their own preferences.json (crates/server-routing-default). Identity
//! precedes everything; only GET is owned (routes.toml), so every other method goes to Node
//! (NOEVIA_RUST_ROUTING_DEFAULT is retired, #1071). A failed or unsafe read answers 503, never a
//! guessed routing and never a fallback to Node.
//!
//! Track R12: with NOEVIA_RUST_SETTINGS_ROUTES=1 the dispatch (lib.rs) also hands the PUT here, and
//! it is answered as core models.cjs does (server-routing-default `put`): the preferences write, then,
//! with `applyToExisting: true`, each project's routing under the shared projects.json lock
//! (server-projects `store::update`). Unset, the PUT is proxied to Node as before.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use server_routing_default::{put, Fault, Root, Routing};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub const ROUTE: &str = "/api/routing-default";
/// How long the PUT's body may take to arrive.
const BODY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Cancels the blocking read when the request future is dropped (client went away).
struct Cancel(Arc<AtomicBool>);

impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Routing default unavailable",
        true,
    )
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    if path == ROUTE && req.method() == Method::PUT && app.config.settings_routes {
        return put(app, req).await;
    }
    if path != ROUTE || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let Ok(permit) = Arc::clone(&app.routing_default).try_acquire_owned() else {
        return unavailable();
    };
    // Copies retained by each blocking operation keep admission until their work has ended, even
    // if this request future is cancelled first.
    let permit = Arc::new(permit);
    let cancel = Cancel(Arc::new(AtomicBool::new(false)));
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let auth_permit = Arc::clone(&permit);
    let now = identity::now_ms();
    let id = match tokio::task::spawn_blocking(move || {
        let _p = auth_permit;
        layer.gate_blocking(&creds, ROUTE, now)
    })
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(r)) => return r.into_response(),
        _ => return identity::Rejection::Unavailable.into_response(),
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        return unavailable();
    };
    let user = id.user.id;
    let c = Arc::clone(&cancel.0);
    let result = tokio::task::spawn_blocking(move || -> Result<Routing, Fault> {
        let _p = permit;
        Root::open(&data_dir)?.read(&user, &c)
    })
    .await;
    match result {
        Ok(Ok(routing)) => reply::json_text(
            StatusCode::OK,
            format!("{{\"routing\":\"{}\"}}", routing.as_str()),
            true,
        ),
        _ => unavailable(),
    }
}

/// The body up to [`put::BODY_LIMIT`] bytes and whether it went past it (the rest is not read).
async fn bounded_body(mut body: Body) -> Result<(Vec<u8>, bool), ()> {
    use http_body_util::BodyExt;
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.map_err(|_| ())?.into_data() {
            let room = put::BODY_LIMIT + 1 - out.len();
            let take = room.min(data.len());
            out.try_reserve(take).map_err(|_| ())?;
            out.extend_from_slice(data.get(..take).ok_or(())?);
            if out.len() > put::BODY_LIMIT {
                out.truncate(put::BODY_LIMIT);
                return Ok((out, true));
            }
        }
    }
    Ok((out, false))
}

fn refused(refusal: put::Refusal) -> Response<Body> {
    reply::error(StatusCode::BAD_REQUEST, refusal.message(), true)
}

/// `PUT /api/routing-default` (NOEVIA_RUST_SETTINGS_ROUTES=1), in core models.cjs's order.
async fn put(app: &Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let Some(data_dir) = app.config.data_dir.clone() else {
        return unavailable();
    };
    let creds = identity::creds_of(&parts);
    let now = identity::now_ms();
    let id = {
        // Admission covers the gate only: a slow body never holds a permit the GET needs.
        let Ok(permit) = Arc::clone(&app.routing_default).try_acquire_owned() else {
            return unavailable();
        };
        let (layer, creds) = (Arc::clone(&app.identity), creds.clone());
        match tokio::task::spawn_blocking(move || {
            let _p = permit;
            layer.gate_blocking(&creds, ROUTE, now)
        })
        .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(r)) => return r.into_response(),
            _ => return identity::Rejection::Unavailable.into_response(),
        }
    };
    let user = id.user.id.clone();
    let admin = id.user.role == server_auth::Role::Admin;
    // workspace.cjs `get` makes the account's directory before any route runs; the front's fence makes it
    // once, gates again, and takes it away if the account went meanwhile (#1425, #2026).
    if let Err(response) =
        crate::project_routes::ensure_account(app, &data_dir, &creds, ROUTE, now, &user, admin)
            .await
    {
        return *response;
    }
    // readBody inside JSON.parse's try: a body that fails to arrive is the same `invalid JSON`.
    let (bytes, over) = match tokio::time::timeout(BODY_DEADLINE, bounded_body(body)).await {
        Ok(Ok(read)) => read,
        _ => return refused(put::Refusal::InvalidJson),
    };
    let request = match put::parse(&bytes, over) {
        Ok(r) => r,
        Err(refusal) => return refused(refusal),
    };
    let Ok(permit) = Arc::clone(&app.routing_default).try_acquire_owned() else {
        return unavailable();
    };
    let switch = app.config.rust_projects;
    // Not cancelled with the request: once it starts, the write runs to its end.
    let done = tokio::task::spawn_blocking(move || -> Result<usize, Box<Response<Body>>> {
        let _p = permit;
        write(&data_dir, &user, request, switch)
    })
    .await;
    match done {
        Ok(Ok(updated)) => {
            reply::json_text(StatusCode::OK, put::answer(request.routing, updated), true)
        }
        Ok(Err(response)) => *response,
        Err(_) => unavailable(),
    }
}

fn gone() -> Box<Response<Body>> {
    Box::new(reply::error(
        StatusCode::GONE,
        "account no longer exists",
        true,
    ))
}

fn internal() -> Box<Response<Body>> {
    Box::new(reply::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal error",
        true,
    ))
}

/// The preferences write and, with `applyToExisting`, the projects pass. With the pass, the
/// preferences are written under the projects.json lock, after the list was read and checked, so two
/// PUTs of one account land one after the other as in Node (whose handler runs without a pause from
/// `savePreferences` to the loop). Returns the number of projects changed.
fn write(
    data_dir: &std::path::Path,
    user: &str,
    request: put::Put,
    switch: server_store::RustProjects,
) -> Result<usize, Box<Response<Body>>> {
    let dir = server_projects::store::user_dir(data_dir, user).map_err(|_| internal())?;
    let preferences = || -> Result<(), Box<Response<Body>>> {
        // The account was removed after the fence: Node's `assertActive`, a 410; nothing is made.
        if !dir.is_dir() {
            return Err(gone());
        }
        Root::open(data_dir)
            .and_then(|root| root.write_preferences(user, request.routing))
            .map_err(|_| {
                // Removed between the check and the write: still the 410.
                if dir.is_dir() {
                    Box::new(unavailable())
                } else {
                    gone()
                }
            })
    };
    if !request.apply_to_existing {
        preferences()?;
        return Ok(0);
    }
    let changed = server_projects::store::update(&dir, switch, |projects| {
        if projects.iter().any(|p| !p.is_object()) {
            return (Err(internal()), false);
        }
        if let Err(response) = preferences() {
            return (Err(response), false);
        }
        match put::apply(projects, request.routing, identity::now_ms()) {
            Ok(n) => (Ok(n), n > 0),
            Err(put::NotAProject) => (Err(internal()), false),
        }
    });
    match changed {
        Ok(result) => result,
        // A list Node's load reads as empty: the preferences are written and nothing else changes.
        Err(server_projects::store::StoreError::Corrupt) if unreadable(&dir) => {
            preferences()?;
            Ok(0)
        }
        Err(server_projects::store::StoreError::Gone) => Err(gone()),
        Err(_) => Err(Box::new(unavailable())),
    }
}

/// Whether `dir/projects.json` is one Node's load reads as an empty list (`JSON.parse` throws, or
/// `projects` is there but not a list), rather than one this port refuses to hold (a number past
/// the `f64` range, deep nesting), which fails closed.
pub(crate) fn unreadable(dir: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join("projects.json")) else {
        return false;
    };
    match js_json::wtf16::parse(&String::from_utf8_lossy(&bytes)) {
        Err(js_json::wtf16::Refusal::Invalid) => true,
        Err(js_json::wtf16::Refusal::Unsupported) => false,
        Ok(v) => !matches!(
            v.get("projects"),
            js_json::JValue::Arr(_) | js_json::JValue::Undefined | js_json::JValue::Null
        ),
    }
}
