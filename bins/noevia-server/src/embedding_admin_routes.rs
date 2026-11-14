//! Track R11 (S10b): `/api/embedding` answered by the front (crates/embedding-migration over this
//! host), behind `NOEVIA_RUST_EMBEDDING_ADMIN`, which is off by default: unset, every method of the
//! path goes to core exactly as before (its route row stays owner = "node"). With `rust`, lib.rs
//! hands the exact WHATWG pathname to [`serve`], whatever the method, and core (told the same
//! switch) refuses the path before authentication and re-reads `embedding-config.json` whenever
//! the file changes, so the front's tmp + rename write is what core uses next.
//!
//! What is outside the crate, as core index.cjs wires it:
//! - The configuration: `embedding-config.json` read on every request (core re-reads it too), the
//!   environment's seed when there is none, and the start-up address guard of the seeded targets
//!   (`guardSeededAddresses`) applied on every request. A change takes the config lock the native
//!   worker publishes under (`server_projects::lock`, `<file>.lock`), re-checks under it that the
//!   file is still the one read, and writes by tmp + rename.
//! - The native worker: its status file as `nativeOwner.snapshot()` reads it (protocol, this start,
//!   at most 15 s old), its request file as `nativeOwner.kick()` writes it (the worker polls it; a
//!   sequence from the clock, so a value core wrote earlier is never repeated), the index files
//!   (`SqlStore`).
//! - The embeddings servers (`/v1/models` and the parity embeds through the worker's `Http`, whose
//!   resolver keeps every request on this network), the Diary sidecar (`/api/internal/embedding`,
//!   service bearer only), the model manager (`/api/v1/embedding-service`, `/api/v1/models`) and
//!   the audit log.
//!
//! Every POST runs one at a time (the turn), so a change never races another; a GET does not wait
//! for it. Named differences from core, each pinned by tests/embedding_admin.rs:
//! - The vector slots a change drops are dropped right after its hold is released, each under its
//!   own config hold (the worker's `drop_slot` takes the config lock itself and refuses to drop the
//!   active or previous target as the file then says); core drops them inside the hold.
//! - A configuration file changed by someone else between the read and the hold refuses the change
//!   (500, nothing written); core is the file's only writer and has no such case.
//! - The parity check embeds the four sample texts one request each (the worker's single-text
//!   embed); its error text is the worker's fixed label, not core's fetch message.
//! - A failed audit write is logged and the answer stands (core's throw answered 500 after the
//!   change was made).
//! - A request body this port cannot hold as JS would is answered as invalid JSON.
//! - A configuration file that exists but cannot be read is damaged (core's lenient read takes it
//!   as missing): embedding is off, the GET says so and every change that writes refuses with 409
//!   `badState`, so the environment's seed is never written over saved state.
//! - The vector slots a change drops run before the worker is kicked (a drop that fails is tried
//!   once more), so the worker the kick wakes does not take the config lock first.
//! - The start-up address guard's verdict for a seeded address is kept for [`VERDICT_TTL`] (the
//!   check may ask DNS); the active and next addresses are checked together.
//! - Around every model manager PUT the front writes the service intent file
//!   (`embedding-service-intent.json`, protocol `rust-embedding-service-intent-v1`): the models
//!   asked for, `done: false` before the PUT, `done: true` once it returned, whatever it answered.
//!   Core's delete guard protects those models meanwhile (the worker's snapshot is up to 10 s old).
//!   When the first write fails, nothing is asked: the change is refused (503 `serviceRefused`).

use crate::{identity, reply, App};
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use embedding_migration::config::{read_state_strict, write_atomic};
use embedding_migration::routes::{diary_url, DIARY_TIMEOUT_MS};
use embedding_migration::target::PROFILES;
use embedding_migration::{
    Config as Saved, Coverage, Embedding, Host, HttpReply, Migration, Pacing, ServeError, Source,
    State as Targets, Target, User,
};
use embedding_worker::{http::Http, runtime, sqlite::SqlStore, Index};
use js_json::{to_js_string, wtf16, JValue};
use project_retrieval::policy::{current_snapshot, same_address};
use prompt_framing::embedding_input::{frame, Kind, Profile};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The switch.
pub const SWITCH: &str = "NOEVIA_RUST_EMBEDDING_ADMIN";
pub const PATH: &str = "/api/embedding";
const CONFIG_FILE: &str = "embedding-config.json";
/// http.cjs `readBody`'s default limit.
const BODY_LIMIT: usize = 1024 * 1024;
/// fetchJson's answer cap (http.cjs `DEFAULT_MAX_RESPONSE_BYTES`).
const ANSWER_CAP: usize = 8 * 1024 * 1024;
/// embedding-service.cjs `STALE_MS`.
const STALE_MS: f64 = 15_000.0;
const SERVICE_PATH: &str = "/api/v1/embedding-service";
/// What an aborted `fetch` throws (fetchJson's timer).
const ABORTED: &str = "This operation was aborted";
const FETCH_FAILED: &str = "fetch failed";
const LOCK_UNAVAILABLE: &str = "Embedding config lock unavailable";
const CHANGED: &str =
    "The embedding configuration changed while this change was being checked. Nothing was changed.";
/// The service intent file core's delete guard reads while a service change of the front's runs.
pub const INTENT_FILE: &str = "embedding-service-intent.json";
pub const INTENT_PROTOCOL: &str = "rust-embedding-service-intent-v1";
const NOT_RECORDED: &str =
    "The embedding service could not be changed: the change could not be recorded.";
/// How long the start-up address guard's verdict on a seeded address is kept.
pub const VERDICT_TTL: Duration = Duration::from_secs(60);

