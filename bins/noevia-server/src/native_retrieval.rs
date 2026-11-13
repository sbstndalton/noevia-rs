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
//! Named differences from Node:
//! - Node's own gate can also refuse an embedding for a reason only it knows (a boot fence, a
//!   barrier, a recovery). The maintenance lease is checked here before and after the embed, but
//!   a lease taken while the embed is in flight does not wait for it as Node's gate does.
//! - Node's own in-flight change of the embedding service (and the 6 s after it) is not visible
//!   here; the service is what the native worker last reported, kept as Node keeps it.
//! - A research job's search does not see Node's in-process wanted versions (a re-index Node runs
//!   itself; the native index writer is the outbox, which replaces a file's chunks in one
//!   transaction). Node's own searches send theirs ([`Inputs::versions`]).
//! - those of `project_retrieval::search`: tie-order and unclaimed-legacy.
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use embedding_worker::{http::Http, runtime, Config as EmbeddingConfig, Target};
use project_retrieval::hits::Hit;
use project_retrieval::policy::{
    current_snapshot, embedding_off, observe, rerank_target, Observation,
};
use project_retrieval::search::{self, ImageSettings, Versions, WithImages};
use prompt_framing::embedding_input::{self, Kind, Profile};
use prompt_framing::js::units;

pub use project_retrieval::search::TOP_K;

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
    /// Case does not count; anything but off, shadow or on refuses the start, as the other
    /// switches do.
    pub fn parse(value: Option<&str>) -> Result<Mode, String> {
        match value
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "off" => Ok(Mode::Off),
            "shadow" => Ok(Mode::Shadow),
            "on" => Ok(Mode::On),
            _ => Err("NOEVIA_RUST_RETRIEVAL must be off, shadow or on".to_owned()),
        }
    }
}

/// What the native search answered: its hits (text hits, or `searchProjectWithImages`' answer).
#[derive(Debug, PartialEq)]
pub enum Outcome<T = Vec<Hit>> {
    Hits(T),
    /// The bridge answers instead, for this reason (a fixed label, no data).
    Defer(&'static str),
}

impl Outcome<WithImages> {
    /// The text half only (`searchProject`).
    pub fn text(self) -> Outcome {
        match self {
            Outcome::Hits(w) => Outcome::Hits(w.text),
            Outcome::Defer(why) => Outcome::Defer(why),
        }
    }
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
    /// The embedding service as last observed (`embedding-service.cjs` `last`).
    observed: Mutex<Option<Observation>>,
    /// `RAG_IMAGE_TOP_K` and `RAG_IMAGE_MIN_SCORE`, read once as `rag.cjs` reads them at load.
    pub images: ImageSettings,
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
        // Only targets with their own address are searched natively, so no inference key.
        let http = Http::new("", None, &inference, None).ok();
        if http.is_none() {
            eprintln!("noevia-server: native retrieval has no embedding client (the inference address is unusable); every search defers to Node");
        }
        Ok(Retrieval {
            mode,
            seed: runtime::seed(get).ok(),
            service_url: get("EMBEDDING_SERVICE_URL"),
            http,
            observed: Mutex::new(None),
            images: ImageSettings::from_env(
                get("RAG_IMAGE_TOP_K").as_deref(),
                get("RAG_IMAGE_MIN_SCORE").as_deref(),
            ),
        })
    }

    pub fn off() -> Retrieval {
        Retrieval {
            mode: Mode::Off,
            seed: None,
            service_url: None,
            http: None,
            observed: Mutex::new(None),
            images: ImageSettings::from_env(None, None),
        }
    }

    /// The native answer for one question (`searchProject`), embedding through this process's client.
    pub async fn retrieve(&self, inputs: Inputs<'_>) -> Outcome {
        self.retrieve_images(inputs, None).await.text()
    }

