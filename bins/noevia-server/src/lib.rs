//! noevia-server: the Rust front (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
//! "Amendment 2026-10-10" in sbstndalton/noevia). It owns the routes contracts/http/routes.toml
//! gives to Rust and proxies every other request to Node ([`legacy_proxy`], deleted with Node).

pub mod account_routes;
#[cfg(feature = "rust-chatgpt-status")]
pub mod chatgpt_oauth_routes;
#[cfg(feature = "rust-chatgpt-status")]
pub mod chatgpt_status_routes;
pub mod code_net;
pub mod config;
pub mod connector_routes;
pub mod decision_settings_routes;
pub mod diary_routes;
pub mod export_routes;
pub mod extraction;
pub mod feature_context;
pub mod gdrive;
pub mod health;
pub mod identity;
pub mod legacy_proxy;
pub mod model_routes;
pub mod model_sizing;
pub mod offsite_engine;
pub mod offsite_mirror;
pub mod offsite_routes;
pub mod offsite_settings;
pub mod offsite_store;
pub mod plugin_directory_routes;
pub mod project_routes;
#[cfg(feature = "rust-storage-browse")]
pub mod project_storage_io;
pub mod project_store;
#[cfg(feature = "rust-storage-reads")]
pub mod provider_models_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod provider_test_routes;
pub mod public_pages;
pub mod reasoning_settings_routes;
pub mod reply;
pub mod routes;
pub mod routing_default_routes;
pub mod routing_mode_routes;
pub mod sampling_settings_routes;
pub mod serve;
pub mod source_job_routes;
pub mod static_files;
pub mod stats_routes;
#[cfg(feature = "rust-storage-browse")]
pub mod storage_browse_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod storage_integration_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod storage_metadata_worker;
#[cfg(feature = "rust-storage-reads")]
pub mod storage_read_routes;
pub mod upkeep;
pub mod usage_routes;
pub mod web_address_routes;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use std::sync::Arc;

