//! Embedding worker lifecycle. No new feature switch: MODEL_LOADER_URL gates
//! construction; Core's boot-bound ownership proof gates all actual work.
use crate::App;
use embedding_worker::{
    http::Http,
    runtime::{self, Background},
    sqlite::{open_vec_required, SqlStore},
    Options,
};
use std::sync::Arc;
fn try_start(
    app: &Arc<App>,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<Background>, String> {
    let Some(loader) = get("MODEL_LOADER_URL").filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let root = app
        .config
        .data_dir
        .clone()
        .ok_or("embedding data directory unavailable")?;
    let store = SqlStore {
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
    runtime::start(
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
    )
    .map(Some)
}

/// Optional retrieval can fail without taking sign-in/chat/the front down.
/// Invalid saved state is re-read by the owner; unavailable prerequisites leave
/// only this subsystem idle, never fail process startup.
pub fn start(app:&Arc<App>,get:impl Fn(&str)->Option<String>)->Result<Option<Background>,String> {
    match try_start(app,get) {
        Ok(worker)=>Ok(worker),
        Err(_)=>{eprintln!("noevia-server: embedding background idle (prerequisite unavailable or another owner)");Ok(None)}
    }
}
