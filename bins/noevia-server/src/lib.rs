//! noevia-server: the Rust front (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
//! "Amendment 2026-10-10" in sbstndalton/noevia). It owns the routes contracts/http/routes.toml
//! gives to Rust and proxies every other request to Node ([`legacy_proxy`], deleted with Node).

pub mod code_net;
pub mod config;
pub mod health;
pub mod identity;
pub mod legacy_proxy;
pub mod reply;
pub mod routes;
pub mod serve;
pub mod static_files;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use std::sync::Arc;

/// What `noevia-server --features` lists, one per line.
pub const FEATURES: &[&str] = &["code-net-guard", "header-read-timeout"];

pub struct App {
    pub config: config::Config,
    pub proxy: legacy_proxy::LegacyProxy,
    pub statics: static_files::StaticFiles,
    pub version: Option<String>,
    pub code_net: Arc<code_net::CodeNetGuard>,
    /// The validated identity for Rust-owned routes; no route uses it in M2.
    pub identity: Arc<identity::IdentityLayer>,
}

impl App {
    pub fn new(config: config::Config) -> Arc<Self> {
        Self::with_lookup(config, code_net::system_lookup())
    }

    /// [`App::new`] with the code-network guard's host lookups made by `lookup`.
    pub fn with_lookup(config: config::Config, lookup: code_net::Lookup) -> Arc<Self> {
        let code_net = code_net::CodeNetGuard::new(&config.code_net, lookup);
        let version = health::local_version(&config.dist, config.stamp_version.as_deref());
        let identity = Arc::new(identity::IdentityLayer::new(&config));
        Arc::new(App {
            identity,
            proxy: legacy_proxy::LegacyProxy::new(config.upstream.clone(), config.trust_proxy),
            statics: static_files::StaticFiles::new(config.dist.clone()),
            version,
            code_net,
            config,
        })
    }
}

async fn handle(
    State(app): State<Arc<App>>,
    ConnectInfo(conn): ConnectInfo<serve::Conn>,
    req: Request<Body>,
) -> Response<Body> {
    // Before any dispatch (static, ready, proxy): a request that arrived on web's own
    // code-network address gets 403 and nothing else (#1246). No local address: refuse.
    if app.code_net.enabled() {
        let refused = match conn.local {
            Some(local) => app.code_net.refuses(local).await,
            None => true,
        };
        if refused {
            return app.code_net.deny();
        }
    }
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
        _ => app.proxy.forward(conn.peer, req).await,
    }
}

/// The front's router; serve it with [`serve::serve`] (or axum's
/// `into_make_service_with_connect_info::<serve::Conn>()`, which has no timeouts).
pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}
