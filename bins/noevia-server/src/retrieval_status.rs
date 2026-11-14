//! Native retrieval readiness for `GET /api/health` (council p11, pool item 4): core `rag.cjs`
//! `ragAvailable` + `retrievalStatus` and core `rust-health.cjs` `ragState`, answered in the front
//! instead of read from the `rag-status.json` snapshot Node publishes. Switch `NOEVIA_RUST_RETRIEVAL`
//! (the one variable of track R4, [`crate::native_retrieval::Mode`]):
//! - `off` (the default): the route reads Node's snapshot, as before; nothing here runs;
//! - `shadow`: the route still answers Node's snapshot; this probe runs beside it, detached, and a
//!   difference is logged (the two words only);
//! - `on`: the route answers this probe's state; Node's snapshot is not read and not asked for.
//!
//! The behaviour copied (core `server/retrieval-status.test.cjs` is the oracle, every case is a test
//! in `retrieval_status/tests.rs`):
//! - the publisher's build: while the embedding owner is not ready the state is
//!   `{ragAvailable: false, retrieval: "unavailable"}` (`ownerReady`); in the front the owner is the
//!   front's own embedding background ([`crate::embedding_background::running`]);
//! - `ragAvailable` = the vector extension registers (`loadDeps().broken`); without it
//!   `retrieval` is `unavailable` and nothing is probed;
//! - `off` when the administrator turned the managed embedding service off and the active target
//!   embeds on it or on the shared engine (`embeddingOff`, over the worker's published service
//!   observation as the native search reads it, `project_retrieval::policy::embedding_off`);
//! - otherwise one probe, cached for `RETRIEVAL_PROBE_TTL_MS` (default 45 s, clamped 200..300000),
//!   bounded by `RETRIEVAL_PROBE_TIMEOUT_MS` (default 2.5 s, clamped 200..5000; the reply body counts
//!   inside the bound), shared by concurrent callers (one request at a time; a caller that waited
//!   reads what the probe it waited for found);
//! - an active target with its own address (a dedicated embedding service): `POST
//!   <address>/v1/embeddings` with the fixed synthetic text `noevia health probe` framed as a query
//!   for the target's profile and the target's model, no maintenance lease, no credentials;
//!   `available` when the reply is a 2xx with at least one item whose `embedding` is a non-empty
//!   array (the reply capped at 1 MiB);
//! - no address of its own (the shared inference engine): `GET <inference>/v1/models` with the
//!   inference bearer (the same environment rules as the default provider's probe), never a body,
//!   `available` when the target's model is among the listed ids (capped at 2 MiB). While the
//!   maintenance lease is held nothing is sent and the last real finding stands (`available`
//!   when there is none yet), and that answer is cached like a finding, as Node caches it;
//! - an unknown profile is `degraded` without a request; a redirect, a non-2xx, a reply over its cap,
//!   unreadable JSON or the timeout are `degraded`.
//!
//! Named differences from Node:
//! - Node's cache is dropped by `rag.init()` (a reconfiguration). Here the cache is keyed by the
//!   active target (profile, model, address): a change of any of them probes again; nothing else
//!   drops it.
//! - Node's `ownerReady` is Node's own view of its paired native owner; here it is whether this
//!   front's embedding background started.
//! - an embedding configuration that cannot be read is `degraded` here (Node holds the last good
//!   configuration in memory and probes that).
//! - Node's `embeddingOff` also sees its own in-flight service change (and the 6 s after it); this
//!   reads what the worker last published, as the native search does.
use std::future::Future;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use embedding_worker::{runtime, Config as EmbeddingConfig, Target};
use project_retrieval::policy::{current_snapshot, embedding_off, observe, Observation};
use prompt_framing::embedding_input::{self, Kind, Profile};
use prompt_framing::js::units;

