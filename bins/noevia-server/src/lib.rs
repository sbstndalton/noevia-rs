//! noevia-server: the Rust front (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
//! "Amendment 2026-10-10" in sbstndalton/noevia). It owns the routes contracts/http/routes.toml
//! gives to Rust and proxies every other request to Node ([`legacy_proxy`], deleted with Node).

pub mod account_routes;
#[cfg(feature = "rust-chatgpt-status")]
pub mod chatgpt_status_routes;
pub mod code_net;
pub mod config;
pub mod export_routes;
pub mod health;
pub mod identity;
pub mod legacy_proxy;
pub mod project_routes;
pub mod project_store;
pub mod reasoning_settings_routes;
pub mod reply;
pub mod routes;
pub mod routing_default_routes;
pub mod routing_mode_routes;
pub mod sampling_settings_routes;
pub mod serve;
pub mod static_files;
pub mod stats_routes;
#[cfg(feature = "rust-storage-browse")]
pub mod storage_browse_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod storage_metadata_worker;
#[cfg(feature = "rust-storage-reads")]
pub mod storage_read_routes;
pub mod upkeep;
pub mod usage_routes;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use std::sync::Arc;

/// What `noevia-server --features` lists, one per line.
/// `rust-auth`: this build answers sign-in and the account (M3), always (NOEVIA_RUST_AUTH is
/// retired, sbstndalton/noevia#1071); the web supervisor refuses to start without it, and Node
/// always refuses the account tables.
/// `rust-projects`: this build answers a project's image routes under NOEVIA_RUST_PROJECTS (M4)
/// and writes projects.json under the lock Node shares; the supervisor confirms it to Node the same
/// way. `rust-project-chats` (cargo feature of that name): it answers a project's chat list under
/// the same switch too; the supervisor confirms that slice to Node separately
/// (NOEVIA_RUST_PROJECT_CHATS_CONFIRMED), so Node refuses the chat-list save only behind a front
/// that answers it (sbstndalton/noevia#1409). `rust-free-chats` (cargo feature of that name): it
/// answers the free-chat list (`GET`/`POST /api/freechats`) under the same switch and shares
/// free-chats.json with Node under its lock; confirmed to Node separately as well
/// (NOEVIA_RUST_FREE_CHATS_CONFIRMED). `rust-stats` (cargo feature of that name): it answers
/// `GET /api/stats` under NOEVIA_RUST_STATS (#1650); the supervisor confirms it to Node
/// (NOEVIA_RUST_STATS_CONFIRMED), which then refuses that GET. `rust-export` (cargo feature of
/// that name): it answers `GET /api/export/conversations` under NOEVIA_RUST_EXPORT (#1315, with
/// NOEVIA_RUST_PROJECTS and rust-free-chats); the supervisor confirms it to Node
/// (NOEVIA_RUST_EXPORT_CONFIRMED), which then refuses that GET. `rust-chatgpt-status` (cargo
/// feature of that name): it answers `GET /api/providers/chatgpt` under NOEVIA_RUST_CHATGPT_STATUS
/// (#1373); the supervisor confirms it to Node (NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED), which then
/// refuses that GET. `rust-project-writes` (cargo feature of that name): it answers the project
/// writes ported for sbstndalton/noevia#1264 (so far `GET`/`POST /api/chats/{chatId}/context`)
/// under NOEVIA_RUST_PROJECT_WRITES, with NOEVIA_RUST_PROJECTS; the supervisor confirms it to Node
/// (NOEVIA_RUST_PROJECT_WRITES_CONFIRMED), which then refuses those writes.
///
/// `project-store`: this build serves the internal project store Node writes projects.json and
/// free-chats.json through (src/project_store.rs, #1264; permanent, no switch); the
/// supervisor confirms it to Node (NOEVIA_RUST_PROJECT_STORE_CONFIRMED) and hands both the secret.
///
/// `client-address-header`: the front drops any `X-Noevia-Client-Ip` a client sends and stamps its
/// own value on what it forwards (#1245). The web supervisor confirms it to Node
/// (NOEVIA_FRONT_CLIENT_IP=1), and Node believes that header only then, so a core release paired
/// with a front that predates the strip cannot be handed a client-chosen address (#1578).
pub const FEATURES: &[&str] = &[
    "client-address-header",
    "code-net-guard",
    "header-read-timeout",
    "project-store",
    "rust-auth",
    "rust-projects",
    #[cfg(feature = "rust-project-chats")]
    "rust-project-chats",
    #[cfg(feature = "rust-free-chats")]
    "rust-free-chats",
    #[cfg(feature = "rust-project-writes")]
    "rust-project-writes",
    "rust-usage",
    "rust-sampling-settings",
    #[cfg(feature = "rust-storage-reads")]
    "rust-storage-reads",
    #[cfg(feature = "rust-storage-browse")]
    "rust-storage-browse",
    #[cfg(feature = "rust-reasoning-settings")]
    "rust-reasoning-settings",
    #[cfg(feature = "rust-routing-mode")]
    "rust-routing-mode",
    #[cfg(feature = "rust-routing-default")]
    "rust-routing-default",
    #[cfg(feature = "rust-stats")]
    "rust-stats",
    #[cfg(feature = "rust-export")]
    "rust-export",
    #[cfg(feature = "rust-chatgpt-status")]
    "rust-chatgpt-status",
];

