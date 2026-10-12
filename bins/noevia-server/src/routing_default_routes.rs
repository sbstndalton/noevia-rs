//! Native `GET /api/routing-default` (M6, sbstndalton/noevia#1305): the signed-in user's default
//! routing, read from their own preferences.json (crates/server-routing-default). Identity
//! precedes everything; only GET is owned (routes.toml), so every other method, and every request
//! while NOEVIA_RUST_ROUTING_DEFAULT is off, goes to Node. A failed or unsafe read answers 503,
//! never a guessed routing and never a fallback to Node.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use server_routing_default::{Fault, Root, Routing};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const ROUTE: &str = "/api/routing-default";

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
