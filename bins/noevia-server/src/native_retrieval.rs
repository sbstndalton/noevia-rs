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
//! With the reranker on (`NOEVIA_FEATURE_RAG_RERANK` and a reranker address, the admin's switch over
//! them), the cosine pool is `RAG_RERANK_POOL` wide and `project_retrieval::rerank` asks the reranker
//! for the kept hits within its deadline, falling back to the cosine order as `rerankPool` does.
//!
//! Defers to the bridge: the active target has no address of its own (it embeds on the shared
//! inference engine, whose admission is Node's), or the embedding configuration or the reranker
//! settings cannot be read.
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
//! - those of `project_retrieval::search` (tie-order, unclaimed-legacy) and of
//!   `project_retrieval::rerank` (the request cut at the deadline, this process's own bench).
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use embedding_worker::{http::Http, runtime, Config as EmbeddingConfig, Target};
use project_retrieval::hits::Hit;
use project_retrieval::policy::{current_snapshot, embedding_off, observe, Observation};
use project_retrieval::rerank::{self, Chain};
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
    /// `RAG_RERANK_POOL`, `RAG_RERANK_KEEP`, `RAG_RERANK_DEADLINE_MS` and `RERANK_API_KEY` of this
    /// process (`rag.cjs` reads them from `process.env`).
    knobs: HashMap<String, String>,
    /// The reranker's deadline bench, this process's (a fresh one when its configuration changes).
    bench: Mutex<Chain>,
}

/// What a search reads besides its question.
pub struct Deps<'a> {
    pub seed: Option<&'a EmbeddingConfig>,
    pub service_url: Option<&'a str>,
    pub observed: &'a Mutex<Option<Observation>>,
    pub bench: &'a Mutex<Chain>,
    /// Milliseconds since the epoch, read when the bench is asked and when a miss is recorded.
    pub clock: &'a (dyn Fn() -> f64 + Sync),
    pub knobs: &'a HashMap<String, String>,
}

const KNOBS: [&str; 4] = [
    "RAG_RERANK_POOL",
    "RAG_RERANK_KEEP",
    "RAG_RERANK_DEADLINE_MS",
    "RERANK_API_KEY",
];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
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
            knobs: KNOBS
                .iter()
                .filter_map(|k| get(k).map(|v| ((*k).to_owned(), v)))
                .collect(),
            bench: Mutex::new(Chain::default()),
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
            knobs: HashMap::new(),
            bench: Mutex::new(Chain::default()),
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
        let deps = Deps {
            seed: self.seed.as_ref(),
            service_url: self.service_url.as_deref(),
            observed: &self.observed,
            bench: &self.bench,
            clock: &|| crate::identity::now_ms() as f64,
            knobs: &self.knobs,
        };
        retrieve_with(
            &deps,
            inputs,
            images,
            |target, framed| async move { http.embed_query(&target, &framed).await },
            |request: rerank::Request, bearer: Option<String>| async move {
                http.post_json(
                    &request.url,
                    bearer.as_deref(),
                    request.body,
                    std::time::Duration::from_secs(60),
                    rerank::MAX_ANSWER_BYTES,
                )
                .await
                .ok()
            },
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
    /// At most this many text hits: [`RESEARCH_HITS`] for research; no cut for Node's
    /// `searchProject`, which answers the cosine top [`TOP_K`] or the reranker's `keep`.
    pub limit: usize,
    /// When the caller stops waiting: the reranker is not waited for past it (and that counts as a
    /// deadline miss, so a reranker slower than the caller is benched as Node benches it).
    pub until: Option<std::time::Instant>,
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
/// `send` posts one reranker request (with the API key as the bearer) and answers its status and
/// JSON body, or `None` when it failed; the reranker's deadline is raced here.
pub async fn retrieve_with<E, F, S, G>(
    deps: &Deps<'_>,
    inputs: Inputs<'_>,
    images: Option<ImageSettings>,
    embed: E,
    send: S,
) -> Outcome<WithImages>
where
    E: FnOnce(Target, Vec<u16>) -> F,
    F: Future<Output = Result<Vec<f32>, String>>,
    S: FnOnce(rerank::Request, Option<String>) -> G,
    G: Future<Output = Option<(u16, serde_json::Value)>>,
{
    let (seed, service_url, observed) = (deps.seed, deps.service_url, deps.observed);
    let none = Outcome::Hits(WithImages::default());
    // `initRerank` over the reranker's effective environment: on, the cosine pool is wider and the
    // reranker picks the kept hits (`rerankPool`).
    let Some(env) = inputs.rerank_env else {
        return Outcome::Defer("reranker settings unavailable");
    };
    let reranker = rerank::Config::from_env(env, |k| deps.knobs.get(k).cloned());
    let pool = reranker.as_ref().map_or(TOP_K, |c| c.pool);
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
                search::search_with_images(&conn, target, &query, pool, &versions, settings).ok()
            }
            None => search::search(&conn, target, &query, pool, &versions)
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
    if let Some(cfg) = reranker {
        let hits = std::mem::take(&mut found.text);
        let open = !lock(deps.bench).bench_for(&cfg).benched((deps.clock)());
        let ranking = if !rerank::needed(&hits) || !open {
            None
        } else {
            let request = rerank::request(&cfg, inputs.question, &hits);
            let mut wait = std::time::Duration::from_millis(cfg.deadline_ms);
            if let Some(until) = inputs.until {
                wait = wait.min(until.saturating_duration_since(std::time::Instant::now()));
            }
            match tokio::time::timeout(wait, send(request, cfg.api_key.clone())).await {
                Err(_) => {
                    lock(deps.bench).bench_for(&cfg).missed((deps.clock)());
                    None
                }
                Ok(answer) => {
                    let scores = answer.and_then(|(status, body)| {
                        rerank::parse_value(cfg.backend, hits.len(), status, &body)
                    });
                    // Only an answer the backend accepts resets the misses (a throw leaves them).
                    if scores.is_some() {
                        lock(deps.bench).bench_for(&cfg).answered();
                    }
                    scores.map(|s| rerank::order(&s))
                }
            }
        };
        found.text = rerank::merge(hits, cfg.keep, ranking);
    }
    found.text.truncate(inputs.limit);
    Outcome::Hits(found)
}

