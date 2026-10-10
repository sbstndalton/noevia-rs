//! noevia-server: the Rust front (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
//! "Amendment 2026-10-10" in sbstndalton/noevia). It owns the routes contracts/http/routes.toml
//! gives to Rust and proxies every other request to Node ([`legacy_proxy`], deleted with Node).

pub mod config;
pub mod health;
pub mod legacy_proxy;
pub mod reply;
pub mod routes;
pub mod static_files;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct App {
    pub config: config::Config,
    pub proxy: legacy_proxy::LegacyProxy,
    pub statics: static_files::StaticFiles,
    pub version: Option<String>,
}

impl App {
    pub fn new(config: config::Config) -> Arc<Self> {
        let version = health::local_version(&config.dist, config.stamp_version.as_deref());
        Arc::new(App {
            proxy: legacy_proxy::LegacyProxy::new(config.upstream.clone(), config.trust_proxy),
            statics: static_files::StaticFiles::new(config.dist.clone()),
            version,
            config,
        })
    }
}

async fn handle(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response<Body> {
    let path = req.uri().path().to_string();
    match routes::dispatch(req.method().as_str(), &path) {
        Some(routes::Native::Ready) => {
            health::ready(&app.config.upstream, app.version.as_deref()).await
        }
        Some(routes::Native::Static) if static_files::canonical(&path) => {
            let statics = app.statics.clone();
            let (method, headers) = (req.method().clone(), req.headers().clone());
            // File reads and first-time compression are blocking work.
            tokio::task::spawn_blocking(move || statics.serve(&method, &headers, &path))
                .await
                .unwrap_or_else(|_| {
                    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "read error", false)
                })
        }
        _ => app.proxy.forward(peer, req).await,
    }
}

/// The front's router; serve it with `into_make_service_with_connect_info::<SocketAddr>()`.
pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}