/// The environment the answers read (core reads `process.env`).
const ENV: [&str; 17] = [
    "EMBEDDING_BASE_URL",
    "EMBEDDING_MODEL",
    "EMBED_MODEL",
    "EMBEDDING_PROFILE",
    "EMBEDDING_NEXT_MODEL",
    "EMBEDDING_NEXT_PROFILE",
    "EMBEDDING_NEXT_BASE_URL",
    "EMBEDDING_REINDEX_DUTY",
    "EMBEDDING_REINDEX_BATCH",
    "EMBEDDING_SERVICE_URL",
    "MODEL_LOADER_URL",
    "MODEL_LOADER_TOKEN",
    "INFERENCE_BASE_URL",
    "LEMONADE_BASE_URL",
    "INFERENCE_API_KEY",
    "LEMONADE_API_KEY",
    "RAG_DIR",
];

/// The switch and the environment the answers read.
#[derive(Clone)]
pub struct Settings {
    pub on: bool,
    env: BTreeMap<&'static str, String>,
    /// `waitTimeoutMs` (180 s) and `POLL_MS` (2 s) of `service-switch`; tests shorten them.
    pub wait_ms: u64,
    pub poll_ms: u64,
    /// The model manager PUT's deadline (`serve`'s 60 s); tests shorten it.
    pub put_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            on: false,
            env: BTreeMap::new(),
            wait_ms: embedding_migration::routes::WAIT_TIMEOUT_MS,
            poll_ms: 2_000,
            put_ms: 60_000,
        }
    }
}

impl std::fmt::Debug for Settings {
    // Names only: never a token, a key or an address.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("on", &self.on)
            .field("set", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        Settings {
            on: get(SWITCH).as_deref() == Some("rust"),
            env: ENV.iter().filter_map(|k| Some((*k, get(k)?))).collect(),
            ..Settings::default()
        }
    }

    /// Whether the front answers this request: the exact pathname, every method.
    pub fn answers(&self, pathname: Option<&str>) -> bool {
        self.on && pathname == Some(PATH)
    }

    fn get(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    /// `String(env.MODEL_LOADER_URL || '').replace(/\/+$/, '')`.
    fn loader(&self) -> String {
        self.get("MODEL_LOADER_URL")
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string()
    }

    /// `EMBEDDING_SERVICE_URL` trimmed, without trailing slashes, or `None`.
    fn service_base(&self) -> Option<String> {
        let raw = self.get("EMBEDDING_SERVICE_URL").unwrap_or_default();
        let base = chat_turn::js::trim(&raw).trim_end_matches('/');
        (!base.is_empty()).then(|| base.to_string())
    }

    fn first(&self, keys: [&str; 2]) -> Option<String> {
        keys.iter()
            .find_map(|k| self.get(k).filter(|v| !v.is_empty()))
    }
}

/// What the route keeps between requests: the turn every POST takes (core's single event loop and
/// `serialized`), the embedding service as last seen (embedding-service.cjs), the kick sequence,
/// the seeded addresses' verdicts.
#[derive(Default)]
pub struct State {
    turn: Arc<tokio::sync::Mutex<()>>,
    service: Mutex<Service>,
    sequence: AtomicU64,
    verdicts: Verdicts,
}

/// `checkEmbeddingAddress`'s verdict: `Err((reason, transient))`.
pub type Verdict = Result<(), (String, bool)>;

/// The start-up address guard's verdicts by URL, each kept for [`VERDICT_TTL`].
#[derive(Default)]
pub struct Verdicts(Mutex<HashMap<String, (Instant, Verdict)>>);

impl Verdicts {
    fn map(&self) -> MutexGuard<'_, HashMap<String, (Instant, Verdict)>> {
        match self.0.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// The verdict on `url` as of `at`: a kept one younger than [`VERDICT_TTL`], else `check`'s
    /// (then kept, and every expired one forgotten).
    pub async fn verdict<F, Fut>(&self, url: &str, at: Instant, check: F) -> Verdict
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Verdict>,
    {
        let fresh = |when: &Instant| at.saturating_duration_since(*when) < VERDICT_TTL;
        if let Some((_, v)) = self.map().get(url).filter(|(when, _)| fresh(when)) {
            return v.clone();
        }
        let v = check(url.to_string()).await;
        let mut map = self.map();
        map.retain(|_, (when, _)| fresh(when));
        map.insert(url.to_string(), (at, v.clone()));
        v
    }
}

/// The refusal of a seeded address, `(url, (reason, transient))`, or `None` when it may be used.
async fn seeded_refusal(
    verdicts: &Verdicts,
    url: Option<String>,
    at: Instant,
) -> Option<(String, (String, bool))> {
    let url = url?;
    let refused = verdicts
        .verdict(&url, at, |u| async move { check_address(&u).await })
        .await
        .err()?;
    Some((url, refused))
}

/// What a change leaves for after its config hold: the vector slots it dropped (the worker's
/// `drop_slot` takes the config lock itself) and the worker's kick.
#[derive(Default)]
struct Deferred {
    drops: Vec<(Index, embedding_worker::Target)>,
    kick: bool,
}

impl Deferred {
    /// Every drop, a failed one once more (the store's errors are one fixed label: a busy lock
    /// cannot be told from anything else), then the kick, so the worker it wakes does not take the
    /// config lock before the drops.
    fn run(
        &mut self,
        mut drop_slot: impl FnMut(&Index, &embedding_worker::Target) -> Result<(), String>,
        kick: impl FnOnce(),
    ) {
        for (index, target) in std::mem::take(&mut self.drops) {
            if drop_slot(&index, &target).is_err() && drop_slot(&index, &target).is_err() {
                eprintln!("[embedding] a vector slot could not be dropped");
            }
        }
        if std::mem::take(&mut self.kick) {
            kick();
        }
    }
}

/// embedding-service.cjs's state with a `nativeOwner` (`protection` is the delete guard's, not
/// this route's).
#[derive(Default)]
struct Service {
    last: Option<(f64, JValue)>,
    last_error: Option<String>,
    generation: u64,
    mutations: u32,
    mutation_at: f64,
}

fn now() -> f64 {
    identity::now_ms() as f64
}

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        wtf16::stringify(body).unwrap_or_default(),
        true,
    )
}