/// How long [`store_answer`] may take: under core `rust-retrieval.cjs` `DEADLINE_MS` (30 s).
pub const STORE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(25);

/// `POST /v1/retrieval/search` on the store listener (`--features rust-retrieval-v1`): Node's own
/// `rag.cjs` `searchProject` asking through core `rust-retrieval.cjs`. Body `{user, projectId, query,
/// versions, images?}` (`versions`: Node's wanted versions of the project's files, a string or
/// `null` each); answer `{hits: [{file, body, score}]}` (the cosine top [`TOP_K`], or the
/// reranker's `RAG_RERANK_KEEP`), with `images: true`
/// `searchProjectWithImages`' `{text, images, indexed}`, or `{defer: reason}`, on which Node
/// searches itself. Node has resolved the project and run its own account and tombstone guards;
/// an index without an account (the shared `RAG_DIR`) is Node's. The caller has checked the
/// per-start secret. One variable feeds both processes, so Node's mode decides how the answer is
/// used: this route answers while the front's switch is `on` or `shadow` and defers when it is
/// `off`. It answers within [`STORE_DEADLINE`], under the client's 30 s, so a slow embedding
/// service ends in a deferral here, not a second embed in Node after a timeout.
pub async fn store_answer(app: std::sync::Arc<crate::App>, raw: &[u8]) -> (u16, String) {
    use serde_json::{json, Value};
    let bad = |why: &str| (400, json!({ "error": why }).to_string());
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
    let defer = |why: &str| (200, json!({ "defer": why }).to_string());
    let retrieval = &app.config.retrieval;
    if retrieval.mode == Mode::Off {
        return defer("off");
    }
    let Some(user) = body.get("user").and_then(Value::as_str) else {
        return defer("shared index");
    };
    // While the native writer owns the live index, its wanted versions are the ones that count.
    versions.extend(app.config.index_writes.search_versions(project, user));
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
            limit: usize::MAX,
            until: Some(
                std::time::Instant::now() + STORE_DEADLINE - std::time::Duration::from_secs(1),
            ),
        },
        images.then_some(retrieval.images),
    );
    let Ok(outcome) = tokio::time::timeout(STORE_DEADLINE, search).await else {
        return defer("deadline");
    };
    let hit = |h: Hit| json!({ "file": h.file, "body": h.body, "score": h.score });
    match outcome {
        // Image meta goes out as the JSON text `mediaRow` gives (JS key order), not re-serialized.
        Outcome::Hits(found) if images => {
            let row = |file: &str, meta: &str| {
                format!(
                    "{{\"file\":{},\"kind\":\"image\",\"meta\":{meta}",
                    json!(file)
                )
            };
            let text: Vec<Value> = found.text.into_iter().map(hit).collect();
            let images: Vec<String> = found
                .images
                .iter()
                .map(|i| {
                    format!(
                        "{},\"label\":{},\"hash\":{},\"score\":{}}}",
                        row(&i.row.file, &i.row.meta),
                        json!(i.label),
                        json!(i.hash),
                        json!(i.score)
                    )
                })
                .collect();
            let indexed: Vec<String> = found
                .indexed
                .iter()
                .map(|r| format!("{}}}", row(&r.file, &r.meta)))
                .collect();
            (
                200,
                format!(
                    "{{\"text\":{},\"images\":[{}],\"indexed\":[{}]}}",
                    Value::Array(text),
                    images.join(","),
                    indexed.join(",")
                ),
            )
        }
        Outcome::Hits(found) => (
            200,
            json!({ "hits": found.text.into_iter().map(hit).collect::<Vec<_>>() }).to_string(),
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
