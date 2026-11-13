//! Native project retrieval (Rust port track R4): what a deep research job asks of a project's
//! sources, answered here instead of over the Node bridge (`/internal/research/retrieve`, core
//! `rust-research-bridge.cjs`). Switch `NOEVIA_RUST_RETRIEVAL`:
//! - `off` (the default; unset or empty too): the Node bridge answers, as before;
//! - `shadow`: the Node bridge answers; the native search runs beside it and a difference is
//!   logged (counts and the first differing position, never any text);
//! - `on`: the native search answers, except where it defers to the bridge (below).
//!
//! It is core `index.cjs` research `retrieve` with `rag.cjs` `searchProject`: the account's project
//! (looked up by the caller), the reranker off, the active embedding target, the index file
//! (never created, #1776), the maintenance lease free, the embedding service not off, the active
//! model's slot, the question embedded as a query by the active target, the read-only search of
//! `project-retrieval`, and the first five hits as `{file, text}`. Every failure Node turns into no
//! hits is no hits here. While it runs it holds `embedding_worker::foreground`, as Node holds
//! `foreground()`, so the native outbox and media passes yield to it.
//!
//! Defers to the bridge: the reranker is on (its client is not ported yet), the active target has no
//! address of its own (it embeds on the shared inference engine, whose admission is Node's), or the
//! embedding configuration or the reranker settings cannot be read.
//!
//! Named differences from Node: Node's own gate can also refuse an embedding for a reason only it
//! knows (a boot fence, a barrier, a recovery; the maintenance lease itself is checked here), and
//! Node's search drops chunks of a file it is re-indexing itself (its in-process wanted versions;
//! the native index writer is the outbox, which replaces a file's chunks in one transaction). And
//! those of `project_retrieval::search`: tie-order and unclaimed-legacy.
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use embedding_worker::{http::Http, runtime, Config as EmbeddingConfig, Target};
use project_retrieval::hits::Hit;
use project_retrieval::policy::{current_snapshot, embedding_off, rerank_target};
use project_retrieval::search::{self, Versions, TOP_K};
use prompt_framing::embedding_input::{self, Kind, Profile};
use prompt_framing::js::units;

/// The research bridge's cut (`index.cjs` `retrieve`: `.slice(0, 5)`).
pub const RESEARCH_HITS: usize = 5;

/// `NOEVIA_RUST_RETRIEVAL`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Off,
    Shadow,
    On,
}

impl Mode {
    pub fn parse(value: Option<&str>) -> Result<Mode, String> {
        match value.map(str::trim).unwrap_or_default() {
            "" | "off" => Ok(Mode::Off),
            "shadow" => Ok(Mode::Shadow),
            "on" => Ok(Mode::On),
            _ => Err("NOEVIA_RUST_RETRIEVAL must be off, shadow or on".to_owned()),
        }
    }
}

/// What the native search answered.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Hits(Vec<Hit>),
    /// The bridge answers instead, for this reason (a fixed label, no data).
    Defer(&'static str),
}

/// The process's native retrieval.
pub struct Retrieval {
    pub mode: Mode,
    /// The environment's embedding target (`runtime::seed`), used while no saved configuration
    /// exists; `None` when the environment's is invalid (then the saved one must be readable).
    seed: Option<EmbeddingConfig>,
    /// `EMBEDDING_SERVICE_URL`.
    service_url: Option<String>,
    http: Option<Http>,
}

impl std::fmt::Debug for Retrieval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retrieval")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Retrieval {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Retrieval, String> {
        let mode = Mode::parse(get("NOEVIA_RUST_RETRIEVAL").as_deref())?;
        if mode == Mode::Off {
            return Ok(Retrieval::off());
        }
        let inference = get("INFERENCE_BASE_URL")
            .filter(|s| !s.is_empty())
            .or_else(|| get("LEMONADE_BASE_URL").filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "http://host.docker.internal:11434".into());
        Ok(Retrieval {
            mode,
            seed: runtime::seed(get).ok(),
            service_url: get("EMBEDDING_SERVICE_URL"),
            // Only targets with their own address are searched natively, so no inference key.
            http: Http::new("", None, &inference, None).ok(),
        })
    }

    pub fn off() -> Retrieval {
        Retrieval {
            mode: Mode::Off,
            seed: None,
            service_url: None,
            http: None,
        }
    }

    /// The native answer for one question, embedding through this process's client.
    pub async fn retrieve(&self, inputs: Inputs<'_>) -> Outcome {
        let Some(http) = &self.http else {
            return Outcome::Defer("embedding client unavailable");
        };
        retrieve_with(
            self.seed.as_ref(),
            self.service_url.as_deref(),
            inputs,
            |target, framed| async move { http.embed_query(&target, &framed).await },
        )
        .await
    }
}

