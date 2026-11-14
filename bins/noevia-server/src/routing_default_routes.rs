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

/// What the projects pass ended in.
enum Projects {
    Updated(usize),
    /// projects.json is not a list Node can read: Node loads it as empty and changes nothing.
    Unreadable,
}

/// `PUT /api/routing-default` (NOEVIA_RUST_SETTINGS_ROUTES=1), in core models.cjs's order.
async fn put(app: &Arc<App>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let Ok(permit) = Arc::clone(&app.routing_default).try_acquire_owned() else {
        return unavailable();
    };
    let permit = Arc::new(permit);
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
    // readBody inside JSON.parse's try: a body that fails to arrive is the same `invalid JSON`.
    let (bytes, over) = match tokio::time::timeout(BODY_DEADLINE, bounded_body(body)).await {
        Ok(Ok(read)) => read,
        _ => return refused(put::Refusal::InvalidJson),
    };
    let request = match put::parse(&bytes, over) {
        Ok(r) => r,
        Err(refusal) => return refused(refusal),
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        return unavailable();
    };
    let switch = app.config.rust_projects;
    let user = id.user.id;
    // Not cancelled with the request: once the preferences are written the projects pass runs too.
    let done = tokio::task::spawn_blocking(move || -> Result<Projects, Box<Response<Body>>> {
        let _p = permit;
        // workspace.cjs `get` makes the account's directory before any route runs.
        let dir = match server_projects::store::ensure_user_dir(&data_dir, &user) {
            Ok(
                server_projects::store::Ensured::Existed(d)
                | server_projects::store::Ensured::Created(d),
            ) => d,
            _ => return Err(Box::new(unavailable())),
        };
        Root::open(&data_dir)
            .and_then(|root| root.write_preferences(&user, request.routing))
            .map_err(|_| Box::new(unavailable()))?;
        if !request.apply_to_existing {
            return Ok(Projects::Updated(0));
        }
        let changed = server_projects::store::update(&dir, switch, |projects| {
            match put::apply(projects, request.routing, now) {
                Ok(n) => (Ok(n), n > 0),
                Err(put::NotAProject) => (Err(()), false),
            }
        });
        match changed {
            Ok(Ok(n)) => Ok(Projects::Updated(n)),
            Ok(Err(())) => Err(Box::new(reply::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
                true,
            ))),
            Err(server_projects::store::StoreError::Corrupt) if unreadable(&dir) => {
                Ok(Projects::Unreadable)
            }
            Err(server_projects::store::StoreError::Gone) => Err(Box::new(reply::error(
                StatusCode::GONE,
                "account no longer exists",
                true,
            ))),
            Err(_) => Err(Box::new(unavailable())),
        }
    })
    .await;
    let updated = match done {
        Ok(Ok(Projects::Updated(n))) => n,
        Ok(Ok(Projects::Unreadable)) => 0,
        Ok(Err(response)) => return *response,
        Err(_) => return unavailable(),
    };
    reply::json_text(StatusCode::OK, put::answer(request.routing, updated), true)
}

/// Whether `dir/projects.json` is one Node's load reads as an empty list (`JSON.parse` throws, or
/// `projects` is there but not a list), rather than one this port refuses to hold (a number past
/// the `f64` range, deep nesting), which fails closed.
fn unreadable(dir: &std::path::Path) -> bool {
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
