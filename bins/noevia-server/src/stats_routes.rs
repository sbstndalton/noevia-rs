//! Native `GET /api/stats` (M6, sbstndalton/noevia#1650): the engine statistics every open page
//! polls (crates/server-stats). Node's session gate first (signed out 401; any signed-in member or
//! admin is answered, as in Node), then the shared answer: concurrent polls share one engine read,
//! and it is kept for 2 s after it lands (routes/models.cjs `statsRead`). Only GET is owned
//! (routes.toml); every other method, a trailing-slash path and every request while
//! NOEVIA_RUST_STATS is off go to Node.
//!
//! A write Node would let through to its model routes (any method but GET/HEAD under
//! `/api/models`, `/api/model-manager`, `/api/auto-roles`, past the session and CSRF gate) drops
//! the shared answer before it is proxied, as Node's handler drops its own before the write runs;
//! a request the gate refuses drops nothing, so a signed-out client cannot force engine reads.
//!
//! The engine address is configuration (config.rs); nothing from the request reaches it.
use crate::{identity, reply, App};
use axum::{
    body::Body,
    http::{Method, Request, Response, StatusCode},
    response::IntoResponse,
};
use server_stats::{Engine, Reply, SharedRead, STATS_TTL};
use std::sync::Arc;
use tokio::sync::Semaphore;

const ROUTE: &str = "/api/stats";

/// Identity checks (blocking SQLite reads) in flight at once; a poll past that is refused with
/// 503 rather than queued (every open page polls every 2-3 s and simply asks again).
const GATE_PERMITS: usize = 32;

pub struct StatsLayer {
    engine: Arc<Engine>,
    read: SharedRead<Arc<Reply>>,
    gate: Arc<Semaphore>,
}

impl StatsLayer {
    pub fn new(engine: Arc<Engine>) -> Self {
        StatsLayer {
            engine,
            read: SharedRead::new(STATS_TTL),
            gate: Arc::new(Semaphore::new(GATE_PERMITS)),
        }
    }

    /// The shared answer (or a fresh one when there is none in flight or still fresh).
    pub async fn answer(&self) -> Option<Arc<Reply>> {
        let engine = Arc::clone(&self.engine);
        self.read
            .get(move || async move { Arc::new(engine.read().await) })
            .await
    }

    /// Drops the shared answer.
    pub fn clear(&self) {
        self.read.clear();
    }
}

fn busy() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Engine statistics are busy. Try again shortly.",
        true,
    )
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let Some(stats) = app.stats.as_ref().map(Arc::clone) else {
        return app.proxy.forward(conn.peer, req).await;
    };
    // Node matches the exact pathname only; routes.toml matching also accepts a trailing slash.
    if path != ROUTE || req.method() != Method::GET {
        return app.proxy.forward(conn.peer, req).await;
    }
    let (parts, _body) = req.into_parts();
    let Ok(permit) = Arc::clone(&stats.gate).try_acquire_owned() else {
        return busy();
    };
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gated = tokio::task::spawn_blocking(move || {
        let _p = permit;
        layer.gate_blocking(&creds, ROUTE, now)
    })
    .await;
    match gated {
        Ok(Ok(_identity)) => {}
        Ok(Err(r)) => return r.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    }
    match stats.answer().await {
        Some(r) => reply::json_text(
            StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            r.body.clone(),
            true,
        ),
        None => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

/// A model or role write the shared answer may have to be dropped for: Node drops it only for a
/// write its gate lets through (see the module documentation), so [`Invalidation::run`] runs the
/// same gate first.
pub struct Invalidation {
    stats: Arc<StatsLayer>,
    creds: server_auth::Creds,
    path: String,
}

/// Called for every request before it is dispatched (synchronously, so no borrow of the request is
/// held across an await).
pub fn invalidation(app: &App, req: &Request<Body>, whatwg: Option<&str>) -> Option<Invalidation> {
    let stats = app.stats.as_ref()?;
    let path = whatwg?;
    if !server_stats::write_invalidates(req.method().as_str(), path) {
        return None;
    }
    Some(Invalidation {
        stats: Arc::clone(stats),
        creds: server_auth::Creds::from_headers(
            req.method().as_str(),
            req.headers()
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_bytes())),
        ),
        path: path.to_string(),
    })
}

impl Invalidation {
    pub async fn run(self, app: &Arc<App>) {
        let layer = Arc::clone(&app.identity);
        let (creds, path) = (self.creds, self.path);
        let now = identity::now_ms();
        let passed = tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, now))
            .await
            .is_ok_and(|r| r.is_ok());
        if passed {
            self.stats.clear();
        }
    }
}