pub struct App {
    /// GET /api/providers/chatgpt under NOEVIA_RUST_AUTH (M6, #1373; permanent since #1071).
    #[cfg(feature = "rust-chatgpt-status")]
    pub chatgpt_status: Option<Arc<chatgpt_status_routes::StatusLayer>>,
    #[cfg(feature = "rust-storage-reads")]
    pub storage_metadata: Arc<storage_metadata_worker::Worker>,
    /// Storage list/read admission: four at once, one per account (#1368).
    #[cfg(feature = "rust-storage-browse")]
    pub storage_browse: server_storage_reads::admission::Admission,
    /// This binary, run as the owned outbound child (`--storage-request-child`).
    #[cfg(feature = "rust-storage-browse")]
    pub storage_child: Option<std::path::PathBuf>,
    pub config: config::Config,
    pub proxy: legacy_proxy::LegacyProxy,
    pub statics: static_files::StaticFiles,
    pub version: Option<String>,
    pub code_net: Arc<code_net::CodeNetGuard>,
    /// The validated identity for Rust-owned routes.
    pub identity: Arc<identity::IdentityLayer>,
    /// The cowork.db writer (M3).
    pub writes: Arc<upkeep::WriterLayer>,
    /// The account routes (M3).
    pub accounts: Arc<account_routes::AccountLayer>,
    pub usage: Arc<usage_routes::UsageLayer>,
    pub reasoning_settings: Arc<tokio::sync::Semaphore>,
    pub sampling_settings: Arc<tokio::sync::Semaphore>,
    pub routing_mode: Arc<tokio::sync::Semaphore>,
    pub routing_default: Arc<tokio::sync::Semaphore>,
    /// GET /api/stats and its shared answer under NOEVIA_RUST_AUTH (M6, #1650; permanent since #1071).
    pub stats: Option<Arc<stats_routes::StatsLayer>>,
    /// GET /api/export/conversations runs one export at a time (#1315; export_routes.rs).
    pub export: Arc<tokio::sync::Semaphore>,
    pub routing_files:
        Option<Result<server_routing_mode::storage::Root, server_routing_mode::storage::Fault>>,
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
        let routing_files = config.rust_routing_mode.map(|_| {
            config
                .data_dir
                .as_deref()
                .ok_or(server_routing_mode::storage::Fault::Unavailable)
                .and_then(server_routing_mode::storage::Root::open)
        });
        Arc::new(App {
            #[cfg(feature = "rust-chatgpt-status")]
            chatgpt_status: chatgpt_status_routes::StatusLayer::new(&config).map(Arc::new),
            #[cfg(feature = "rust-storage-reads")]
            storage_metadata: Arc::new(storage_metadata_worker::Worker::default()),
            #[cfg(feature = "rust-storage-browse")]
            storage_browse: server_storage_reads::admission::Admission::default(),
            #[cfg(feature = "rust-storage-browse")]
            storage_child: storage_browse_routes::child_command(),
            identity,
            writes,
            accounts,
            usage: Arc::new(usage_routes::UsageLayer::default()),
            reasoning_settings: Arc::new(tokio::sync::Semaphore::new(4)),
            sampling_settings: Arc::new(tokio::sync::Semaphore::new(4)),
            routing_mode: Arc::new(tokio::sync::Semaphore::new(4)),
            routing_default: Arc::new(tokio::sync::Semaphore::new(4)),
            export: Arc::new(tokio::sync::Semaphore::new(1)),
            stats: config
                .rust_stats
                .as_ref()
                .map(|engine| Arc::new(stats_routes::StatsLayer::new(Arc::clone(engine)))),
            routing_files,
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
    if let Some(w) = whatwg.as_deref().filter(|w| *w != path) {
        let on_whatwg = routes::dispatch_under(req.method().as_str(), w, switches);
        if matches!(
            on_whatwg,
            Some(
                routes::Native::Account
                    | routes::Native::Projects
                    | routes::Native::Usage
                    | routes::Native::SamplingSettings
                    | routes::Native::ReasoningSettings
                    | routes::Native::StorageMetadata
                    | routes::Native::StorageBrowse
                    | routes::Native::RoutingMode
                    | routes::Native::RoutingDefault
                    | routes::Native::Stats
                    | routes::Native::Export
                    | routes::Native::ChatgptStatus
            )
        ) {
            native = on_whatwg;
        }
    }
    let served_bundle = match native {
        Some(routes::Native::Ready) => true,
        Some(routes::Native::Static) => static_files::canonical(&path),
        Some(
            routes::Native::Account
            | routes::Native::Projects
            | routes::Native::Usage
            | routes::Native::SamplingSettings
            | routes::Native::ReasoningSettings
            | routes::Native::StorageMetadata
            | routes::Native::StorageBrowse
            | routes::Native::RoutingMode
            | routes::Native::RoutingDefault
            | routes::Native::Stats
            | routes::Native::Export
            | routes::Native::ChatgptStatus,
        )
        | None => false,
    };
    // A model or role write Node's gate lets through drops the shared /api/stats answer first.
    if let Some(write) = stats_routes::invalidation(&app, &req, whatwg.as_deref()) {
        write.run(&app).await;
    }
    if !served_bundle {
        // Node's gate wrote these on every request it saw; since M3 it no longer can.
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
        Some(routes::Native::StorageMetadata) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                storage_read_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = p;
                reply::error(StatusCode::SERVICE_UNAVAILABLE, "Storage unavailable", true)
            }
        }
        Some(routes::Native::StorageBrowse) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-browse")]
            {
                storage_browse_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-browse"))]
            {
                let _ = p;
                reply::error(StatusCode::SERVICE_UNAVAILABLE, "Storage unavailable", true)
            }
        }
        Some(routes::Native::RoutingMode) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            routing_mode_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Account) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            account_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::ReasoningSettings) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            reasoning_settings_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::SamplingSettings) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            sampling_settings_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Usage) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            usage_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Projects) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            project_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::RoutingDefault) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            routing_default_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Stats) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            stats_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::ChatgptStatus) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-chatgpt-status")]
            {
                chatgpt_status_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-chatgpt-status"))]
            {
                let _ = p;
                app.proxy.forward(conn.peer, req).await
            }
        }
        Some(routes::Native::Export) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            export_routes::serve(&app, conn, req, p).await
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