/// One question of one account's project. The caller has checked the project is the account's.
pub struct Inputs<'a> {
    pub data_dir: &'a Path,
    pub user: &'a str,
    pub project: &'a str,
    pub question: &'a str,
    /// This process's start id (whose embedding worker snapshot counts).
    pub start: &'a str,
    pub now_ms: f64,
    /// Whether the maintenance lease is held (Node's gate refuses every embedding then).
    pub maintenance_held: bool,
    /// The reranker's effective environment; `None` when the settings could not be read.
    pub rerank_env: Option<&'a HashMap<String, String>>,
}

/// `[a-zA-Z0-9_-]`-ish ids only: never a path. Node joins the id from the account's own project list.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('.') && !id.contains(['/', '\\', '\0'])
}

fn index_path(data_dir: &Path, user: &str, project: &str) -> Option<PathBuf> {
    if !safe_id(project) {
        return None;
    }
    let dir = server_projects::store::user_dir(data_dir, user).ok()?;
    Some(dir.join("rag").join(format!("{project}.db")))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// [`Retrieval::retrieve`] with the embedding call given (tests answer it from a recording).
pub async fn retrieve_with<E, F>(
    seed: Option<&EmbeddingConfig>,
    service_url: Option<&str>,
    inputs: Inputs<'_>,
    embed: E,
) -> Outcome
where
    E: FnOnce(Target, Vec<u16>) -> F,
    F: Future<Output = Result<Vec<f32>, String>>,
{
    let none = Outcome::Hits(Vec::new());
    match inputs.rerank_env {
        None => return Outcome::Defer("reranker settings unavailable"),
        Some(env) if rerank_target(env).enabled => return Outcome::Defer("reranker on"),
        Some(_) => {}
    }
    // An invalid environment seed still reads a saved configuration; without one it is refused.
    let seed = seed.cloned().unwrap_or_else(runtime::invalid_seed);
    let Ok(config) = runtime::load_config(inputs.data_dir, &seed) else {
        return Outcome::Defer("embedding configuration unavailable");
    };
    let target = config.active;
    let Some(base) = target.base_url.clone().filter(|b| !b.is_empty()) else {
        return Outcome::Defer("shared engine target");
    };
    let Some(path) = index_path(inputs.data_dir, inputs.user, inputs.project) else {
        return none;
    };
    let _foreground = embedding_worker::foreground::enter();
    if !path.is_file() || inputs.maintenance_held {
        return none;
    }
    let status = std::fs::read(inputs.data_dir.join("embedding-worker-status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let snapshot = status
        .as_ref()
        .and_then(|s| current_snapshot(s, inputs.start, inputs.now_ms));
    if embedding_off(snapshot, inputs.now_ms, service_url, Some(&base)) {
        return none;
    }
    let (profile, model) = (target.profile.clone(), target.model.clone());
    let slot_path = path.clone();
    let dim = blocking(move || {
        embedding_worker::sqlite_vec_loader::register().ok()?;
        let conn = search::open_index(&slot_path).ok()??;
        let slot = search::find_slot(
            &conn,
            search::Target {
                profile: &profile,
                model: &model,
            },
        )
        .ok()??;
        Some(slot.dim())
    })
    .await
    .flatten();
    let Some(dim) = dim else { return none };
    let Ok(framing) = Profile::try_from(target.profile.as_str()) else {
        return none;
    };
    let framed = embedding_input::frame(framing, Kind::Query, &units(inputs.question), None);
    let (profile, model) = (target.profile.clone(), target.model.clone());
    let Ok(query) = embed(target, framed).await else {
        return none;
    };
    if query.len() != dim {
        return none;
    }
    let hits = blocking(move || {
        let conn = search::open_index(&path).ok()??;
        search::search(
            &conn,
            search::Target {
                profile: &profile,
                model: &model,
            },
            &query,
            TOP_K,
            &Versions::new(),
        )
        .ok()
    })
    .await
    .flatten()
    .unwrap_or_default();
    Outcome::Hits(hits.into_iter().take(RESEARCH_HITS).collect())
}

/// The shadow log line: whether the two answers agree, by counts and the first differing position.
pub fn compare(node: &[(String, String)], native: &[(String, String)]) -> String {
    match node.iter().zip(native).position(|(a, b)| a != b) {
        None if node.len() == native.len() => format!("match ({} hits)", node.len()),
        first => format!(
            "differs (node {} hits, native {} hits, first difference at {})",
            node.len(),
            native.len(),
            first.unwrap_or(node.len().min(native.len()))
        ),
    }
}

#[cfg(test)]
mod tests;