fn error(status: u16, text: &str) -> Response<Body> {
    answer(status, &JValue::obj([("error", JValue::from(text))]))
}

/// `String(v)` (an object with its own `toString` reads as `[object Object]`).
fn string_of(v: &JValue) -> String {
    to_js_string(v).unwrap_or_else(|_| "[object Object]".into())
}

pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let at = identity::now_ms();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &path, at)).await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let admin = id.user.role == server_auth::Role::Admin;
    if !admin {
        // The route's own answer, before any file, server or turn is touched.
        return error(403, "Administrator required");
    }
    let post = parts.method == Method::POST;
    // `readJson(req)`: empty is `{}`, too large or not JSON is the route's 400 `invalid JSON`.
    let parsed: Result<JValue, ()> = if post {
        match axum::body::to_bytes(body, BODY_LIMIT + 1).await {
            Ok(b) if b.is_empty() => Ok(JValue::Obj(Vec::new())),
            Ok(b) if b.len() <= BODY_LIMIT => {
                wtf16::parse(&String::from_utf8_lossy(&b)).map_err(|_| ())
            }
            _ => Err(()),
        }
    } else {
        Ok(JValue::Undefined)
    };
    let Some(root) = app.config.data_dir.clone() else {
        return error(503, "Embedding settings unavailable");
    };
    let turn = if post {
        Some(Arc::clone(&app.embedding_admin.turn).lock_owned().await)
    } else {
        None
    };
    let (app, rt) = (Arc::clone(app), tokio::runtime::Handle::current());
    let method = parts.method.as_str().to_string();
    let user = id.user.id.clone();
    let work = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        let mut front = Front::new(app, rt, root);
        let mut embedding = front.embedding();
        let user = User { id: &user, admin };
        let reply = embedding.handle(
            &mut front,
            &method,
            Some(user),
            parsed.as_ref().map_err(|_| ()),
        );
        // Every path releases the hold and runs what it dropped; this is the backstop.
        front.unlock();
        reply
    })
    .await;
    match work {
        Ok(reply) => answer(reply.status, &reply.body),
        Err(_) => error(500, "Internal error"),
    }
}

/// The crate's host: this front's files, servers and audit log, each async step run to its end.
struct Front {
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    root: PathBuf,
    /// The configuration file as read for this request; a hold refuses when it is not that.
    expected: Option<Targets>,
    held: Option<server_projects::lock::FileLock>,
    deferred: Deferred,
    store: SqlStore,
    http: Option<Http>,
}

fn worker_target(t: &Target) -> embedding_worker::Target {
    embedding_worker::Target {
        profile: t.profile.clone(),
        model: t.model.clone(),
        base_url: t.base_url.clone(),
    }
}

