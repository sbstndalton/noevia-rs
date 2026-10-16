//! Native `GET /api/stats` (M6, sbstndalton/noevia#1650): the engine statistics every open page
//! polls (crates/server-stats). Node's session gate first (signed out 401; any signed-in member or
//! admin is answered, as in Node), then the shared answer: concurrent polls share one engine read,
//! and it is kept for 2 s after it lands (routes/models.cjs `statsRead`). Only GET is owned
//! (routes.toml); every other method, a trailing-slash path and every request without
//! NOEVIA_RUST_AUTH=1 go to Node (its old switch NOEVIA_RUST_STATS is retired, #1071).
//!
//! A write Node would let through to its model routes (any method but GET/HEAD under
//! `/api/models`, `/api/model-manager`, `/api/auto-roles`, past the session and CSRF gate) drops
//! the shared answer before it is proxied, as Node's handler drops its own before the write runs;
//! a request the gate refuses drops nothing, so a signed-out client cannot force engine reads.
//! Those gate reads are bounded too ([`INVALIDATION_PERMITS`]); past the bound the drop is skipped,
//! which costs at most one answer up to [`STATS_TTL`] old, never an engine read.
//!
//! A request with no credential at all (no `cowork_session` cookie, no Authorization header) gets
//! Node's 401 before it takes a gate permit, so signed-out polls can neither see 503 nor use up the
//! permits signed-in polls need.
//!
//! HEAD stays Node's on purpose (sbstndalton/noevia#1654): nothing in the app sends it, Node's gate
//! refuses it signed out, and owning it would add a second contract for no caller.
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

/// Gate reads in flight at once for model and role writes (see the module documentation).
pub const INVALIDATION_PERMITS: usize = 4;

pub struct StatsLayer {
    engine: Arc<Engine>,
    read: SharedRead<Arc<Reply>>,
    gate: Arc<Semaphore>,
    invalidation_gate: Arc<Semaphore>,
}

impl StatsLayer {
    pub fn new(engine: Arc<Engine>) -> Self {
        StatsLayer {
            engine,
            read: SharedRead::new(STATS_TTL),
            gate: Arc::new(Semaphore::new(GATE_PERMITS)),
            invalidation_gate: Arc::new(Semaphore::new(INVALIDATION_PERMITS)),
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

    /// Takes every gate permit (poll and invalidation) until the returned guards drop: what a
    /// burst of slow identity reads looks like, for tests.
    #[doc(hidden)]
    pub fn hold_every_permit(&self) -> Vec<tokio::sync::OwnedSemaphorePermit> {
        [&self.gate, &self.invalidation_gate]
            .into_iter()
            .filter_map(|g| {
                let n = u32::try_from(g.available_permits()).ok()?;
                Arc::clone(g).try_acquire_many_owned(n).ok()
            })
            .collect()
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
    let creds = identity::creds_of(&parts);
    if app.identity.authenticator().is_some() && !server_auth::request::could_authenticate(&creds) {
        return identity::Rejection::Refused(server_auth::Refusal::Unauthorized).into_response();
    }
    let Ok(permit) = Arc::clone(&stats.gate).try_acquire_owned() else {
        return busy();
    };
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
        if !server_auth::request::could_authenticate(&self.creds) {
            return;
        }
        let Ok(permit) = Arc::clone(&self.stats.invalidation_gate).try_acquire_owned() else {
            return;
        };
        let layer = Arc::clone(&app.identity);
        let (creds, path) = (self.creds, self.path);
        let now = identity::now_ms();
        let passed = tokio::task::spawn_blocking(move || {
            let _p = permit;
            layer.gate_blocking(&creds, &path, now)
        })
        .await
        .is_ok_and(|r| r.is_ok());
        if passed {
            self.stats.clear();
        }
    }
}