/// `RETRIEVAL_PROBE_TEXT`: a fixed synthetic string, never project or chat content.
pub const PROBE_TEXT: &str = "noevia health probe";
/// `readCappedJson(res, 1024 * 1024)` for the embedding service's answer.
pub const SIDECAR_CAP: usize = 1024 * 1024;
/// `readCappedJson(res, 2 * 1024 * 1024)` for the engine's model list.
pub const ROUTER_CAP: usize = 2 * 1024 * 1024;

/// One of core's four words (`rust-health.cjs` `WORDS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    Available,
    Degraded,
    Off,
    Unavailable,
}

impl Word {
    pub fn as_str(self) -> &'static str {
        match self {
            Word::Available => "available",
            Word::Degraded => "degraded",
            Word::Off => "off",
            Word::Unavailable => "unavailable",
        }
    }
}

/// What one probe found. `Held`: the maintenance lease was held, nothing was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    Up,
    Down,
    Held,
}

/// The one request a probe makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `POST url` with this JSON body, no credentials; up when an item carries a vector.
    Sidecar { url: String, body: String },
    /// `GET url` with the bearer (if any); up when `model` is a listed id.
    Router {
        url: String,
        bearer: Option<String>,
        model: String,
    },
}

/// core `rag.cjs` `clampInt(v, dflt, lo, hi)`: JS `parseInt(v, 10)` (leading whitespace, a sign,
/// then decimal digits; anything else is NaN = the default), then clamped.
pub fn clamp_int(value: Option<&str>, default: i64, lo: i64, hi: i64) -> i64 {
    let Some(raw) = value else { return default };
    let s = raw.trim_start_matches(|c: char| crate_js_space(c));
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        _ => (false, s),
    };
    let run: String = digits.chars().take_while(char::is_ascii_digit).collect();
    if run.is_empty() {
        return default;
    }
    // A run too long for i64 is far outside every clamp range anyway.
    let n = run.parse::<i64>().unwrap_or(i64::MAX);
    let n = if negative { -n } else { n };
    n.clamp(lo, hi)
}

/// JS `String.prototype.trim` whitespace (what `parseInt` skips first).
fn crate_js_space(c: char) -> bool {
    let mut buf = [0u16; 2];
    c.encode_utf16(&mut buf)
        .first()
        .is_some_and(|u| prompt_framing::js::is_js_space(*u))
}

/// The probe's bounds, read once from the environment as `rag.cjs` reads them at load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Knobs {
    pub timeout: Duration,
    pub ttl_ms: i64,
}

impl Knobs {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Knobs {
        let timeout = clamp_int(get("RETRIEVAL_PROBE_TIMEOUT_MS").as_deref(), 2500, 200, 5000);
        let ttl_ms = clamp_int(get("RETRIEVAL_PROBE_TTL_MS").as_deref(), 45000, 200, 300_000);
        Knobs {
            timeout: Duration::from_millis(timeout.unsigned_abs()),
            ttl_ms,
        }
    }
}

/// `base.replace(/\/+$/, '').replace(/\/v1$/, '')`.
fn api_root(base: &str) -> &str {
    let trimmed = base.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed)
}

/// The request for the active target, or `None` when its profile is unknown (Node's `frame` throws:
/// `degraded`, nothing is sent). `router` is the engine's model list URL and bearer.
pub fn request_for(target: &Target, router: &(String, Option<String>)) -> Option<Request> {
    let profile = Profile::try_from(target.profile.as_str()).ok()?;
    match target.base_url.as_deref().filter(|b| !b.is_empty()) {
        Some(base) => {
            let framed = embedding_input::frame(profile, Kind::Query, &units(PROBE_TEXT), None);
            let input = String::from_utf16(&framed).ok()?;
            // `JSON.stringify({ model, input })`: model first.
            let body = format!(
                "{{\"model\":{},\"input\":[{}]}}",
                serde_json::Value::String(target.model.clone()),
                serde_json::Value::String(input)
            );
            Some(Request::Sidecar {
                url: format!("{}/v1/embeddings", api_root(base)),
                body,
            })
        }
        None => Some(Request::Router {
            url: router.0.clone(),
            bearer: router.1.clone(),
            model: target.model.clone(),
        }),
    }
}