impl Front {
    fn new(app: Arc<App>, rt: tokio::runtime::Handle, root: PathBuf) -> Front {
        let s = &app.config.embedding_admin;
        let get = |k: &str| s.get(k);
        // The worker's own seed and endpoints (embedding_background.rs).
        let store = SqlStore {
            rag_dir: s
                .get("RAG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            root: root.clone(),
            seed: runtime::seed(get).unwrap_or_else(|_| runtime::invalid_seed()),
            open_vec: embedding_worker::sqlite::open_vec_required,
        };
        let inference = s
            .first(["INFERENCE_BASE_URL", "LEMONADE_BASE_URL"])
            .unwrap_or_else(|| "http://host.docker.internal:11434".into());
        let token = s
            .first(["INFERENCE_API_KEY", "LEMONADE_API_KEY"])
            .filter(|t| t != "local");
        let http = Http::new(&s.loader(), s.get("MODEL_LOADER_TOKEN"), &inference, token).ok();
        Front {
            store,
            http,
            rt,
            root,
            expected: None,
            held: None,
            deferred: Deferred::default(),
            app,
        }
    }

    fn settings(&self) -> &Settings {
        &self.app.config.embedding_admin
    }

    /// The configuration as core holds it now: the saved file, else the environment's targets with
    /// the start-up address guard applied (`refuseSeeded`).
    fn embedding(&mut self) -> Embedding {
        let s = self.settings().clone();
        let env = |k: &str| s.get(k);
        let (mut config, _) = Saved::load_strict(self.root.join(CONFIG_FILE), &env);
        self.expected = (config.source() == Source::Saved).then(|| config.state().clone());
        if config.source() == Source::Environment && config.damaged().is_none() {
            let url_of = |t: Option<&Target>| t?.base_url.clone().filter(|u| !u.is_empty());
            let (active, next) = (url_of(Some(config.active())), url_of(config.next()));
            let (verdicts, at) = (&self.app.embedding_admin.verdicts, Instant::now());
            let (active, next) = self.rt.block_on(async {
                tokio::join!(
                    seeded_refusal(verdicts, active, at),
                    seeded_refusal(verdicts, next, at)
                )
            });
            let reason = |name: &str, refused: Option<(String, (String, bool))>| {
                let (url, (why, transient)) = refused?;
                let how = if transient { "not used yet" } else { "refused" };
                Some((format!("{name} {url} {how}: {why}"), transient))
            };
            if let Some((why, transient)) = reason("EMBEDDING_BASE_URL", active) {
                config.refuse_seeded_as(Some(&why), None, transient);
            }
            if let Some((why, transient)) = reason("EMBEDDING_NEXT_BASE_URL", next) {
                config.refuse_seeded_as(None, Some(&why), transient);
            }
        }
        let mut embedding = Embedding::new(
            Migration::new(config, Pacing::from_env(&env)),
            diary_url(&self.app.config.diary.base),
        );
        embedding.wait_timeout_ms = s.wait_ms;
        embedding
    }

    fn service(&self) -> MutexGuard<'_, Service> {
        match self.app.embedding_admin.service.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn read_snapshot(&self) -> Option<JValue> {
        let start = self.app.config.start_id.as_deref()?;
        let bytes = std::fs::read(self.root.join("embedding-worker-status.json")).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        current_snapshot(&value, start, now())?;
        wtf16::parse(&String::from_utf8_lossy(&bytes)).ok()
    }

    /// `observeNative()`: the worker's last view of the service, unless a change of ours is newer.
    fn observe(&mut self) {
        let Some(s) = self.read_snapshot() else {
            return;
        };
        let (service, at) = (s.get("service"), s.get("serviceAt").as_num());
        let Some(at) = at.filter(|a| a.is_finite() && *a <= now()) else {
            return;
        };
        let mut sv = self.service();
        if !service.truthy()
            || sv.mutations > 0
            || at <= sv.mutation_at + 6000.0
            || sv.last.as_ref().is_some_and(|(last, _)| at < *last)
        {
            return;
        }
        sv.last = Some((at, service.clone()));
    }

    /// `refresh({ direct: true })`: the model manager asked now.
    fn refresh_direct(&mut self) -> Option<JValue> {
        let s = self.settings().clone();
        if s.loader().is_empty() {
            return None;
        }
        let generation = self.service().generation;
        let r = self
            .rt
            .block_on(loader(&s, Method::GET, SERVICE_PATH, None, 6_000));
        let mut sv = self.service();
        match r {
            Ok(r) if r.ok && r.body.get("managed") == &JValue::Bool(true) => {
                if generation == sv.generation && sv.mutations == 0 {
                    sv.last = Some((now(), r.body));
                    sv.last_error = None;
                }
            }
            Ok(r) if r.status == 404 && sv.last.is_none() => {
                sv.last_error = Some("notManaged".into())
            }
            Ok(r) => sv.last_error = Some(format!("HTTP {}", r.status)),
            Err(message) => sv.last_error = Some(wtf16::slice(&message, 200)),
        }
        sv.last.as_ref().map(|(_, body)| body.clone())
    }

    /// `rag.cjs` `embeddingOff(target)`.
    fn embedding_off(&mut self, t: &Target) -> bool {
        if !self.service_managed() || self.service_enabled() == Some(true) {
            return false;
        }
        t.base_url.as_deref().is_none_or(str::is_empty)
            || self.service_is_at(&t.base_url.clone().into())
    }

    /// `nativeOwner.kick()`: the request file the worker polls.
    fn send_kick(&self) {
        let start = self.app.config.start_id.clone().unwrap_or_default();
        let clock = now() as u64;
        let sequence = &self.app.embedding_admin.sequence;
        // From the clock and rising: never a sequence core's counter (from 1) wrote before.
        sequence.fetch_max(clock, Ordering::AcqRel);
        let n = sequence.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        let text = serde_json::json!({"start": start, "sequence": n}).to_string();
        let _ = write_atomic(
            &self.root.join("embedding-worker-request.json"),
            text.as_bytes(),
        );
    }

    /// The service intent: the models a model manager PUT asks for, `done` once it returned.
    fn write_intent(&self, models: &[JValue], done: bool) -> Result<(), ()> {
        let start = self.app.config.start_id.clone().unwrap_or_default();
        let intent = JValue::obj([
            ("protocol", INTENT_PROTOCOL.into()),
            ("start", start.into()),
            ("at", JValue::Num(now())),
            ("models", JValue::Arr(models.to_vec())),
            ("done", JValue::Bool(done)),
        ]);
        let text = wtf16::stringify(&intent).ok_or(())?;
        write_atomic(&self.root.join(INTENT_FILE), text.as_bytes()).map_err(|_| ())
    }
}

impl Host for Front {
    type Index = Index;

    fn lock(&mut self) -> Result<(), String> {
        let file = self.root.join(CONFIG_FILE);
        let _ = std::fs::create_dir_all(&self.root);
        let held = server_projects::lock::acquire(&file).map_err(|_| LOCK_UNAVAILABLE)?;
        if read_state_strict(&file) != Ok(self.expected.clone()) {
            return Err(CHANGED.into());
        }
        self.held = Some(held);
        Ok(())
    }

    fn unlock(&mut self) {
        if self.held.take().is_some() {
            self.expected = read_state_strict(&self.root.join(CONFIG_FILE))
                .ok()
                .flatten();
        }
        let mut deferred = std::mem::take(&mut self.deferred);
        let store = &self.store;
        deferred.run(
            |index, target| store.drop_slot(index, target),
            || self.send_kick(),
        );
    }

    /// `nativeOwner.kick()`; inside a hold it is sent once the hold's drops have run.
    fn kick(&mut self) {
        if self.held.is_some() || !self.deferred.drops.is_empty() {
            self.deferred.kick = true;
        } else {
            self.send_kick();
        }
    }

    fn snapshot(&mut self) -> Option<JValue> {
        self.read_snapshot()
    }

    fn indexes(&mut self) -> Vec<Index> {
        self.store.indexes()
    }

    fn coverage(&mut self, index: &Index, target: &Target) -> Option<Coverage> {
        let (chunks, vectors) = self.store.coverage(index, &worker_target(target))?;
        Some(Coverage {
            chunks: chunks as f64,
            vectors: vectors as f64,
        })
    }

