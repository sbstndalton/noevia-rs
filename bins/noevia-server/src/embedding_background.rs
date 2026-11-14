//! Migration ownership is independent of model-loader. Only the ten-second
//! service refresh needs MODEL_LOADER_URL; boot-bound proof gates all work.
use crate::App;
use embedding_worker::{
    http::Http,
    runtime::{self, Background},
    sqlite::{open_vec_required, SqlStore},
    Options,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether this process's embedding background started (the front is the embedding owner).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether [`start`] started the background (core `rust-health.cjs` `ownerReady`, as the front
/// reads it for its own retrieval readiness, retrieval_status.rs).
pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Records whether the background runs ([`start`] calls it; a test of the readiness route stands in
/// for the background with it).
pub fn mark_running(running: bool) {
    RUNNING.store(running, Ordering::SeqCst);
}

fn try_start(
    app: &Arc<App>,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<Background>, String> {
    let loader = get("MODEL_LOADER_URL")
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let root = app
        .config
        .data_dir
        .clone()
        .ok_or("embedding data directory unavailable")?;
    let store = SqlStore {
        rag_dir: get("RAG_DIR")
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from),
        root,
        seed: runtime::seed(&get).unwrap_or_else(|_| runtime::invalid_seed()),
        open_vec: open_vec_required,
    };
    embedding_worker::sqlite_vec_loader::register()?;
    let inference = get("INFERENCE_BASE_URL")
        .filter(|s| !s.is_empty())
        .or_else(|| get("LEMONADE_BASE_URL").filter(|s| !s.is_empty()))
        .unwrap_or("http://host.docker.internal:11434".into());
    let token = ["INFERENCE_API_KEY", "LEMONADE_API_KEY"]
        .iter()
        .find_map(|key| get(key).filter(|s| !s.is_empty()))
        .filter(|s| s != "local");
    let http = Http::new(&loader, get("MODEL_LOADER_TOKEN"), &inference, token)?;
    let pairing = app
        .config
        .project_store
        .as_ref()
        .ok_or("embedding pairing token unavailable")?;
    let start = app
        .config
        .start_id
        .as_ref()
        .ok_or("embedding pairing start unavailable")?;
    let n = |key: &str, default: f64| {
        get(key)
            .map(|s| {
                if s.trim().is_empty() {
                    0.0
                } else {
                    s.parse().unwrap_or(f64::NAN)
                }
            })
            .unwrap_or(default)
    };
    let http = http.with_admission(
        &format!("http://{}", app.config.upstream.authority()),
        &pairing.token,
    )?;
    runtime::start_with_documents(
        store,
        http,
        format!("http://{}", app.config.upstream.authority()),
        pairing.token.clone(),
        start.clone(),
        Options::new(
            n("EMBEDDING_REINDEX_DUTY", 0.5),
            n("EMBEDDING_REINDEX_BATCH", 4.0),
            n("EMBEDDING_REINDEX_MIN_PAUSE_MS", 50.0),
            n("EMBEDDING_REINDEX_RETRY_MS", 60_000.0),
            n("EMBEDDING_NEXT_RECHECK_MS", 300_000.0),
        ),
        get("EMBEDDING_SERVICE_URL"),
        app.extraction.clone().map(|engine| {
            std::sync::Arc::new(move |control| {
                engine.with_background_control(control)
                    as std::sync::Arc<dyn server_projects::extract::Engine + Send + Sync>
            }) as embedding_worker::media_runtime::EngineFactory
        }),
    )
    .map(Some)
}

/// Optional retrieval can fail without taking sign-in/chat/the front down.
/// Invalid saved state is re-read by the owner; unavailable prerequisites leave
/// only this subsystem idle, never fail process startup.
pub fn start(
    app: &Arc<App>,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<Background>, String> {
    match try_start(app, get) {
        Ok(worker) => {
            mark_running(worker.is_some());
            Ok(worker)
        }
        Err(_) => {
            eprintln!("noevia-server: embedding background idle (prerequisite unavailable or another owner)");
            Ok(None)
        }
    }
}