/// Whether an answer of the request is "up": the sidecar's `data[0].embedding` is a non-empty
/// array, or the target's model is one of the router's listed ids.
pub fn answer_up(request: &Request, body: &serde_json::Value) -> bool {
    let items = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    match request {
        Request::Sidecar { .. } => items
            .first()
            .and_then(|i| i.get("embedding"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|v| !v.is_empty()),
        Request::Router { model, .. } => items
            .iter()
            .any(|m| m.get("id").and_then(serde_json::Value::as_str) == Some(model.as_str())),
    }
}

#[derive(Clone, Debug)]
struct Cached {
    word: Word,
    at_ms: i64,
    key: Target,
}

/// One process's retrieval readiness: the probe cache and the observed embedding service.
pub struct Status {
    pub knobs: Knobs,
    cache: Mutex<Option<Cached>>,
    /// One probe at a time; a caller that waited reads the cache the probe filled.
    flight: tokio::sync::Mutex<()>,
    observed: Mutex<Option<Observation>>,
}

impl std::fmt::Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Status")
            .field("knobs", &self.knobs)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// What one readiness read looks at besides the cache.
pub struct Inputs<'a> {
    pub data_dir: &'a Path,
    /// This process's start id (whose embedding worker snapshot counts).
    pub start: &'a str,
    /// Milliseconds since the epoch; read before the cache check and again after a probe.
    pub clock: &'a (dyn Fn() -> i64 + Sync),
    /// `ownerReady()`.
    pub owner_ready: bool,
    /// `ragAvailable()`: the vector extension registers.
    pub deps_ok: bool,
    /// The environment's embedding target (`runtime::seed`), used while no saved one exists.
    pub seed: Option<&'a EmbeddingConfig>,
    /// `EMBEDDING_SERVICE_URL`.
    pub service_url: Option<&'a str>,
    /// Whether the maintenance lease is held (asked only on the engine path).
    pub maintenance_held: &'a (dyn Fn() -> bool + Sync),
    /// The engine's model list URL and bearer ([`crate::health_routes::default_provider_probe`]).
    pub router: (String, Option<String>),
}

/// The files a read looks at, read on the blocking pool.
struct Disk {
    config: Result<EmbeddingConfig, String>,
    status: Option<serde_json::Value>,
}

impl Status {
    pub fn new(knobs: Knobs) -> Status {
        Status {
            knobs,
            cache: Mutex::new(None),
            flight: tokio::sync::Mutex::new(()),
            observed: Mutex::new(None),
        }
    }

    fn fresh(&self, key: &Target, now_ms: i64) -> Option<Word> {
        lock(&self.cache)
            .as_ref()
            .filter(|c| &c.key == key && now_ms - c.at_ms < self.knobs.ttl_ms)
            .map(|c| c.word)
    }