    fn drop_slot(&mut self, index: &Index, target: &Target) -> Result<(), String> {
        self.deferred
            .drops
            .push((index.clone(), worker_target(target)));
        Ok(())
    }

    fn served_models(&mut self, base_url: Option<&str>, model: &str) -> Option<Vec<String>> {
        let target = embedding_worker::Target {
            profile: PROFILES[0].into(),
            model: model.into(),
            base_url: base_url.filter(|b| !b.is_empty()).map(str::to_string),
        };
        let http = self.http.as_ref()?;
        self.rt.block_on(http.served(&target)).ok()
    }

    /// `rag.embed(texts, { kind: 'document', target })`.
    fn embed(&mut self, texts: &[&str], target: &Target) -> Result<Vec<Vec<f64>>, String> {
        if self.embedding_off(target) {
            return Err("Embedding is turned off.".into());
        }
        let profile = Profile::try_from(target.profile.as_str()).map_err(|_| {
            format!(
                "Unknown embedding profile \"{}\" (known: {})",
                target.profile,
                PROFILES.join(", ")
            )
        })?;
        let http = self.http.as_ref().ok_or(FETCH_FAILED)?;
        let t = worker_target(target);
        texts
            .iter()
            .map(|text| {
                let units: Vec<u16> = text.encode_utf16().collect();
                let framed = frame(profile, Kind::Document, &units, None);
                let v = self.rt.block_on(http.embed_query(&t, &framed))?;
                Ok(v.into_iter().map(f64::from).collect())
            })
            .collect()
    }

    fn check_address(&mut self, url: &str) -> Result<(), String> {
        self.rt.block_on(check_address(url)).map_err(|(why, _)| why)
    }

    fn diary(&mut self, url: &str, method: &str, body: Option<&str>) -> Result<HttpReply, String> {
        let diary = self.app.diary.clone().ok_or(FETCH_FAILED)?;
        let method = if method == "POST" {
            Method::POST
        } else {
            Method::GET
        };
        let body = body.map(|b| Bytes::from(b.to_string()));
        let timeout = Duration::from_millis(DIARY_TIMEOUT_MS);
        match self
            .rt
            .block_on(diary.sidecar().service_json(method, url, body, timeout))
        {
            Ok(r) => Ok(HttpReply {
                ok: r.ok,
                status: r.status,
                body: r.body.unwrap_or(JValue::Undefined),
            }),
            Err(server_diary::CallError::Timeout) => Err(ABORTED.into()),
            Err(_) => Err(FETCH_FAILED.into()),
        }
    }

    /// `authService.audit(action, user, null, detail)`.
    fn audit(&mut self, action: &str, user: &str, detail: &JValue) {
        let text = js_json::stringify(detail).unwrap_or_default();
        let written = self.app.writes.writer().and_then(|w| {
            w.write(|tx| {
                tx.execute(
                    "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,NULL,?2,?3,?4)",
                    server_store::rusqlite::params![user, action, text, identity::now_ms()],
                )?;
                Ok(())
            })
        });
        if written.is_err() {
            eprintln!("[embedding] the {action} audit record could not be written");
        }
    }

    fn service_refresh(&mut self) {
        self.observe();
    }

    fn service_managed(&mut self) -> bool {
        self.observe();
        self.service().last.is_some()
    }

    fn service_enabled(&mut self) -> Option<bool> {
        match self.service_status() {
            JValue::Null => None,
            s if s.get("stale").truthy() => None,
            s => Some(s.get("enabled") == &JValue::Bool(true)),
        }
    }

    fn service_status(&mut self) -> JValue {
        self.observe();
        let sv = self.service();
        let Some((at, body)) = &sv.last else {
            return JValue::Null;
        };
        let mut items = match body {
            JValue::Obj(items) => items.clone(),
            _ => Vec::new(),
        };
        let stale = body.get("stale").truthy() || now() - at > STALE_MS || sv.mutations > 0;
        wtf16::set(&mut items, "stale", JValue::Bool(stale));
        if let Some(e) = &sv.last_error {
            wtf16::set(&mut items, "lastError", e.clone().into());
        }
        JValue::Obj(items)
    }

    fn service_base_url(&self) -> Option<String> {
        self.settings().service_base()
    }

    fn service_is_at(&mut self, address: &JValue) -> bool {
        let Some(base) = self.settings().service_base() else {
            return false;
        };
        let address = if address.is_nullish() {
            String::new()
        } else {
            string_of(address)
        };
        same_address(Some(&address), Some(&base))
    }

    fn service_file_of(&mut self, model: &JValue) -> Option<String> {
        self.observe();
        let sv = self.service();
        let body = &sv.last.as_ref()?.1;
        let entries = [body.get("models"), body.get("previous")];
        let file = entries
            .iter()
            .filter_map(|list| match list {
                JValue::Arr(items) => Some(items),
                _ => None,
            })
            .flatten()
            .find(|e| e.get("model") == model && !model.is_nullish())?
            .get("file");
        file.truthy().then(|| string_of(file))
    }

