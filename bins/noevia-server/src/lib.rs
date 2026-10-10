//! noevia-server: the Rust front (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
//! "Amendment 2026-10-10" in sbstndalton/noevia). It owns the routes contracts/http/routes.toml
//! gives to Rust and proxies every other request to Node ([`legacy_proxy`], deleted with Node).

pub mod account_routes;
pub mod code_net;
pub mod config;
pub mod health;
pub mod identity;
pub mod legacy_proxy;
pub mod project_routes;
pub mod reply;
pub mod routes;
pub mod serve;
pub mod static_files;
pub mod upkeep;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use std::sync::Arc;

/// What `noevia-server --features` lists, one per line.
/// `rust-auth`: this build answers sign-in and the account under NOEVIA_RUST_AUTH (M3); the web
/// supervisor should let Node refuse the account tables only when the front reports it.
/// `rust-projects`: this build answers a project's image routes under NOEVIA_RUST_PROJECTS (M4)
/// and writes projects.json under the lock Node shares; the supervisor confirms it to Node the
/// same way.
pub const FEATURES: &[&str] = &[
    "code-net-guard",
    "header-read-timeout",
    "rust-auth",
    "rust-projects",
];

pub struct App {
    pub config: config::Config,
    pub proxy: legacy_proxy::LegacyProxy,
    pub statics: static_files::StaticFiles,
    pub version: Option<String>,
    pub code_net: Arc<code_net::CodeNetGuard>,
    /// The validated identity for Rust-owned routes.
    pub identity: Arc<identity::IdentityLayer>,
    /// The cowork.db writer while NOEVIA_RUST_AUTH is on (M3).
    pub writes: Arc<upkeep::WriterLayer>,
    /// The account routes while NOEVIA_RUST_AUTH is on (M3).
    pub accounts: Arc<account_routes::AccountLayer>,
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
        let writes = Arc::new(upkeep::WriterLayer::new(&config));
        let accounts = Arc::new(account_routes::AccountLayer::new(&config));
        Arc::new(App {
            identity,
            writes,
            accounts,
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
    let switches = app.config.switches();
    let mut native = routes::dispatch_under(req.method().as_str(), &path, switches);
    // Node routes on the WHATWG pathname (`new URL(req.url, base)`): a Rust-owned switched route
    // is matched on it too, so "/api/x/../auth/session" is the same route as "/api/auth/session".
    let whatwg = identity::whatwg_pathname_of(req.uri());
    if switches.rust_auth {
        if let Some(w) = whatwg.as_deref().filter(|w| *w != path) {
            let on_whatwg = routes::dispatch_under(req.method().as_str(), w, switches);
            if matches!(
                on_whatwg,
                Some(routes::Native::Account | routes::Native::ProjectAssets)
            ) {
                native = on_whatwg;
            }
        }
    }
    let served_bundle = match native {
        Some(routes::Native::Ready) => true,
        Some(routes::Native::Static) => static_files::canonical(&path),
        Some(routes::Native::Account | routes::Native::ProjectAssets) | None => false,
    };
    if app.writes.switch().is_some() && !served_bundle {
        // Node's gate wrote these on every request it saw; with NOEVIA_RUST_AUTH it no longer can.
        let creds = server_auth::Creds::from_headers(
            req.method().as_str(),
            req.headers()
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_bytes())),
        );
        let (identity, writes) = (Arc::clone(&app.identity), Arc::clone(&app.writes));
        let now = identity::now_ms();
        let _ = tokio::task::spawn_blocking(move || {
            writes.upkeep_blocking(identity.authenticator(), &creds, now)
        })
        .await;
    }
    match native {
        Some(routes::Native::Account) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            account_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::ProjectAssets) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            project_routes::serve(&app, conn, req, p).await
        }
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