    /// `ragState(rag)` under the publisher's `ownerReady()`: `(ragAvailable, retrieval)`. `probe`
    /// sends the one request and answers what it found (`Found::Up`/`Down`); it is called at most
    /// once, never while the maintenance lease is held on the engine path, and never for a cached
    /// answer.
    pub async fn state<P, F>(&self, inputs: Inputs<'_>, probe: P) -> (bool, Word)
    where
        P: FnOnce(Request) -> F,
        F: Future<Output = Found>,
    {
        if !inputs.owner_ready {
            return (false, Word::Unavailable);
        }
        if !inputs.deps_ok {
            return (false, Word::Unavailable);
        }
        let seed = inputs.seed.cloned().unwrap_or_else(runtime::invalid_seed);
        let dir = inputs.data_dir.to_path_buf();
        let disk = tokio::task::spawn_blocking(move || Disk {
            config: runtime::load_config(&dir, &seed),
            status: std::fs::read(dir.join("embedding-worker-status.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok()),
        })
        .await;
        let Ok(Disk { config: Ok(config), status }) = disk else {
            return (true, Word::Degraded);
        };
        let target = config.active;
        let now = (inputs.clock)();
        let last = {
            let mut observed = lock(&self.observed);
            let snapshot = status
                .as_ref()
                .and_then(|s| current_snapshot(s, inputs.start, now as f64));
            *observed = observe(observed.take(), snapshot, now as f64);
            observed.clone()
        };
        if embedding_off(
            last.as_ref(),
            now as f64,
            inputs.service_url,
            target.base_url.as_deref(),
        ) {
            return (true, Word::Off);
        }
        if let Some(word) = self.fresh(&target, now) {
            return (true, word);
        }
        let _flight = self.flight.lock().await;
        // A probe this caller waited for has just filled the cache.
        if let Some(word) = self.fresh(&target, (inputs.clock)()) {
            return (true, word);
        }
        let found = match request_for(&target, &inputs.router) {
            None => Found::Down,
            Some(Request::Router { .. }) if (inputs.maintenance_held)() => Found::Held,
            Some(request) => probe(request).await,
        };
        let word = match found {
            Found::Up => Word::Available,
            Found::Down => Word::Degraded,
            // Nothing was probed: the last real finding stands, or `available` until one runs.
            Found::Held => lock(&self.cache)
                .as_ref()
                .filter(|c| c.key == target)
                .map_or(Word::Available, |c| c.word),
        };
        *lock(&self.cache) = Some(Cached {
            word,
            at_ms: (inputs.clock)(),
            key: target,
        });
        (true, word)
    }
}

fn client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()
        })
        .as_ref()
}

/// Sends the request within `timeout` (the reply body inside the bound) and reads it under its cap.
pub async fn send(request: Request, timeout: Duration) -> Found {
    let Some(client) = client() else {
        return Found::Down;
    };
    let (builder, cap) = match &request {
        Request::Sidecar { url, body } => (
            client
                .post(url.as_str())
                .header("Content-Type", "application/json")
                .body(body.clone()),
            SIDECAR_CAP,
        ),
        Request::Router { url, bearer, .. } => {
            let mut b = client
                .get(url.as_str())
                .header("Content-Type", "application/json");
            if let Some(key) = bearer {
                b = b.header("Authorization", format!("Bearer {key}"));
            }
            (b, ROUTER_CAP)
        }
    };
    let read = async {
        let mut response = builder.send().await.ok()?;
        // `redirect: 'error'` and `!res.ok`: a 3xx (never followed) or any non-2xx is down.
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > cap {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<serde_json::Value>(&body).ok()
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(Some(value)) if answer_up(&request, &value) => Found::Up,
        _ => Found::Down,
    }
}

/// This process's readiness, its bounds from the environment.
pub fn process() -> &'static Status {
    static STATUS: OnceLock<Status> = OnceLock::new();
    STATUS.get_or_init(|| Status::new(Knobs::from_lookup(&|k| std::env::var(k).ok())))
}

/// The state for the health route, with this process's inputs.
pub async fn for_app(app: &crate::App, data_dir: &Path) -> (bool, Word) {
    let get = |k: &str| std::env::var(k).ok();
    let seed = runtime::seed(get).ok();
    let service_url = get("EMBEDDING_SERVICE_URL");
    let held = || app.config.maint.authority.held();
    let status = process();
    let deps_ok = embedding_worker::sqlite_vec_loader::register().is_ok();
    status
        .state(
            Inputs {
                data_dir,
                start: app.config.start_id.as_deref().unwrap_or_default(),
                clock: &crate::identity::now_ms,
                owner_ready: crate::embedding_background::running(),
                deps_ok,
                seed: seed.as_ref(),
                service_url: service_url.as_deref(),
                maintenance_held: &held,
                router: crate::health_routes::default_provider_probe(&get),
            },
            |request| send(request, status.knobs.timeout),
        )
        .await
}

#[cfg(test)]
mod tests;