    /// `service.serve(models)`: a PUT to the model manager; the service view is stale meanwhile.
    fn service_serve(&mut self, models: &[JValue]) -> Result<(), ServeError> {
        let s = self.settings().clone();
        if s.loader().is_empty() {
            return Err(ServeError {
                status: Some(503),
                message: "The model manager is not configured.".into(),
                code: Some("serviceUnavailable".into()),
            });
        }
        self.observe();
        // Core's delete guard protects what is asked for from before the PUT until it returned.
        if self.write_intent(models, false).is_err() {
            eprintln!("[embedding] the service intent could not be written; nothing was asked");
            return Err(ServeError {
                status: Some(503),
                message: NOT_RECORDED.into(),
                code: Some("serviceRefused".into()),
            });
        }
        {
            let mut sv = self.service();
            sv.generation += 1;
            sv.mutations += 1;
        }
        let body = wtf16::stringify(&JValue::obj([("models", JValue::Arr(models.to_vec()))]));
        let r = self
            .rt
            .block_on(loader(&s, Method::PUT, SERVICE_PATH, body, s.put_ms));
        if self.write_intent(models, true).is_err() {
            eprintln!("[embedding] the service intent could not be marked done");
        }
        {
            let mut sv = self.service();
            sv.mutations = sv.mutations.saturating_sub(1);
            sv.mutation_at = now();
            sv.generation += 1;
        }
        self.kick();
        let r = r.map_err(|message| ServeError {
            status: None,
            message,
            code: None,
        })?;
        if !r.ok {
            let said = [r.body.get("detail"), r.body.get("error")]
                .into_iter()
                .find(|v| v.truthy());
            let detail = said.map_or_else(|| format!("HTTP {}", r.status), string_of);
            return Err(ServeError {
                status: Some(if r.status == 400 { 400 } else { 502 }),
                message: format!(
                    "The embedding service could not be changed: {}",
                    wtf16::slice(&detail, 300)
                ),
                code: Some("serviceRefused".into()),
            });
        }
        if r.body.get("managed").truthy() {
            self.service().last = Some((now(), r.body));
        }
        let names: Vec<String> = models.iter().map(|m| string_of(m.get("model"))).collect();
        let what = if names.is_empty() {
            "nothing (off)".to_string()
        } else {
            names.join(", ")
        };
        println!("[embedding] service now serves {what}");
        Ok(())
    }

    /// `service.waitLoaded(names, { timeoutMs })`: polls until the router reports every name loaded.
    fn service_wait_loaded(&mut self, models: &[String], timeout_ms: u64) -> bool {
        let deadline = now() + timeout_ms as f64;
        let poll = Duration::from_millis(self.settings().poll_ms);
        loop {
            let s = self.refresh_direct();
            let loaded = match s.as_ref().map(|s| s.get("running").get("loaded")) {
                Some(JValue::Arr(items)) => items.clone(),
                _ => Vec::new(),
            };
            let has = |n: &String| loaded.iter().any(|l| l.as_str() == Some(n.as_str()));
            if s.is_some() && models.iter().all(has) {
                return true;
            }
            if now() >= deadline {
                return false;
            }
            std::thread::sleep(poll);
        }
    }

