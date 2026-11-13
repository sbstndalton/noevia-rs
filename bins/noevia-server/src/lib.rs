//! noevia-server: the Rust front (full-Rust migration M1, docs/adr-0001-rust-and-repo-split.md
//! "Amendment 2026-10-10" in sbstndalton/noevia). It owns the routes contracts/http/routes.toml
//! gives to Rust and proxies every other request to Node ([`legacy_proxy`], deleted with Node).

pub mod account_routes;
pub mod admin_user_routes;
pub mod approval_registry;
pub mod approval_revocation;
pub mod auto_roles_routes;
pub mod autotune_cancel_routes;
pub mod autotune_clear_routes;
pub mod autotune_read_routes;
pub mod autotune_resume_routes;
pub mod autotune_runner;
pub mod autotune_settings_routes;
pub mod autotune_untuned_routes;
pub mod calibration_cancel_routes;
pub mod calibration_routes;
pub mod chat_activity;
pub mod chat_admission;
pub mod chat_edge;
pub mod chat_framing_suggest;
pub mod chat_mirror;
pub mod chat_turn;
pub mod chat_wire;
#[cfg(feature = "rust-chatgpt-status")]
pub mod chatgpt_oauth_routes;
#[cfg(feature = "rust-chatgpt-status")]
pub mod chatgpt_status_routes;
pub mod code_egress;
pub mod code_net;
pub mod config;
pub mod connector_routes;
pub mod decision_settings_routes;
pub mod delete_model;
pub mod diary_backup;
pub mod diary_routes;
pub mod embedding_background;
pub mod export_routes;
pub mod extraction;
pub mod feature_context;
pub mod gdrive;
pub mod gdrive_tools;
pub mod health;
pub mod health_routes;
pub mod identity;
pub mod idle_unload_read;
pub mod inference_machines;
pub mod legacy_proxy;
pub mod load_model;
pub mod machine_service;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_broker;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_call;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_directory;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_directory_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_discovery;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_discovery_actor;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_discovery_backend;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_egress;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_manifest;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod mcp_tools;
pub mod model_assignments;
pub mod model_downloads_read;
pub mod model_kind;
pub mod model_maint;
pub mod model_manager_scan;
pub mod model_routes;
pub mod model_sizing;
pub mod model_state_routes;
pub mod native_retrieval;
#[cfg(feature = "rust-storage-reads")]
pub mod nextcloud_routes;
pub mod offsite_engine;
pub mod offsite_mirror;
pub mod offsite_routes;
pub mod offsite_settings;
pub mod offsite_store;
pub mod plugin_directory_routes;
pub mod preset_read_routes;
pub mod project_config_routes;
pub mod project_routes;
#[cfg(feature = "rust-storage-browse")]
pub mod project_storage_io;
pub mod project_store;
pub mod provider_files;
#[cfg(feature = "rust-storage-reads")]
pub mod provider_models_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod provider_reseal;
#[cfg(feature = "rust-storage-reads")]
pub mod provider_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod provider_test_routes;
pub mod prune_missing;
pub mod public_pages;
pub mod reasoning_settings_routes;
pub mod reply;
pub mod reranker_routes;
pub mod reranker_settings;
pub mod research_routes;
mod residual_fallback;
pub mod routes;
pub mod routing_default_routes;
pub mod routing_mode_routes;
pub mod sampling_settings_routes;
pub mod serve;
pub mod snapshot;
pub mod source_job_routes;
#[cfg(feature = "rust-storage-browse")]
pub mod source_storage_io;
pub mod specialists_routes;
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
pub mod task_routes;
pub mod tool_approval_routes;
#[cfg(feature = "rust-storage-reads")]
pub mod toolbox_picker;
#[cfg(feature = "rust-storage-reads")]
pub mod toolbox_routes;
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
/// `rust-diary-routes`: this build also answers the rest of the Diary routes the web client names
/// except the local exchange (sbstndalton/noevia#2206): `/api/diary` (nothing), `/api/diary/context`,
/// `/api/diary/exchanges`, `/api/diary/storage-status`, `/api/diary-connector` and
/// `/api/profile/diary-connectors[/{id}]`. It is the only writer of `diary_connectors` and reads the
/// running-exchange set Node publishes (`diary-conversations/active.json`). Node refuses them (core
/// rust-diary-routes.cjs `RUST_DIARY_ROUTES_OWNED`), so the web supervisor pairs them.
///
/// `rust-diary-local-exchange`: this build answers `/api/diary/local-exchange` (every method; a POST
/// relays the companion exchange, streamed or not). The inference maintenance gate it holds stays
/// Node's (the `llm:` budget chat shares is this front's since chat port S6, chat_edge.rs): the front
/// takes the gate with an inference-admission lease
/// (`{ diaryExchange: { user } }`) that only a Core with that ask grants. Node refuses the route (core
/// rust-diary-local-exchange.cjs `RUST_DIARY_LOCAL_EXCHANGE_OWNED`), so the web supervisor pairs them.
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
/// natively (server-account vault_mirror) and nudges its own mirror schedule when a save turns the
/// mirror on; the paired core refuses that path (rust-vault-mirror-preferences.cjs
/// `RUST_VAULT_MIRROR_PREFERENCES_OWNED`).
/// `rust-chat-mirror-v1`: this build reads every active account's chat change journal in process and
/// owns the Diary mirror and chat brain schedules (chat_mirror.rs, crate chat-mirror-sched); the paired
/// core keeps only their two work executors and nudges the schedule over the project store (core
/// rust-chat-mirror.cjs `RUST_CHAT_MIRROR_OWNED`). Both processes must carry it: a front without it
/// beside this core leaves the journal unread, a core without it beside this front runs a second schedule.
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
/// `rust-providers`: this build answers `GET|POST /api/providers`, `PUT|DELETE /api/providers/{id}`
/// and `DELETE /api/providers/chatgpt` (provider_routes.rs, sbstndalton/noevia#2190 round 6) and is
/// the only writer of `providers.json` and `shared-providers.json`, including the ChatGPT row the
/// token broker writes when a sign-in is saved; Node re-reads the private rows by their file
/// signal and saves only its seal-on-load migration, through the project store. The paired core
/// refuses these methods (rust-providers.cjs `RUST_PROVIDERS_OWNED`).
/// `rust-providers-reseal`: this build re-seals the provider keys in those two files on the internal
/// store, `POST /v1/providers/reseal` (provider_reseal.rs, one pass per file under its lock with the
/// current and the previous key), which core's secrets rotation (rust-providers-reseal.cjs) calls in
/// place of rewriting the files itself; the pair is checked like `rust-providers`.
///
/// `rust-google-drive`: this build is Google Drive's token authority (gdrive.rs, sbstndalton/noevia#2190
/// round 7): the per-account device sign-in, the sealed refresh tokens (`google-drive-users/<id>.sealed`)
/// and the off-site backup's slot (`google-drive.sealed`), and the refresh. Node's Drive chat tools ask it
/// for access tokens over the internal store (`/v1/gdrive/*`) and never see a refresh token. It answers
/// `POST /api/connectors/gdrive/{connect,disconnect}` and `PUT .../{policy,backup-copy}`
/// (connector_routes.rs) and is the single writer of `tool_policies`; the paired core refuses them
/// (rust-google-drive.cjs `RUST_GOOGLE_DRIVE_OWNED`).
/// `rust-offsite-backup`: this build runs the off-site backup (offsite_*.rs): the eight
/// `/api/admin/offsite-backup` routes, the jobs, the nightly schedule (outside the appdata and NAS
/// windows), the status file and the snapshot store; the paired core refuses the routes
/// (rust-offsite-backup.cjs `RUST_OFFSITE_BACKUP_OWNED`) and has no backup code.
/// `rust-health`: this build answers `GET /api/health` (health_routes.rs, sbstndalton/noevia#2190
/// close-out): the default provider and the Diary sidecar it probes itself, retrieval from the
/// `rag-status.json` snapshot Node publishes. `rust-auto-roles`: `GET /api/auto-roles`
/// (auto_roles_routes.rs; persistence is native) from `auto-roles.json` and the
/// `auto-roles-snapshot.json` Node publishes. `rust-auto-roles-write` adds PUT and native deletion cleanup. `rust-specialists`: the
/// six /api/specialists and /api/spaces routes (specialists_routes.rs, sbstndalton/noevia#2446); no Node counterpart exists, so no
/// core marker pairs with it. `rust-workspace`: `GET /api/workspace`
/// (sbstndalton/noevia#2191 close-out), with the delete-old-chats sweep and the orphan chat-context
/// cleanup it runs. The paired core refuses each (rust-health.cjs `RUST_HEALTH_OWNED`,
/// rust-auto-roles.cjs `RUST_AUTO_ROLES_OWNED`, rust-workspace.cjs `RUST_WORKSPACE_OWNED`) and
/// publishes the snapshots, so the supervisor pairs each capability with its marker.
///
/// `rust-mcp-credentials`: this build is the broker of the MCP server credentials (mcp_broker.rs,
/// mcp_routes.rs, sbstndalton/noevia#2202). It answers `GET /api/mcp-keys/servers`, `PUT|DELETE
/// /api/mcp-keys/{id}`, `GET /api/mcp-oauth/servers`, `POST /api/mcp-oauth/{id}/connect`, `DELETE
/// /api/mcp-oauth/{id}` and the sign-in callback, is the only writer of `mcp_oauth_tokens`,
/// `mcp_oauth_clients` and `directory_mcp_user_keys`, hands Node's chat access tokens and key
/// headers per call over the internal store (`/v1/mcp/*`), publishes who is ready in
/// `mcp-credential-state.json` and re-seals the three tables for a secrets rotation. The paired
/// core refuses the routes (rust-mcp-credentials.cjs `RUST_MCP_CREDENTIALS_OWNED`) and every Node
/// write of the tables.
///
/// `rust-mcp-directory`: this build owns the admin MCP directory (mcp_directory.rs,
/// mcp_directory_routes.rs, sbstndalton/noevia#2202 round 2): `GET|POST /api/admin/mcp-directory`,
/// `POST .../custom` and `.../custom/preview`, `PUT .../{id}/keys` and `.../{id}/oauth-client`, `DELETE
/// .../{id}`. It lists a candidate server's tools itself (initialize, tools/list over JSON or
/// event-stream replies, public https only), is the only writer of `directory_mcp_servers` (the server
/// rows and the administrators' shared key slot) and re-seals that slot for a secrets rotation. Node
/// re-reads the table for its chat-time MCP client. The paired core refuses the routes
/// (rust-mcp-directory.cjs `RUST_MCP_DIRECTORY_OWNED`), asks this front for discovery
/// (`POST /v1/mcp/discovery`) and writes nothing of the table.
/// `rust-toolboxes-permitted`: `GET /api/toolboxes/permitted` (toolbox_routes.rs), computed here from
/// the front's own state and that snapshot (core `RUST_TOOLBOXES_PERMITTED_OWNED`).
/// `rust-toolbox-picker`: every method of `/api/toolboxes`; current account filtering and
/// response composition in toolbox_picker.rs from the front's own MCP discovery and the chat process's published toolbox facts.
/// `rust-connectors`: `GET /api/connectors` and `PUT /api/connectors/nextcloud/policy`
/// (nextcloud_routes.rs; core `RUST_CONNECTORS_OWNED`).
///
/// `rust-model-sizing`: this build answers `GET /api/models/estimate` and `GET
/// /api/models/preset/suggest` (sbstndalton/noevia#2189, src/model_sizing.rs), permanently and
/// with no switch. Node refuses those two GETs (core rust-model-sizing.cjs `RUST_MODEL_SIZING_OWNED`),
/// so the web supervisor pairs this capability with that Core in both directions.
///
/// `rust-model-maintenance`: this build answers `POST /api/models/presets/reload`, `GET
/// /api/models/evidence` and `POST /api/models/evidence/import` (sbstndalton/noevia#2210,
/// src/model_maint), permanently and with no switch: the inference gate's maintenance hold, the
/// models.ini read, the reload guard state file (`native-preset-reload-<hash>.json`) and the
/// evidence store (`evidence-<hash>/`, whose only writer this is; Node's producers queue their
/// records in `evidence-inbox-<hash>/`). Node refuses those (core rust-model-maintenance.cjs
/// `RUST_MODEL_MAINTENANCE_OWNED`) and keeps its own reloads' baselines in the same state file under
/// the same lock file, so the web supervisor and the release assembly pair this capability with that
/// Core in both directions.
///
/// `noevia-inference-admission-authority/1` (engine_admission::authority `AUTHORITY_MARKER`, port contract
/// row H): this build issues every inference admission lease and answers `POST
/// /v1/inference-admission/session` on the project-store listener; Core's latch
/// (`noevia-inference-admission-latch/1`) takes its holds only for leases issued here.
/// `rust-calibration-worker-v2`: this build runs the calibration job itself (sbstndalton/noevia#2303,
/// src/model_maint/calibration_worker.rs): its launch under the maintenance hold, the run, its
/// cancellation and evidence, `native-calibration-<hash>.json` and the boot recovery of a job a
/// restart interrupted. Core then builds no public calibrator and does not recover the job at boot
/// (core rust-calibration-routes.cjs `CALIBRATION_WORKER_OWNER`), so the web supervisor and the
/// release assembly pair them in both directions. Generation 2: every load the job asks for is first
/// decided by Core's own load refusal (`POST /internal/model-maintenance/load-refusal`, core
/// model-load-refusal.cjs), so it pairs only with `noevia-calibration-worker-rust/2`.
///
/// `noevia-maintenance-lease-authority/2` (model_maint/lease.rs `AUTHORITY_MARKER`, port contract row J):
/// this build is the one model-maintenance lease authority and answers `POST
/// /v1/maintenance-lease/{acquire,renew,release,held,session}` on the project-store listener. Core's
/// `noevia-maintenance-lease-client/2` takes every maintenance hold there; the release assembly pairs the
/// two markers in both directions.
///
/// `rust-preset-baseline`: this build is the preset reload baseline's one in-memory authority
/// (port contract 2026-11-04 RS-1, model_maint/reload.rs `Guard`) and answers `POST
/// /v1/preset-baseline/{session,begin,settle,invalidate,verdict,router-text}` on the project-store
/// listener. A Core that opens a protocol-1 `session` writes the baseline only through it; until one
/// does (an older Core), that Core writes the state file itself under the same lock, which this
/// build re-reads on every use (shared mode), so no pairing is required in either direction.
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
/// `GET .../source-jobs/{jobId}`, and (round 8) runs each job's operation in this process. It serves no
/// internal source-job store routes (`noevia-source-jobs-store/1` is gone). Node refuses the background
/// starts and the poll (core rust-source-jobs.cjs `RUST_SOURCE_JOBS_OWNED`) and has no source journal
/// client, so the web supervisor pairs them.
///
/// `rust-project-source-work`: this build runs the source operations itself (sbstndalton/noevia#2191
/// round 8, crates/server-projects `upload` and `sync`): the organized and older upload, the attached
/// folders' sync, the folder import's plan and its tickets, on Docling and the streamed storage read, in
/// the foreground or as a source job. It keeps no `X-Noevia-Source-Work` worker call. Node refuses
/// `POST /api/projects/{id}/upload`, `.../sources/sync` and `.../folder-import/plan` (core
/// rust-project-source-work.cjs `RUST_PROJECT_SOURCE_WORK_OWNED`), reads the storage latch from
/// `UI_DATA_DIR/storage-latch.db` and the running file operations from `users/<id>/source-file-ops.json`,
/// so the web supervisor pairs them.
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
/// `rust-budget-watch-v3`: this build runs the #697 memory watchdog (src/model_maint/budget_watch.rs, Core's
/// `inference-budget-watch.cjs` moved): the sampling, the strikes, the idle baseline, the cooldown and the
/// emergency unload through the inference gate. Core then does not start its own and answers only the
/// maintenance state and the quarantine on `/internal/budget-watch` (core rust-budget-watch.cjs
/// `BUDGET_WATCH_OWNER`), so the web supervisor and the release assembly pair them in both directions: a
/// mixed pair would run two watchdogs, or none.
///
/// `rust-task-reads`: this build answers `/api/code/active` and the GETs of a project's Code and Browser
/// state and task (task_routes.rs); Core refuses them (rust-task-reads.cjs) and offers the facts only it
/// holds on the loopback bridge (rust-task-facts.cjs). The supervisor pairs them.
///
/// `rust-autotune-settings`: this build answers every method of /api/models/autotune/settings;
/// public policy/response here, conditional live tune-state writer over a trusted loopback bridge.
///
/// `rust-model-state-reads`: this build answers GET /api/models/inference-budget and GET /api/models/installed
/// (model_state_routes.rs); Core refuses them (rust-model-state-reads.cjs) and offers the facts only it holds on the
/// loopback bridge (rust-model-facts.cjs). The supervisor pairs them.
///
/// `rust-mcp-discovery`: this build discovers MCP tools itself (mcp_discovery_actor.rs: connect, list, convert,
/// bind the curated boxes, cadence, backoff) and answers `POST /v1/mcp/discovery` on the internal store; core
/// (mcp-native-discovery.cjs) is its fail-closed client and publishes only toolbox-chat-facts.json. A core that
/// still discovers beside this front would offer different tools than the front's own pages, and a front without
/// it beside a core that asks would leave every chat with no MCP tools, so the release pairs them.
///
/// `rust-chat-framing-suggest`: this build answers every method of `/api/chat-framing/suggest`
/// (chat_framing_suggest.rs, chat port S4): the gate, 405, the 10-a-minute `chat-frame:` budget, the
/// body and the cut to 4,000 UTF-16 units; Core refuses the POST (rust-chat-framing-suggest.cjs) and
/// keeps only the user's lists and the decision behind `/internal/chat-framing/suggest`. The
/// supervisor pairs them.
///
/// `rust-chat-turn`: this build answers `POST /api/chat` (chat_turn.rs, chat port S5) as a
/// transparent relay: the session gate, then the request unread to Core's loopback
/// `/internal/chat/turn`, whose unchanged pipeline answers byte for byte; Core refuses any other
/// POST /api/chat (rust-chat-turn.cjs). The supervisor and assemble-release pair them.
///
/// `rust-chat-edge`: this build answers what routes/chat.cjs answered before a turn reached its loop
/// (chat_edge.rs, chat port S6): the `llm:<user>` budget, which the Diary local exchange shares, the
/// body cap, JSON, the request shape, the Cowork refusals and the Diary add-on gate, each after Core's
/// inference gate. A Cowork start that passes goes to Core's `/internal/chat/cowork`. Core removes its
/// own limiter (rust-chat-edge.cjs). The supervisor and assemble-release pair them.
///
/// `rust-chat-wire`: this build writes the chat turn's event stream itself (chat_wire.rs, chat port
/// S7): it asks Core's relayed turn for NDJSON (`x-noevia-chat-wire: ndjson/1`) and writes the
/// headers, one frame per event, the 5 s `: keep-alive` comment and the end; a Core that answers with
/// its own event stream is refused (502). Core writes the relayed turn's events as NDJSON
/// (rust-chat-wire.cjs). The supervisor and assemble-release pair them.
///
/// `rust-chat-admission`: this build takes Core's inference gate for a chat turn, a Cowork start and
/// the Diary local exchange itself, with an admission lease (chat_admission.rs, chat port S8;
/// crates/engine-admission): at most 3 s of waiting, ended when the client leaves, Node's 503 and its
/// `[chat] 503` line answered here. The call carries the lease and Core does not enter the gate for it
/// again (rust-chat-admission.cjs); the S6 gate hold and probe are gone. The supervisor and
/// assemble-release pair them.
///
/// `chat-activity`: this build records each authenticated request's account in
/// `UI_DATA_DIR/chat-activity.json` (chat_activity.rs, chat port S3), which core's change journal
/// reader merges as request starts (chat-activity.cjs). Either side alone keeps today's behaviour.
/// With `rust-chat-mirror-v1` that reader is this front's own journal consumer (chat_mirror.rs
/// `ACTIVITY_FILE`), which merges the same file on each tick; core no longer reads it.
///
/// `client-address-header`: the front drops any `X-Noevia-Client-Ip` a client sends and stamps its
/// own value on what it forwards (#1245). The web supervisor confirms it to Node
/// (NOEVIA_FRONT_CLIENT_IP=1), and Node believes that header only then, so a core release paired
/// with a front that predates the strip cannot be handed a client-chosen address (#1578).
pub const FEATURES: &[&str] = &[
    "rust-residual-fallbacks-v1",
    "rust-source-startup-v1",
    "rust-dav-listener-v1",
    "reranker-settings-v1",
    "rust-diary-backup-worker",
    "rust-embedding-background-v1",
    "rust-mcp-discovery",
    "rust-chat-mirror-v1",
    "rust-index-outbox-v3",
    "rust-media-background-v1",
    "client-address-header",
    "code-net-guard",
    "header-read-timeout",
    "rust-feature-owner",
    "legacy-root-claim",
    "project-store",
    "rust-research",
    // Rust port track R4: POST /v1/retrieval/search on the store listener (native_retrieval.rs).
    "rust-retrieval-v1",
    "rust-task-reads",
    "rust-tool-approvals",
    // Port contract 2026-11-04 S9: the pending approvals and chat grants are the front's
    // (approval_registry.rs); Node's turn asks through the store's /v1/approvals/*.
    "rust-approval-registry",
    // Re-reviews 1ed3a14/689d17a: single-use grant tickets redeemed at dispatch, revocations that
    // drain redeemed writes; Core names it (approval_registry.rs PROTOCOL).
    "registry-executor-v3",
    "noevia-research-jobs-store/1",
    "rust-auth",
    "rust-diary",
    "rust-diary-routes",
    "rust-diary-local-exchange",
    "rust-model-reads",
    "rust-autotune-untuned",
    "rust-model-state-reads",
    "rust-autotune-settings",
    "rust-project-reads",
    "rust-project-lifecycle",
    "rust-project-storage",
    "rust-project-files",
    "rust-project-document-pages",
    "rust-conversation-import",
    "rust-document-extraction",
    "rust-project-documents",
    "rust-source-jobs",
    "rust-project-source-work",
    "rust-project-config",
    "rust-large-document-host-lock",
    "rust-public-pages",
    "rust-plugin-directory",
    "rust-decision-settings",
    "rust-google-drive",
    "rust-offsite-backup",
    "rust-model-sizing",
    "rust-model-maintenance",
    "rust-preset-baseline",
    // Port contract row H (engine_admission::authority AUTHORITY_MARKER; the release assembly reads this literal).
    "noevia-inference-admission-authority/1",
    // Port contract row J: the maintenance lease authority (model_maint/lease.rs AUTHORITY_MARKER; the
    // release assembly reads this exact literal and pairs it with core's lease client).
    "noevia-maintenance-lease-authority/2",
    "rust-autotune-cancel",
    "rust-calibration-cancel",
    "rust-calibration-routes",
    "rust-calibration-worker-v2",
    "rust-model-manager-scan",
    "rust-model-evidence-recheck",
    // Row V: the front is the only issuer of the egress proxy's control key and signed task grants
    // (code_egress.rs); the paired Core asks it (`noevia-code-egress-client/1`) and publishes nothing.
    "rust-code-egress-issuer",
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
    #[cfg(feature = "rust-storage-reads")]
    "rust-providers",
    #[cfg(feature = "rust-storage-reads")]
    "rust-providers-reseal",
    #[cfg(feature = "rust-storage-reads")]
    "rust-mcp-credentials",
    #[cfg(feature = "rust-storage-reads")]
    "rust-mcp-directory",
    #[cfg(feature = "rust-storage-reads")]
    "rust-toolboxes-permitted",
    #[cfg(feature = "rust-storage-reads")]
    "rust-toolbox-picker",
    "rust-autotune-clear",
    "rust-autotune-read",
    "rust-preset-read",
    "rust-idle-unload-read",
    "rust-model-assignments",
    "rust-inference-machines",
    "rust-model-downloads-read",
    "rust-autotune-resume",
    "rust-budget-watch-v3",
    #[cfg(feature = "rust-storage-reads")]
    "rust-connectors",
    // An account's deletion forgets the MCP sign-ins (rust-storage-reads) and the ChatGPT tokens
    // (rust-chatgpt-status) itself: a build without either must not be offered beside a Core that no
    // longer deletes accounts (admin_user_routes.rs `cleanup`).
    #[cfg(all(feature = "rust-storage-reads", feature = "rust-chatgpt-status"))]
    "rust-admin-users",
    "rust-vault-mirror-preferences",
    "rust-health",
    "rust-auto-roles",
    "rust-auto-roles-write",
    "rust-specialists",
    "rust-workspace",
    "rust-chat-framing-suggest",
    "rust-chat-turn",
    "rust-chat-edge",
    "rust-chat-wire",
    "rust-chat-admission",
    "chat-activity",
    // GET /api/chats/search (#1450) and GET /api/chats/{chatId}/export (#1471): core rust-chat-search.cjs
    // `RUST_CHAT_SEARCH_OWNED` and rust-chat-export.cjs `RUST_CHAT_EXPORT_OWNED` refuse them.
    "rust-chat-search",
    "rust-chat-export",
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

/// Whether the auth database has a row for this account; an unreadable database says `false` (a caller
/// that asks before saving something for the account keeps nothing it cannot vouch for).
pub(crate) fn account_row_exists(writes: &upkeep::WriterLayer, id: &str) -> bool {
    writes
        .writer()
        .ok()
        .and_then(|w| {
            w.read(|r| {
                r.row("SELECT 1 FROM users WHERE id=?1", [id], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .ok()
        })
        .flatten()
        .is_some()
}

pub struct App {
    /// The chat approval registry (port contract 2026-11-04 S9): pending write approvals, routing
    /// questions and chat-wide grants, answered by POST /api/tool-approvals/{id}.
    pub approvals: Arc<approval_registry::Registry>,
    pub machines: Arc<machine_service::Service>,
    pub auto_roles: auto_roles_routes::Writer,
    /// The demand marker and the wait for Node's published snapshots (snapshot.rs).
    pub demand: Arc<snapshot::Demand>,
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
    /// The MCP credential broker (sbstndalton/noevia#2202): OAuth sign-ins and per-account keys.
    #[cfg(feature = "rust-storage-reads")]
    pub mcp: Option<Arc<mcp_broker::Mcp>>,
    /// MCP tool discovery (sbstndalton/noevia row T): made on first use, `None` inside when it cannot run.
    #[cfg(feature = "rust-storage-reads")]
    pub mcp_discovery: std::sync::OnceLock<Option<Arc<mcp_discovery_backend::Handle>>>,
    /// The code-sandbox egress control (row V, code_egress.rs): the only issuer of the proxy's
    /// control key and of the signed task grants; `None` when `CODE_EGRESS_PORT` is unset.
    pub code_egress: Option<Arc<code_egress::Issuer>>,
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
    /// `connectorRate` of the Diary connector endpoint (core routes/diary.cjs): fixed windows, one
    /// per bucket key, in this process's memory like core's.
    diary_connector_rate: std::sync::Mutex<server_account::rate::RateLimiter>,
    /// The chat mirror and brain schedule, set once the server runs (`rust-chat-mirror-v1`);
    /// Node's triggers reach it over the project store.
    pub chat_mirror: std::sync::OnceLock<Arc<chat_mirror::ChatMirror>>,
    /// core index.cjs's `llmRateLimiter`, one map as in Node: the `chat-frame:<user>` buckets (chat
    /// port S4) and the `llm:<user>` budget that the chat turn and the Diary local exchange share
    /// (chat port S6, chat_edge.rs).
    pub(crate) llm_rate: std::sync::Mutex<server_account::rate::RateLimiter>,
    /// Who made an authenticated request, for Node's chat change journal reader (chat port S3).
    pub activity: Arc<chat_activity::Activity>,
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
    /// `connectorRate.rateLimited(key, limit, 60000)`: counts this call, true when it is over.
    pub(crate) fn connector_rate(&self, key: &str, limit: u64) -> bool {
        self.diary_connector_rate
            .lock()
            .map(|mut r| r.limited(key, limit, 60_000, identity::now_ms()))
            // A poisoned limiter cannot count; refusing is the safe side for a credential endpoint.
            .unwrap_or(true)
    }

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
        if let Some(drive) = drive.as_ref() {
            // A sign-in that finishes for a deleted account saves nothing (review L5).
            let writes = Arc::clone(&writes);
            drive.guard_accounts(move |id| account_row_exists(&writes, id));
        }
        let offsite = match (off, drive.as_ref()) {
            (Some(o), Some(d)) => Some(Arc::new(offsite_routes::Service::new(o, Arc::clone(d)))),
            _ => None,
        };
        let approvals = Arc::new(approval_registry::Registry::new(
            match (config.data_dir.as_ref(), config.start_id.as_ref()) {
                (Some(data_dir), Some(start)) => Some(approval_registry::Live {
                    data_dir: data_dir.clone(),
                    start: start.clone(),
                }),
                // No store or no start id: no grant can be bound, so approve_all approves once.
                _ => None,
            },
        ));
        Arc::new(App {
            approvals,
            machines: Arc::new(machine_service::Service::default()),
            demand: Arc::new(snapshot::Demand::default()),
            drive,
            offsite,
            #[cfg(feature = "rust-chatgpt-status")]
            chatgpt_status: chatgpt_status_routes::StatusLayer::new(&config).map(Arc::new),
            #[cfg(feature = "rust-chatgpt-status")]
            chatgpt_oauth: chatgpt_oauth_routes::Broker::new(&config, version.as_deref())
                .map(Arc::new),
            #[cfg(feature = "rust-storage-reads")]
            mcp: mcp_broker::Mcp::new(&config).map(Arc::new),
            code_egress: code_egress::Issuer::new(&config),
            #[cfg(feature = "rust-storage-reads")]
            mcp_discovery: std::sync::OnceLock::new(),
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
            chat_mirror: std::sync::OnceLock::new(),
            diary_connector_rate: std::sync::Mutex::new(server_account::rate::RateLimiter::new(
                identity::now_ms(),
            )),
            llm_rate: std::sync::Mutex::new(server_account::rate::RateLimiter::new(
                identity::now_ms(),
            )),
            activity: chat_activity::Activity::new(config.data_dir.clone()),
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
            auto_roles: auto_roles_routes::Writer::default(),
            proxy: legacy_proxy::LegacyProxy::new(config.upstream.clone(), config.trust_proxy),
            statics: static_files::StaticFiles::new(config.dist.clone()),
            version,
            code_net,
            config,
        })
    }
}

/// Whether `path` is `/api/projects/{id}` or below it with an `id` (percent-decoded once, as
/// Node decodes a path segment) that names a coworker's hidden project.
fn hidden_project_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/projects/") else {
        return false;
    };
    let segment = rest.split('/').next().unwrap_or("");
    server_projects::js::decode_uri_component(segment)
        .is_some_and(|id| id.starts_with(server_projects::store::SPECIALIST_PREFIX))
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
    // A coworker's backing project is the coworker's to change (sbstndalton/noevia#2446): no
    // `/api/projects/{id}...` route, Rust's or Node's, may name it. Checked on the raw path and on
    // Node's WHATWG pathname, so no spelling of the id reaches a handler.
    if hidden_project_path(&path)
        || identity::whatwg_pathname_of(req.uri())
            .as_deref()
            .is_some_and(hidden_project_path)
    {
        return reply::json(
            StatusCode::NOT_FOUND,
            &serde_json::json!({"error": "no such project"}),
            true,
        );
    }
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
                    | routes::Native::AdminUsers
                    | routes::Native::Connectors
                    | routes::Native::Providers
                    | routes::Native::Health
                    | routes::Native::AutoRoles
                    | routes::Native::Specialists
                    | routes::Native::McpCredentials
                    | routes::Native::McpDirectory
                    | routes::Native::AutotuneSettings
                    | routes::Native::ToolboxPicker
                    | routes::Native::ToolboxesPermitted
                    | routes::Native::ToolApprovals
                    | routes::Native::ChatFramingSuggest
                    | routes::Native::ChatTurn
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
            | routes::Native::AdminUsers
            | routes::Native::Connectors
            | routes::Native::Providers
            | routes::Native::Health
            | routes::Native::AutoRoles
            | routes::Native::Specialists
            | routes::Native::McpCredentials
            | routes::Native::McpDirectory
            | routes::Native::AutotuneSettings
            | routes::Native::ToolboxPicker
            | routes::Native::ToolboxesPermitted
            | routes::Native::ToolApprovals
            | routes::Native::ChatFramingSuggest
            | routes::Native::ChatTurn,
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
        // Chat port S3: the account of an authenticated /api/ request, as Node's request gate saw it,
        // for its change journal reader (chat_activity.rs).
        let api = whatwg.as_deref().unwrap_or(&path).starts_with("/api/");
        let activity = Arc::clone(&app.activity);
        let _ = tokio::task::spawn_blocking(move || {
            let user = writes.upkeep_blocking(identity.authenticator(), &creds, now);
            if let Some(user) = user.filter(|_| api) {
                activity.record(&user, now);
            }
        })
        .await;
    }
    if let Some(p) = whatwg.as_deref() {
        let fallback = residual_fallback::classify(req.method().as_str(), p).or_else(|| {
            (p != path
                && ["/api/", "/api/models", "/api/connect"].contains(&p)
                && routes::route_for(p).is_none()
                && (residual_fallback::classify(req.method().as_str(), &path).is_some()
                    || path.starts_with("/api/providers/")))
            .then_some(residual_fallback::Kind::Missing)
        });
        if let Some(kind) = fallback {
            return residual_fallback::serve(&app, req, p.to_string(), kind).await;
        }
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
        Some(routes::Native::McpCredentials) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                mcp_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "MCP credentials unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::McpDirectory) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                mcp_directory_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "MCP directory unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::AutotuneSettings) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            autotune_settings_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::ToolApprovals) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            tool_approval_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::ChatFramingSuggest) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            chat_framing_suggest::serve(&app, req, p).await
        }
        Some(routes::Native::ChatTurn) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            chat_turn::serve(&app, conn, req, p).await
        }
        Some(routes::Native::ToolboxPicker) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                toolbox_picker::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Toolboxes unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::ToolboxesPermitted) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                toolbox_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Toolboxes unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::Providers) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            #[cfg(feature = "rust-storage-reads")]
            {
                provider_routes::serve(&app, conn, req, p).await
            }
            #[cfg(not(feature = "rust-storage-reads"))]
            {
                let _ = (p, conn, req);
                reply::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Providers unavailable",
                    true,
                )
            }
        }
        Some(routes::Native::AdminUsers) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            admin_user_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Health) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            health_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::Specialists) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            specialists_routes::serve(&app, conn, req, p).await
        }
        Some(routes::Native::AutoRoles) => {
            let Some(p) = whatwg else {
                return reply::error(StatusCode::BAD_REQUEST, "invalid URL", true);
            };
            auto_roles_routes::serve(&app, conn, req, p).await
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
        // Node-owned: forwarded without a client-named lease (#2485).
        _ => chat_edge::forward_node(&app, conn, req).await,
    }
}

/// The front's router; serve it with [`serve::serve`] (or axum's
/// `into_make_service_with_connect_info::<serve::Conn>()`, which has no timeouts).
pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

mod machine_json;
pub mod source_startup;

pub mod dav;

#[cfg(test)]
mod feature_literal_compatibility_tests {
    #[test]
    fn research_job_feature_literal_matches_its_store() {
        assert!(
            super::FEATURES.contains(&server_projects::research_jobs::RESEARCH_JOBS_STORE_MARKER)
        );
    }
}