/// What `noevia-server --features` lists, one per line.
/// `rust-auth`: this build answers sign-in and the account (M3), always (NOEVIA_RUST_AUTH is
/// retired, sbstndalton/noevia#1071); the web supervisor refuses to start without it, and Node
/// always refuses the account tables.
/// `rust-projects`: this build always answers a project's image routes (M4) and is the only writer
/// of projects.json; `rust-project-chats` and `rust-free-chats` (cargo features of those names): a
/// project's chat list and the free-chat list (`GET`/`POST /api/freechats`, free-chats.json). Their
/// switch NOEVIA_RUST_PROJECTS is retired (sbstndalton/noevia#1071): Node always refuses those
/// routes, so config.rs and the web supervisor refuse a build without any of them. `rust-stats` (cargo feature of that name): it answers
/// `GET /api/stats` under NOEVIA_RUST_STATS (#1650); the supervisor confirms it to Node
/// (NOEVIA_RUST_STATS_CONFIRMED), which then refuses that GET. `rust-export` (cargo feature of
/// that name): it answers `GET /api/export/conversations` under NOEVIA_RUST_EXPORT (#1315, with
/// rust-free-chats); the supervisor confirms it to Node
/// (NOEVIA_RUST_EXPORT_CONFIRMED), which then refuses that GET. `rust-chatgpt-status` (cargo
/// feature of that name): it answers `GET /api/providers/chatgpt` under NOEVIA_RUST_CHATGPT_STATUS
/// (#1373); the supervisor confirms it to Node (NOEVIA_RUST_CHATGPT_STATUS_CONFIRMED), which then
/// refuses that GET. `rust-project-writes` (cargo feature of that name): it answers the project
/// writes ported for sbstndalton/noevia#1264 (so far `GET`/`POST /api/chats/{chatId}/context`)
/// permanently (#1071); Node refuses these two methods without a deployment switch.
///
/// `rust-diary`: this build answers the Diary routes that are only a proxy and a policy
/// (`/api/diary/source`, `/today`, `/storage-import`, `/workspace-export`, `/workspace-import`;
/// sbstndalton/noevia#2185), permanently and with no switch. Node refuses them (core
/// rust-diary.cjs `RUST_DIARY_OWNED`), so the web supervisor must refuse a front without it.
///
/// `rust-model-reads`: this build answers `GET /api/models/capabilities` and `GET
/// /api/models/hardware` (sbstndalton/noevia#2187), permanently and with no switch. Node refuses
/// those two GETs (core rust-models.cjs `RUST_MODELS_OWNED`), so the web supervisor must refuse a
/// front without it beside such a Core, and a front with it beside an older Core.
///
/// `rust-project-reads`: this build answers `GET /api/projects/{id}/sources/text`, `GET
/// /api/projects/{id}/documents/original` and `GET /api/projects/{id}/uploads/original`
/// (sbstndalton/noevia#2191), permanently and with no switch. Node refuses those GETs (core
/// rust-project-reads.cjs `RUST_PROJECT_READS_OWNED`), so the web supervisor must refuse a front
/// without it beside such a Core, and a front with it beside an older Core.
///
/// `rust-project-lifecycle`: this build answers `POST /api/projects` and `DELETE
/// /api/projects/{projectId}` (sbstndalton/noevia#2191 round 2), permanently and with no switch,
/// and writes the index outbox (`users/<id>/index-outbox.db`) core's executor drains. Node refuses
/// those routes (core rust-project-lifecycle.cjs `RUST_PROJECT_LIFECYCLE_OWNED`) and publishes the
/// toolbox catalogue a create reads (`project-catalogue.json`), so the web supervisor must refuse a
/// front without it beside such a Core, and a front with it beside an older Core.
/// `rust-public-pages`: this build answers `GET|HEAD /about` and `/privacy` and every method of
/// `/device` (sbstndalton/noevia#2190); `rust-plugin-directory`: `/api/plugins/directory`. Both are
/// permanent with no switch; Node refuses them (core rust-public-pages.cjs and
/// rust-plugin-directory.cjs), so the web supervisor pairs each with its Core marker.
///
/// `rust-decision-settings`: this build answers every method of `/api/admin/decision-settings`
/// and `/test` and is the single writer of the `decision:configuration` row
/// (sbstndalton/noevia#2190); Node re-reads that row per decision and refuses the routes (core
/// rust-decision-settings.cjs `RUST_DECISION_SETTINGS_OWNED`), so the supervisor pairs them.
///
/// `rust-vault-mirror-preferences`: this build answers `GET|PUT /api/chat-vault-mirror/preferences`
/// natively (server-account vault_mirror) and writes the mirror nudges core polls; the paired core
/// refuses that path (rust-vault-mirror-preferences.cjs `RUST_VAULT_MIRROR_PREFERENCES_OWNED`).
/// `rust-storage-integrations`: this build answers `POST /api/integrations/storage/folder`,
/// `/test` and `/nextcloud/{start,poll,cancel}` (sbstndalton/noevia#2190): it owns the pending
/// Nextcloud sign-ins (`nextcloud-login-flows.json`), saves a completed sign-in's storage row and
/// writes the login proofs Node's storage latch re-reads. Node refuses those five POSTs (core
/// rust-storage-integrations.cjs `RUST_STORAGE_INTEGRATIONS_OWNED`), so the supervisor pairs them.
/// `rust-storage-connection`: this build also answers `PUT /api/integrations/storage` and is the
/// single writer of the `storage_connections` row (sbstndalton/noevia#2190 round 4), including the
/// legacy WebDAV setup Node asks for through the internal store (`/v1/storage/legacy-webdav`). The
/// paired core refuses the PUT (rust-storage-connection.cjs `RUST_STORAGE_CONNECTION_OWNED`) and
/// its database guard refuses every Node write of the table but the same-plaintext reseal.
/// `rust-provider-test`: this build answers `POST /api/providers/test` (provider_test_routes.rs,
/// sbstndalton/noevia#2190 round 4), reading the stored rows from disk for the key it may reuse;
/// the paired core refuses it (rust-provider-test.cjs `RUST_PROVIDER_TEST_OWNED`).
/// `rust-chatgpt-oauth`: this build is Sign in with ChatGPT's token broker (chatgpt_oauth_routes.rs,
/// sbstndalton/noevia#2190 round 5): it answers the device sign-in and the account's model list,
/// is the only writer of `chatgpt_oauth_tokens`, and hands Node's chat access tokens over the
/// internal store (`/v1/chatgpt/*`); the paired core refuses the routes
/// (rust-chatgpt-oauth.cjs `RUST_CHATGPT_OAUTH_OWNED`) and every Node write of the table.
/// `rust-provider-models`: this build answers `/api/providers/{id}/models` (provider_models_routes.rs,
/// round 5): the stored rows read from disk, the per-account cache, a ChatGPT row through the broker;
/// the paired core refuses it (rust-provider-models.cjs `RUST_PROVIDER_MODELS_OWNED`).
///
/// `rust-google-drive`: this build is Google Drive's token authority (gdrive.rs, sbstndalton/noevia#2190
/// round 7): the per-account device sign-in, the sealed refresh tokens (`google-drive-users/<id>.sealed`)
/// and the off-site backup's slot (`google-drive.sealed`), and the refresh. Node's Drive chat tools ask it
/// for access tokens over the internal store (`/v1/gdrive/*`) and never see a refresh token. It answers
/// `POST /api/connectors/gdrive/{connect,disconnect}` and `PUT .../{policy,backup-copy}`
/// (connector_routes.rs) and is the single writer of `tool_policies`; the paired core refuses them
/// (rust-google-drive.cjs `RUST_GOOGLE_DRIVE_OWNED`).
/// `rust-offsite-backup`: this build runs the off-site backup (offsite_*.rs): the seven
/// `/api/admin/offsite-backup` routes, the jobs, the nightly schedule (outside the appdata and NAS
/// windows), the status file and the snapshot store; the paired core refuses the routes
/// (rust-offsite-backup.cjs `RUST_OFFSITE_BACKUP_OWNED`) and has no backup code.
///
/// `rust-model-sizing`: this build answers `GET /api/models/estimate` and `GET
/// /api/models/preset/suggest` (sbstndalton/noevia#2189, src/model_sizing.rs), permanently and
/// with no switch. Node refuses those two GETs (core rust-model-sizing.cjs `RUST_MODEL_SIZING_OWNED`),
/// so the web supervisor pairs this capability with that Core in both directions.
///
/// `rust-project-storage`: this build writes to a user's connected storage for a project's
/// lifecycle (sbstndalton/noevia#2191 round 3): a create stores its text files in the project's
/// folder (#687) and the front runs the `sweep` jobs of its index outbox itself. Core then claims
/// only its own job kinds (core rust-project-storage.cjs `CORE_KINDS`), so the web supervisor must
/// refuse a front without it beside such a Core (its sweeps would never run), and a front with it
/// beside an older Core (both would run them).
///
/// `rust-project-files`: this build answers `DELETE /api/projects/{id}/files` (a source file's delete
/// from the user's storage, sbstndalton/noevia#2191 round 3) and queues the file's RAG removal as an
/// `unindex` job in the index outbox. Node refuses the route (core rust-project-files.cjs
/// `RUST_PROJECT_FILES_OWNED`) and runs `unindex` jobs, so the web supervisor pairs them.
///
/// `rust-project-document-pages`: this build answers `GET /api/projects/{id}/documents/pages` (a
/// stored document's page text and notice, sbstndalton/noevia#2191 round 4), permanently and with
/// no switch. Node refuses the GET (core rust-project-document-pages.cjs
/// `RUST_PROJECT_DOCUMENT_PAGES_OWNED`), so the web supervisor pairs them.
///
/// `rust-conversation-import`: this build answers `POST /api/import/conversations` (Settings → Data
/// → Import conversations, sbstndalton/noevia#2191 round 4) with Node's recovery journal, so an
/// import Node left unfinished is finished here. Node refuses the POST (core
/// rust-conversation-import.cjs `RUST_CONVERSATION_IMPORT_OWNED`), so the web supervisor pairs them.
///
/// `rust-document-extraction`: this build extracts new project documents itself (Docling, with the
/// OCR worker for the per-page retry; crates/server-projects `extract`) and drains each user's
/// durable extraction queue (`users/<id>/extraction-jobs.db`, sbstndalton/noevia#2191 round 5).
///
/// `rust-project-documents`: this build answers `POST /api/projects/{id}/documents` on that pipeline
/// (sbstndalton/noevia#2191 round 5). Node refuses the POST (core rust-project-documents.cjs
/// `RUST_PROJECT_DOCUMENTS_OWNED`), so the web supervisor pairs them.
///
/// `rust-source-jobs`: this build is the one owning process of the source-job store
/// (sbstndalton/noevia#2191 round 6, crates/server-projects `source_work`): it answers
/// `POST /api/projects/{id}/upload?background=1`, `POST .../sources/sync?background=1` and
/// `GET .../source-jobs/{jobId}`, and runs each job's operation by sending the synchronous route to
/// Node with `X-Noevia-Source-Work: 1`. It serves no internal source-job store routes any more
/// (`noevia-source-jobs-store/1` is gone). Node refuses the background starts and the poll (core
/// rust-source-jobs.cjs `RUST_SOURCE_JOBS_OWNED`) and has no source journal client, so the web
/// supervisor pairs them.
///
/// `rust-large-document-host-lock`: this build takes the host-wide large-document lock
/// (`UI_DATA_DIR/large-document.lock`, crates/server-projects `extract::large_lock`,
/// sbstndalton/noevia#2191 round 7) that core's large-document-lock.cjs also takes through
/// large-document-file-lock.cjs, so the two never hold a document over 25 MiB at once (#697). A
/// front without it beside a core with it (or the reverse) could, so the web supervisor pairs them.
///
/// `project-store`: this build serves the internal project store Node writes projects.json and
/// free-chats.json through (src/project_store.rs, #1264; permanent, no switch); the
/// supervisor confirms it to Node (NOEVIA_RUST_PROJECT_STORE_CONFIRMED) and hands both the secret.
///
/// `start-id`: this build reads NOEVIA_START_ID, the web supervisor's id for this container start,
/// and refuses to start beside Node without it (sbstndalton/noevia#1071: a running reply's partial
/// file carries the start that runs it, so a restart's leftovers read as interrupted).
///
/// `legacy-root-claim`: this binary exposes the filesystem-only legacy claim helper before server startup.
/// `client-address-header`: the front drops any `X-Noevia-Client-Ip` a client sends and stamps its
/// own value on what it forwards (#1245). The web supervisor confirms it to Node
/// (NOEVIA_FRONT_CLIENT_IP=1), and Node believes that header only then, so a core release paired
/// with a front that predates the strip cannot be handed a client-chosen address (#1578).
pub const FEATURES: &[&str] = &[
    "client-address-header",
    "code-net-guard",
    "header-read-timeout",
    "rust-feature-owner",
    "legacy-root-claim",
    "project-store",
    server_projects::research_jobs::RESEARCH_JOBS_STORE_MARKER,
    "rust-auth",
    "rust-diary",
    "rust-model-reads",
    "rust-project-reads",
    "rust-project-lifecycle",
    "rust-project-storage",
    "rust-project-files",
    "rust-project-document-pages",
    "rust-conversation-import",
    "rust-document-extraction",
    "rust-project-documents",
    "rust-source-jobs",
    "rust-large-document-host-lock",
    "rust-public-pages",
    "rust-plugin-directory",
    "rust-decision-settings",
    "rust-google-drive",
    "rust-offsite-backup",
    "rust-model-sizing",
    "rust-web-address-read",
    "rust-web-address-write",
    "rust-projects",
    "start-id",
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
    #[cfg(feature = "rust-storage-reads")]
    "rust-storage-integrations",
    #[cfg(feature = "rust-storage-reads")]
    "rust-storage-connection",
    #[cfg(feature = "rust-storage-reads")]
    "rust-provider-test",
    #[cfg(feature = "rust-chatgpt-status")]
    "rust-chatgpt-oauth",
    #[cfg(feature = "rust-storage-reads")]
    "rust-provider-models",
    "rust-vault-mirror-preferences",
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
    /// Google Drive's token authority (sbstndalton/noevia#2190 round 7); `None` without UI_DATA_DIR.
    pub drive: Option<Arc<gdrive::Drive>>,
    /// The off-site backup (jobs, schedule, status); `None` without UI_DATA_DIR.
    pub offsite: Option<Arc<offsite_routes::Service>>,
    /// GET /api/providers/chatgpt under NOEVIA_RUST_AUTH (M6, #1373; permanent since #1071).
    #[cfg(feature = "rust-chatgpt-status")]
    pub chatgpt_status: Option<Arc<chatgpt_status_routes::StatusLayer>>,
    /// Sign in with ChatGPT's token broker (sbstndalton/noevia#2190 round 5): the device sign-in,
    /// the token store and refresh, the catalogue cache; Node's chat asks it for access tokens.
    #[cfg(feature = "rust-chatgpt-status")]
    pub chatgpt_oauth: Option<Arc<chatgpt_oauth_routes::Broker>>,
    /// GET /api/providers/{id}/models: the per-account list cache (sbstndalton/noevia#2190 round 5).
    #[cfg(feature = "rust-storage-reads")]
    pub provider_models: provider_models_routes::Lists,
    #[cfg(feature = "rust-storage-reads")]
    pub storage_metadata: Arc<storage_metadata_worker::Worker>,
    /// Storage list/read admission: four at once, one per account (#1368).
    #[cfg(feature = "rust-storage-browse")]
    pub storage_browse: server_storage_reads::admission::Admission,
    /// This binary, run as the owned outbound child (`--storage-request-child`).
    #[cfg(feature = "rust-storage-browse")]
    pub storage_child: Option<std::path::PathBuf>,
    /// The Nextcloud sign-ins and the login proofs (#2190).
    #[cfg(feature = "rust-storage-reads")]
    pub storage_integrations: storage_integration_routes::State,
    /// Project storage write admission (round 3, #2191): four at once, one per account, apart
    /// from the browse's.
    #[cfg(feature = "rust-storage-browse")]
    pub storage_writes: server_storage_reads::admission::Admission,
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
    /// The ONE context controller for this native App and its existing private store.
    pub feature_context: Option<Arc<feature_context::Layer>>,
    pub usage: Arc<usage_routes::UsageLayer>,
    /// The Diary sidecar client (sbstndalton/noevia#2185); `None` only if its HTTP client could
    /// not be built, when its routes answer 503.
    pub diary: Option<Arc<server_diary::Diary>>,
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
    /// The Docling/OCR client of the extraction pipeline (sbstndalton/noevia#2191 round 5); `None`
    /// outside a Tokio runtime or when its HTTP client could not be built (the document route
    /// answers 503).
    pub extraction: Option<Arc<extraction::HttpEngine>>,
    /// GET /api/plugins/directory: its client and hour-long cache (#2190).
    pub plugin_directory: plugin_directory_routes::Directory,
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
        let feature_context = feature_context::Layer::new(&config, Arc::clone(&writes));
        let routing_files = config.rust_routing_mode.map(|_| {
            config
                .data_dir
                .as_deref()
                .ok_or(server_routing_mode::storage::Fault::Unavailable)
                .and_then(server_routing_mode::storage::Root::open)
        });
        let off = config.offsite.as_ref().map(|o| Arc::new(o.clone()));
        let drive = off
            .as_ref()
            .and_then(|o| gdrive::Drive::new(&config, Arc::clone(o)))
            .map(Arc::new);
        let offsite = match (off, drive.as_ref()) {
            (Some(o), Some(d)) => Some(Arc::new(offsite_routes::Service::new(o, Arc::clone(d)))),
            _ => None,
        };
        Arc::new(App {
            drive,
            offsite,
            #[cfg(feature = "rust-chatgpt-status")]
            chatgpt_status: chatgpt_status_routes::StatusLayer::new(&config).map(Arc::new),
            #[cfg(feature = "rust-chatgpt-status")]
            chatgpt_oauth: chatgpt_oauth_routes::Broker::new(&config, version.as_deref())
                .map(Arc::new),
            #[cfg(feature = "rust-storage-reads")]
            provider_models: provider_models_routes::Lists::default(),
            #[cfg(feature = "rust-storage-reads")]
            storage_metadata: Arc::new(storage_metadata_worker::Worker::default()),
            #[cfg(feature = "rust-storage-browse")]
            storage_browse: server_storage_reads::admission::Admission::default(),
            #[cfg(feature = "rust-storage-browse")]
            storage_child: storage_browse_routes::child_command(),
            #[cfg(feature = "rust-storage-reads")]
            storage_integrations: storage_integration_routes::State::new(
                config.storage_qa_ca.as_deref(),
            ),
            #[cfg(feature = "rust-storage-browse")]
            storage_writes: server_storage_reads::admission::Admission::default(),
            identity,
            writes,
            accounts,
            feature_context,
            usage: Arc::new(usage_routes::UsageLayer::default()),
            diary: server_diary::Diary::new(config.diary.clone())
                .ok()
                .map(Arc::new),
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
            extraction: tokio::runtime::Handle::try_current()
                .ok()
                .and_then(|rt| extraction::HttpEngine::new(rt, config.data_dir.clone())),
            plugin_directory: plugin_directory_routes::Directory::new(
                config.plugin_directory.clone(),
            ),
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
                    | routes::Native::WebAddress
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
                    | routes::Native::Diary
                    | routes::Native::Models
                    | routes::Native::PublicPages
                    | routes::Native::PluginDirectory
                    | routes::Native::DecisionSettings
                    | routes::Native::StorageIntegrations
                    | routes::Native::ProviderTest
                    | routes::Native::ChatgptOAuth
                    | routes::Native::ProviderModels
                    | routes::Native::Offsite
                    | routes::Native::Connectors
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
            | routes::Native::WebAddress
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
            | routes::Native::Diary
            | routes::Native::Models
            | routes::Native::PublicPages
            | routes::Native::PluginDirectory
            | routes::Native::DecisionSettings
            | routes::Native::StorageIntegrations
            | routes::Native::ProviderTest
            | routes::Native::ChatgptOAuth
            | routes::Native::ProviderModels
            | routes::Native::Offsite
            | routes::Native::Connectors,
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
        Some(routes::Native::WebAddress) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            web_address_routes::serve(&app, req, p).await
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
        Some(routes::Native::ProviderModels) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                provider_models_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Provider models unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::ChatgptOAuth) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-chatgpt-status")]
            {
                chatgpt_oauth_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-chatgpt-status"))]
            {
                let _ = p;
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ChatGPT sign-in unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::Offsite) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            offsite_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Connectors) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            connector_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Export) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            export_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Diary) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            diary_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Models) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            model_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::PublicPages) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", false);
            };
            public_pages::serve(&app, conn, req, p).await
        }
        Some(routes::Native::PluginDirectory) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            plugin_directory_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::StorageIntegrations) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                storage_integration_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(StatusCode::SERVICE_UNAVAILABLE, "Storage unavailable", true)
            }
        }
        Some(routes::Native::ProviderTest) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                provider_test_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Provider test unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::DecisionSettings) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            decision_settings_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Ready) => {
            if app.proxy.is_in_process() {
                app.proxy.forward(conn.peer, req).await
            } else {
                health::ready(&app.config.upstream, app.version.as_deref()).await
            }
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