    /// `resolveModelFile({ model })`: the GGUF of an installed model, by the manager's own list.
    fn resolve_file(&mut self, model: &str) -> Option<String> {
        let s = self.settings().clone();
        let want = chat_turn::js::trim(model).to_string();
        if s.loader().is_empty() || want.is_empty() {
            return None;
        }
        let r = self
            .rt
            .block_on(loader(&s, Method::GET, "/api/v1/models", None, 8_000))
            .ok()?;
        let rows = match (&r.body, r.body.get("models")) {
            (JValue::Arr(rows), _) | (_, JValue::Arr(rows)) => rows.clone(),
            _ => return None,
        };
        let want = JValue::from(want.as_str());
        rows.iter()
            .find(|m| {
                let gguf = m
                    .get("file")
                    .as_str()
                    .is_some_and(|f| f.to_ascii_lowercase().ends_with(".gguf"));
                let sections = matches!(m.get("sections"), JValue::Arr(s) if s.contains(&want));
                m.truthy()
                    && !m.get("companion").truthy()
                    && gguf
                    && (m.get("modelId") == &want
                        || sections
                        || m.get("name") == &want
                        || m.get("stem") == &want)
            })
            .and_then(|m| m.get("file").as_str().map(str::to_string))
    }
}

/// fetchJson to the model manager: its token header, no proxy, no redirect, the 8 MiB cap.
async fn loader(
    s: &Settings,
    method: Method,
    path: &str,
    body: Option<String>,
    timeout_ms: u64,
) -> Result<HttpReply, String> {
    let failed = |e: reqwest::Error| {
        let text = if e.is_timeout() {
            ABORTED
        } else {
            FETCH_FAILED
        };
        text.to_string()
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .map_err(failed)?;
    let mut req = client
        .request(method, format!("{}{path}", s.loader()))
        .header("Content-Type", "application/json");
    if let Some(token) = s.get("MODEL_LOADER_TOKEN").filter(|t| !t.is_empty()) {
        req = req.header("X-Model-Loader-Token", token);
    }
    if let Some(body) = body {
        req = req.body(body);
    }
    let mut res = req.send().await.map_err(failed)?;
    if res.status().is_redirection() {
        return Err(FETCH_FAILED.into());
    }
    let (status, ok) = (res.status().as_u16(), res.status().is_success());
    let mut buf = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(failed)? {
        buf.extend_from_slice(&chunk);
        if buf.len() > ANSWER_CAP {
            return Ok(HttpReply {
                ok: false,
                status,
                body: JValue::Undefined,
            });
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let body = wtf16::parse(&text).unwrap_or(JValue::Str(text));
    Ok(HttpReply { ok, status, body })
}

// core embedding-address.cjs `checkEmbeddingAddress` = service-endpoint.cjs `checkServiceEndpoint`
// with any path and link-local refused, reasons word for word. (diary_retrieval's port is the Diary's
// own, Python-derived rule and wording; decision_settings_routes.rs has this rule without reasons.)
// The extra agreement core asks of its ssrf port only refuses what these ranges already refuse.

fn denied_v4([a, b, c, d]: [u8; 4]) -> Option<&'static str> {
    if a == 0 {
        return Some("the unspecified address");
    }
    if [(169, 254, 169, 254), (100, 100, 100, 200)].contains(&(a, b, c, d)) {
        return Some("a cloud metadata address");
    }
    if a == 169 && b == 254 {
        return Some("a link-local address");
    }
    if a >= 224 {
        return Some(match (a, b) {
            (255, 255) => "the broadcast address",
            (a, _) if a < 240 => "a multicast address",
            _ => "a reserved address",
        });
    }
    if a == 192 && b == 0 && (c == 0 || c == 2) {
        return Some("a reserved address");
    }
    if (a == 198 && b == 51 && c == 100) || (a == 203 && b == 0 && c == 113) {
        return Some("a documentation address");
    }
    None
}

fn local_v4([a, b, ..]: [u8; 4]) -> bool {
    a == 127
        || a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
}

/// `whyNotLocal(ip, { linkLocal: false })`: why `ip` is not on this network, or `None`.
pub fn why_not_local(ip: IpAddr) -> Option<&'static str> {
    let public = Some("a public address");
    let h = match ip {
        IpAddr::V4(v4) => {
            return denied_v4(v4.octets()).or(public.filter(|_| !local_v4(v4.octets())))
        }
        IpAddr::V6(v6) => v6.segments(),
    };
    if h == [0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254] {
        return Some("a cloud metadata address");
    }
    if h[..5].iter().all(|x| *x == 0) && h[5] == 0xffff {
        let v4 = Ipv4Addr::from(((u32::from(h[6])) << 16) | u32::from(h[7])).octets();
        return denied_v4(v4).or(public.filter(|_| !local_v4(v4)));
    }
    if h[..7].iter().all(|x| *x == 0) {
        return match h[7] {
            1 => None,
            0 => Some("the unspecified address"),
            _ => Some("a reserved address"),
        };
    }
    if h[..6].iter().all(|x| *x == 0) {
        return Some("a reserved address");
    }
    if (h[0] & 0xffc0) == 0xfe80 {
        return Some("a link-local address");
    }
    if (h[0] & 0xff00) == 0xff00 {
        return Some("a multicast address");
    }
    if (h[0] == 0x64 && h[1] == 0xff9b)
        || (h[0] == 0x2001 && (h[1] == 0 || h[1] == 0xdb8))
        || (h[0] == 0x100 && h[1..4].iter().all(|x| *x == 0))
    {
        return Some("a reserved address");
    }
    public.filter(|_| (h[0] & 0xfe00) != 0xfc00)
}

/// `checkEmbeddingAddress(url)`: `Err((reason, transient))`; transient: the name did not resolve.
pub async fn check_address(raw: &str) -> Result<(), (String, bool)> {
    let refuse = |why: &str| Err((why.to_string(), false));
    let Ok(url) = url::Url::parse(chat_turn::js::trim(raw)) else {
        return refuse("not a URL");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return refuse("not an http(s) URL");
    }
    if !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty()) {
        return refuse("credentials in the URL");
    }
    let filled = |part: Option<&str>| part.is_some_and(|p| !p.is_empty());
    if filled(url.query()) || filled(url.fragment()) || raw.contains(['?', '#']) {
        return refuse("a query string or fragment in the URL");
    }
    let host = url.host_str().unwrap_or_default();
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.is_empty() {
        return refuse("no host");
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match why_not_local(ip) {
            Some(why) => refuse(&format!("{host} is not on this network ({host} is {why})")),
            None => Ok(()),
        };
    }
    if host.trim_end_matches('.').split('.').any(str::is_empty) {
        return refuse("not a valid host name");
    }
    let lookup = tokio::net::lookup_host((host.as_str(), 0));
    let answers: Vec<IpAddr> = match tokio::time::timeout(Duration::from_secs(5), lookup).await {
        Ok(Ok(a)) => a.map(|a| a.ip()).collect(),
        Ok(Err(_)) => {
            return Err(("the name does not resolve (ENOTFOUND)".into(), true));
        }
        Err(_) => return Err(("the name does not resolve (lookup timed out)".into(), true)),
    };
    if answers.is_empty() {
        return Err(("the name does not resolve".into(), true));
    }
    for a in answers {
        if let Some(why) = why_not_local(a) {
            return refuse(&format!("{host} is not on this network ({a} is {why})"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_drops_run_before_the_kick_and_a_failed_one_is_tried_again() {
        use std::cell::RefCell;
        let index = |p: &str| Index {
            user: None,
            project: p.into(),
        };
        let target = |m: &str| embedding_worker::Target {
            profile: "nomic-v1".into(),
            model: m.into(),
            base_url: None,
        };
        let mut d = Deferred::default();
        d.drops.push((index("qa-one"), target("qa-embed-a")));
        d.drops.push((index("qa-two"), target("qa-embed-b")));
        d.kick = true;
        let log = RefCell::new(Vec::<String>::new());
        let tries = RefCell::new(HashMap::<String, u32>::new());
        d.run(
            |i, t| {
                log.borrow_mut()
                    .push(format!("drop {} {}", i.project, t.model));
                let mut tries = tries.borrow_mut();
                let n = tries.entry(i.project.clone()).or_default();
                *n += 1;
                // qa-one's lock is busy once; qa-two's stays busy.
                if (i.project == "qa-one" && *n == 1) || i.project == "qa-two" {
                    Err("embedding index unavailable".into())
                } else {
                    Ok(())
                }
            },
            || log.borrow_mut().push("kick".into()),
        );
        assert_eq!(
            log.into_inner(),
            [
                "drop qa-one qa-embed-a",
                "drop qa-one qa-embed-a",
                "drop qa-two qa-embed-b",
                "drop qa-two qa-embed-b",
                "kick"
            ]
        );
        assert!(d.drops.is_empty() && !d.kick);
        // Nothing queued: no kick.
        let mut kicked = false;
        Deferred::default().run(|_, _| Ok(()), || kicked = true);
        assert!(!kicked);
    }

    #[test]
    fn a_seeded_address_verdict_is_kept_for_a_minute_and_checks_run_together() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|_| unreachable!());
        let v = Verdicts::default();
        let calls = AtomicU64::new(0);
        let check = |answer: Verdict| {
            let calls = &calls;
            move |_url: String| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { answer }
            }
        };
        let (url, at) = ("http://qa-embed.invalid:8080", Instant::now());
        let refused: Verdict = Err(("the name does not resolve (ENOTFOUND)".into(), true));
        assert_eq!(
            rt.block_on(v.verdict(url, at, check(refused.clone()))),
            refused
        );
        // Within the minute the kept verdict stands and nothing is asked.
        let later = at + Duration::from_secs(59);
        assert_eq!(rt.block_on(v.verdict(url, later, check(Ok(())))), refused);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Another address is its own.
        let other = "http://127.0.0.1:8080";
        assert_eq!(rt.block_on(v.verdict(other, at, check(Ok(())))), Ok(()));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // After the minute it is asked again.
        let expired = at + VERDICT_TTL;
        assert_eq!(rt.block_on(v.verdict(url, expired, check(Ok(())))), Ok(()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // Two checks are not run one after the other (no lock is held while one waits).
        let slow = |_url: String| async {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(())
        };
        let fresh = Verdicts::default();
        let started = Instant::now();
        let both = rt.block_on(async {
            tokio::join!(
                fresh.verdict("http://10.0.0.1", started, slow),
                fresh.verdict("http://10.0.0.2", started, slow)
            )
        });
        assert_eq!(both, (Ok(()), Ok(())));
        assert!(started.elapsed() < Duration::from_millis(790));
    }

    #[test]
    fn the_switch_is_off_unless_named_and_covers_the_exact_path() {
        let settings = |v: Option<&str>| {
            let v = v.map(str::to_string);
            Settings::from_lookup(&move |k: &str| (k == SWITCH).then(|| v.clone()).flatten())
        };
        for off in [None, Some(""), Some("1"), Some("Rust"), Some("on")] {
            assert!(!settings(off).answers(Some(PATH)), "{off:?}");
        }
        let on = settings(Some("rust"));
        assert!(on.answers(Some(PATH)));
        for other in ["/api/embedding/", "/api/embeddings", "/api/embedding/x"] {
            assert!(!on.answers(Some(other)), "{other}");
        }
        assert!(!on.answers(None));
    }

    #[test]
    fn tokens_stay_out_of_debug() {
        let s = Settings::from_lookup(&|k: &str| match k {
            "MODEL_LOADER_TOKEN" | "INFERENCE_API_KEY" => Some("qa-secret-value".into()),
            "EMBEDDING_SERVICE_URL" => Some("http://qa-embed.invalid:8080/".into()),
            _ => None,
        });
        let shown = format!("{s:?}");
        assert!(!shown.contains("qa-secret") && !shown.contains("qa-embed.invalid"));
        assert_eq!(
            s.service_base().as_deref(),
            Some("http://qa-embed.invalid:8080")
        );
    }

    #[test]
    fn the_address_rule_words_every_refusal_as_core() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|_| unreachable!());
        let check = |url: &str| rt.block_on(check_address(url));
        for ok in [
            "http://127.0.0.1:8080",
            "http://10.1.2.3/v1",
            "https://[::1]:9000/",
            "http://100.64.0.1",
            "http://[fd12::1]",
            "http://192.168.1.2",
        ] {
            assert_eq!(check(ok), Ok(()), "{ok}");
        }
        let refused = |url: &str, why: &str| {
            assert_eq!(check(url), Err((why.to_string(), false)), "{url}");
        };
        refused("ftp://127.0.0.1", "not an http(s) URL");
        refused("nonsense", "not a URL");
        refused("http://qa:pw@127.0.0.1", "credentials in the URL");
        refused(
            "http://127.0.0.1/?x",
            "a query string or fragment in the URL",
        );
        refused(
            "http://127.0.0.1/#",
            "a query string or fragment in the URL",
        );
        refused(
            "http://8.8.8.8",
            "8.8.8.8 is not on this network (8.8.8.8 is a public address)",
        );
        refused(
            "http://169.254.169.254",
            "169.254.169.254 is not on this network (169.254.169.254 is a cloud metadata address)",
        );
        refused(
            "http://169.254.1.1",
            "169.254.1.1 is not on this network (169.254.1.1 is a link-local address)",
        );
        refused(
            "http://0.0.0.0",
            "0.0.0.0 is not on this network (0.0.0.0 is the unspecified address)",
        );
        refused(
            "http://[fe80::1]",
            "fe80::1 is not on this network (fe80::1 is a link-local address)",
        );
        refused(
            "http://[::ffff:8.8.8.8]",
            "::ffff:808:808 is not on this network (::ffff:808:808 is a public address)",
        );
        refused(
            "http://203.0.113.9",
            "203.0.113.9 is not on this network (203.0.113.9 is a documentation address)",
        );
        refused(
            "http://255.255.255.255",
            "255.255.255.255 is not on this network (255.255.255.255 is the broadcast address)",
        );
        refused(
            "http://[2001:db8::1]",
            "2001:db8::1 is not on this network (2001:db8::1 is a reserved address)",
        );
        refused(
            "http://[2606:4700::1]",
            "2606:4700::1 is not on this network (2606:4700::1 is a public address)",
        );
    }
}