    /// [`Retrieval::retrieve`], and with `images` the image half beside the text
    /// (`searchProjectWithImages`).
    pub async fn retrieve_images(
        &self,
        inputs: Inputs<'_>,
        images: Option<ImageSettings>,
    ) -> Outcome<WithImages> {
        let Some(http) = &self.http else {
            return Outcome::Defer("embedding client unavailable");
        };
        retrieve_with(
            self.seed.as_ref(),
            self.service_url.as_deref(),
            &self.observed,
            inputs,
            images,
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
    /// Whether the maintenance lease is held (Node's gate refuses every embedding then); asked
    /// before and after the embed.
    pub maintenance_held: &'a (dyn Fn() -> bool + Sync),
    /// The reranker's effective environment; `None` when the settings could not be read.
    pub rerank_env: Option<&'a HashMap<String, String>>,
    /// Node's wanted versions for the project's files (`rag.cjs` `fileVersions`); empty for research.
    pub versions: Versions,
    /// At most this many hits: [`RESEARCH_HITS`] for research, [`TOP_K`] for Node's `searchProject`.
    pub limit: usize,
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

/// The files a search reads before it embeds, read on the blocking pool.
struct Disk {
    config: Result<EmbeddingConfig, String>,
    index: bool,
    status: Option<serde_json::Value>,
}

/// [`Retrieval::retrieve_images`] with the embedding call given (tests answer it from a recording).
/// `images`: also the image half (`searchProjectWithImages`, whose empty answer is the same shape).
pub async fn retrieve_with<E, F>(
    seed: Option<&EmbeddingConfig>,
    service_url: Option<&str>,
    observed: &Mutex<Option<Observation>>,
    inputs: Inputs<'_>,
    images: Option<ImageSettings>,
    embed: E,
) -> Outcome<WithImages>
where
    E: FnOnce(Target, Vec<u16>) -> F,
    F: Future<Output = Result<Vec<f32>, String>>,
{
    let none = Outcome::Hits(WithImages::default());
    match inputs.rerank_env {
        None => return Outcome::Defer("reranker settings unavailable"),
        Some(env) if rerank_target(env).enabled => return Outcome::Defer("reranker on"),
        Some(_) => {}
    }
    let path = index_path(inputs.data_dir, inputs.user, inputs.project);
    // An invalid environment seed still reads a saved configuration; without one it is refused.
    let seed = seed.cloned().unwrap_or_else(runtime::invalid_seed);
    let (dir, probe) = (inputs.data_dir.to_path_buf(), path.clone());
    let Some(disk) = blocking(move || Disk {
        config: runtime::load_config(&dir, &seed),
        index: probe.is_some_and(|p| p.is_file()),
        status: std::fs::read(dir.join("embedding-worker-status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok()),
    })
    .await
    else {
        return Outcome::Defer("embedding configuration unavailable");
    };
    let Ok(config) = disk.config else {
        return Outcome::Defer("embedding configuration unavailable");
    };
    let target = config.active;
    let Some(base) = target.base_url.clone().filter(|b| !b.is_empty()) else {
        return Outcome::Defer("shared engine target");
    };
    // `observeNative()` on every search, as Node's `managed()` and `enabled()` do.
    let last = {
        let mut observed = match observed.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let snapshot = disk
            .status
            .as_ref()
            .and_then(|s| current_snapshot(s, inputs.start, inputs.now_ms));
        *observed = observe(observed.take(), snapshot, inputs.now_ms);
        observed.clone()
    };
    let (Some(path), true) = (path, disk.index) else {
        return none;
    };
    let _foreground = embedding_worker::foreground::enter();
    if (inputs.maintenance_held)() {
        return none;
    }
    if embedding_off(last.as_ref(), inputs.now_ms, service_url, Some(&base)) {
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
    if query.len() != dim || (inputs.maintenance_held)() {
        return none;
    }
    let versions = inputs.versions;
    let mut found = blocking(move || {
        let conn = search::open_index(&path).ok()??;
        let target = search::Target {
            profile: &profile,
            model: &model,
        };
        match images {
            Some(settings) => {
                search::search_with_images(&conn, target, &query, TOP_K, &versions, settings).ok()
            }
            None => search::search(&conn, target, &query, TOP_K, &versions)
                .ok()
                .map(|text| WithImages {
                    text,
                    ..WithImages::default()
                }),
        }
    })
    .await
    .flatten()
    .unwrap_or_default();
    found.text.truncate(inputs.limit);
    Outcome::Hits(found)
}

/// How long [`store_answer`] may take: under core `rust-retrieval.cjs` `DEADLINE_MS` (30 s).
pub const STORE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(25);

/// `POST /v1/retrieval/search` on the store listener (`--features rust-retrieval-v1`): Node's own
/// `rag.cjs` `searchProject` asking through core `rust-retrieval.cjs`. Body `{user, projectId, query,
/// versions, images?}` (`versions`: Node's wanted versions of the project's files, a string or
/// `null` each); answer `{hits: [{file, body, score}]}` (at most [`TOP_K`]), with `images: true`
/// `searchProjectWithImages`' `{text, images, indexed}`, or `{defer: reason}`, on which Node
/// searches itself. Node has resolved the project and run its own account and tombstone guards;
/// an index without an account (the shared `RAG_DIR`) is Node's. The caller has checked the
/// per-start secret. One variable feeds both processes, so Node's mode decides how the answer is
/// used: this route answers while the front's switch is `on` or `shadow` and defers when it is
/// `off`. It answers within [`STORE_DEADLINE`], under the client's 30 s, so a slow embedding
/// service ends in a deferral here, not a second embed in Node after a timeout.
pub async fn store_answer(app: std::sync::Arc<crate::App>, raw: &[u8]) -> (u16, serde_json::Value) {
    use serde_json::{json, Value};
    let bad = |why: &str| (400, json!({ "error": why }));
    let Ok(body) = serde_json::from_slice::<Value>(raw) else {
        return bad("invalid JSON");
    };
    let (Some(project), Some(query)) = (
        body.get("projectId").and_then(Value::as_str),
        body.get("query").and_then(Value::as_str),
    ) else {
        return bad("projectId and query required");
    };
    let mut versions = Versions::new();
    match body.get("versions") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (file, want) in m {
                match want {
                    Value::String(v) => versions.insert(file.clone(), Some(v.clone())),
                    Value::Null => versions.insert(file.clone(), None),
                    _ => return bad("versions must be strings or null"),
                };
            }
        }
        Some(_) => return bad("versions must be an object"),
    }
    let images = match body.get("images") {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return bad("images must be a boolean"),
    };
    let defer = |why: &str| (200, json!({ "defer": why }));
    let retrieval = &app.config.retrieval;
    if retrieval.mode == Mode::Off {
        return defer("off");
    }
    let Some(user) = body.get("user").and_then(Value::as_str) else {
        return defer("shared index");
    };
    let Some(data_dir) = app.config.data_dir.clone() else {
        return defer("no data directory");
    };
    let a = std::sync::Arc::clone(&app);
    let rerank_env =
        tokio::task::spawn_blocking(move || crate::reranker_routes::effective_env(&a).ok())
            .await
            .ok()
            .flatten();
    let held = || app.config.maint.authority.held();
    let search = retrieval.retrieve_images(
        Inputs {
            data_dir: &data_dir,
            user,
            project,
            question: query,
            start: app.config.start_id.as_deref().unwrap_or_default(),
            now_ms: crate::identity::now_ms() as f64,
            maintenance_held: &held,
            rerank_env: rerank_env.as_ref(),
            versions,
            limit: TOP_K,
        },
        images.then_some(retrieval.images),
    );
    let Ok(outcome) = tokio::time::timeout(STORE_DEADLINE, search).await else {
        return defer("deadline");
    };
    let hit = |h: Hit| json!({ "file": h.file, "body": h.body, "score": h.score });
    let row = |r: search::ImageRow| json!({ "file": r.file, "kind": "image", "meta": r.meta });
    match outcome {
        Outcome::Hits(found) if images => (
            200,
            json!({
                "text": found.text.into_iter().map(hit).collect::<Vec<_>>(),
                "images": found.images.into_iter().map(|i| {
                    let mut v = row(i.row);
                    if let Some(o) = v.as_object_mut() {
                        o.insert("label".into(), json!(i.label));
                        o.insert("hash".into(), json!(i.hash));
                        o.insert("score".into(), json!(i.score));
                    }
                    v
                }).collect::<Vec<_>>(),
                "indexed": found.indexed.into_iter().map(row).collect::<Vec<_>>(),
            }),
        ),
        Outcome::Hits(found) => (
            200,
            json!({ "hits": found.text.into_iter().map(hit).collect::<Vec<_>>() }),
        ),
        Outcome::Defer(why) => defer(why),
    }
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
