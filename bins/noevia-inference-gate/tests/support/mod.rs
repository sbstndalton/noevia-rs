//! Test support: a synthetic engine router and helpers to run the gate in front of it.
//!
//! The fake router behaves like the engine router the gate fronts, at test speed: a listing with
//! aliases (a request may name a model by an alias), explicit loads and unloads, autoload on
//! requests that name a model (unless `autoload=false`), `models-max` with idle-LRU eviction,
//! sleeping models that wake on a request (also on `/slots` with `autoload=false`, as the router's
//! child does), unloads that take a while (longer when the model is busy, when they cut its
//! streams), a stuck mode where an unload never finishes, slow or failing unload answers, preset
//! reloads (on `/models` and `/v1/models`) that restart every running model after a while, session
//! generations (`x-conversation-id`) that go on after their client leaves and can be resumed
//! (`GET /v1/stream`), stopped (`DELETE /v1/stream`) and looked up (`POST /v1/streams/lookup`),
//! and a down mode where every connection is dropped. It records every event
//! (including a client that went away mid-stream, and the most chat models it ever ran at once) so
//! tests can assert that no stream was ever cut by an unload.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

pub type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum St {
    Unloaded,
    Loading,
    Loaded,
    Sleeping,
    Stopping,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// `POST /models/load`.
    Load(String),
    /// A request loaded the model.
    Autoload(String),
    /// A request woke a sleeping model.
    Wake(String),
    /// An unload started; `busy`: while a stream was running on it.
    Unload {
        model: String,
        busy: bool,
    },
    /// The fake's own LRU stopped an idle model for another.
    Evict(String),
    /// A running stream was cut by an unload.
    StreamCut(String),
    /// A request reached the model.
    Request {
        model: String,
        path: String,
    },
    Reload,
    Delete(String),
    /// A client went away mid-stream: the router saw its connection close.
    ClientGone(String),
    /// A session generation whose client left goes on for a later resume.
    Detached(String),
    /// A stream resume reached the model.
    Resume(String),
    /// An explicit stop ended a session generation before its end.
    Stopped(String),
}

#[derive(Clone, Debug)]
pub struct Model {
    pub status: St,
    pub small: bool,
    pub streams: u32,
    pub instance: u64,
    pub last_used: Instant,
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FakeConfig {
    pub load_ms: u64,
    pub stop_ms: u64,
    pub busy_stop_ms: u64,
    pub chunks: u64,
    pub chunk_ms: u64,
    pub models_max: usize,
    /// How long the router takes to answer an unload (the stop itself runs after).
    pub unload_answer_ms: u64,
    /// Answer every unload with a server error (nothing is unloaded).
    pub unload_error: bool,
    /// How long a preset reload takes before it restarts every running model and answers.
    pub reload_ms: u64,
    /// `DELETE /v1/stream` answers at once, before the generation has seen the stop (as a
    /// router in front of several children does).
    pub stop_answers_at_once: bool,
    /// `DELETE /v1/stream` answers with this status and stops nothing.
    pub stop_status: Option<u16>,
    /// How the session lookup answers.
    pub lookup: Lookup,
    /// A stream resume answers with this status and a complete JSON error body.
    pub resume_status: Option<u16>,
}

/// How the fake's session lookup answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lookup {
    /// Every known session, running or done.
    #[default]
    Full,
    /// Leaves out sessions still running (their owner could not be asked); lists done ones.
    OmitRunning,
    /// Answers `[]` while any asked session still runs; lists done ones otherwise.
    EmptyWhileRunning,
    /// Answers with a server error.
    Error,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            load_ms: 60,
            stop_ms: 20,
            busy_stop_ms: 150,
            chunks: 4,
            chunk_ms: 30,
            models_max: 1,
            unload_answer_ms: 0,
            unload_error: false,
            reload_ms: 0,
            stop_answers_at_once: false,
            stop_status: None,
            lookup: Lookup::Full,
            resume_status: None,
        }
    }
}

pub struct State {
    pub models: BTreeMap<String, Model>,
    pub events: Vec<Event>,
    pub stuck: bool,
    pub cfg: FakeConfig,
    pub seen_headers: Vec<(String, hyper::HeaderMap)>,
    /// Conversation id to (model, instance, chunks still to send) of a session generation.
    pub sessions: BTreeMap<String, (String, u64, u64)>,
    /// Conversation ids whose session generation reached its end.
    pub done_sessions: std::collections::BTreeSet<String>,
    /// Session lookups answered.
    pub lookups: u32,
    /// The most chat models (not small) it ever ran at once.
    pub peak_chat_running: usize,
}

pub struct FakeRouter {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
    pub changed: Arc<Notify>,
    pub down: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeRouter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeRouter {
    /// Models: `(id, status, small)`.
    pub async fn start(models: &[(&str, St, bool)], cfg: FakeConfig) -> Self {
        let now = Instant::now();
        let state = Arc::new(Mutex::new(State {
            models: models
                .iter()
                .map(|(id, status, small)| {
                    (
                        (*id).to_owned(),
                        Model {
                            status: *status,
                            small: *small,
                            streams: 0,
                            instance: 1,
                            last_used: now,
                            aliases: Vec::new(),
                        },
                    )
                })
                .collect(),
            events: Vec::new(),
            stuck: false,
            cfg,
            seen_headers: Vec::new(),
            sessions: BTreeMap::new(),
            done_sessions: Default::default(),
            lookups: 0,
            peak_chat_running: 0,
        }));
        {
            let mut s = state.lock().unwrap();
            let running = chat_running(&s);
            s.peak_chat_running = running;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let changed = Arc::new(Notify::new());
        let down = Arc::new(AtomicBool::new(false));
        let task = {
            let state = Arc::clone(&state);
            let changed = Arc::clone(&changed);
            let down = Arc::clone(&down);
            tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        continue;
                    };
                    if down.load(Ordering::SeqCst) {
                        drop(socket);
                        continue;
                    }
                    let state = Arc::clone(&state);
                    let changed = Arc::clone(&changed);
                    let down = Arc::clone(&down);
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            let state = Arc::clone(&state);
                            let changed = Arc::clone(&changed);
                            let down = down.load(Ordering::SeqCst);
                            async move {
                                if down {
                                    // A pooled connection to a router that went away: no answer.
                                    return Err(std::io::Error::other("router down"));
                                }
                                Ok::<_, std::io::Error>(route(state, changed, req).await)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(socket), service)
                            .await;
                    });
                }
            })
        };
        Self {
            addr,
            state,
            changed,
            down,
            task,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn events(&self) -> Vec<Event> {
        self.state.lock().unwrap().events.clone()
    }

    pub fn cuts(&self) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|e| matches!(e, Event::StreamCut(_) | Event::Unload { busy: true, .. }))
            .collect()
    }

    pub fn loads_of(&self, model: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| matches!(e, Event::Autoload(m) | Event::Load(m) if m == model))
            .count()
    }

    pub fn status(&self, model: &str) -> St {
        self.state.lock().unwrap().models[model].status
    }

    pub fn set_stuck(&self, stuck: bool) {
        self.state.lock().unwrap().stuck = stuck;
        self.changed.notify_waiters();
    }

    /// The most chat models the engine ever ran at once.
    pub fn peak_chat_running(&self) -> usize {
        self.state.lock().unwrap().peak_chat_running
    }

    pub fn set_aliases(&self, model: &str, aliases: &[&str]) {
        self.state
            .lock()
            .unwrap()
            .models
            .get_mut(model)
            .unwrap()
            .aliases = aliases.iter().map(|a| (*a).to_owned()).collect();
    }

    pub fn configure(&self, f: impl FnOnce(&mut FakeConfig)) {
        f(&mut self.state.lock().unwrap().cfg);
    }

    pub fn set_status(&self, model: &str, status: St) {
        self.state
            .lock()
            .unwrap()
            .models
            .get_mut(model)
            .unwrap()
            .status = status;
        self.changed.notify_waiters();
    }
}

fn full(status: u16, body: impl Into<Bytes>, content_type: &str) -> Response<BoxBody> {
    let mut response = Response::new(
        Full::new(body.into())
            .map_err(|never: Infallible| match never {})
            .boxed(),
    );
    *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
    response
        .headers_mut()
        .insert("content-type", content_type.parse().unwrap());
    response
}

fn json(status: u16, value: serde_json::Value) -> Response<BoxBody> {
    full(status, value.to_string(), "application/json")
}

fn error(status: u16, message: &str) -> Response<BoxBody> {
    json(
        status,
        serde_json::json!({ "error": { "code": status, "message": message } }),
    )
}

fn running(status: St) -> bool {
    !matches!(status, St::Unloaded)
}

fn chat_running(state: &State) -> usize {
    state
        .models
        .values()
        .filter(|m| !m.small && running(m.status))
        .count()
}

/// The id a name refers to (an id, or an alias), as the router resolves it.
fn resolve(state: &State, name: &str) -> Option<String> {
    if state.models.contains_key(name) {
        return Some(name.to_owned());
    }
    state
        .models
        .iter()
        .find(|(_, m)| m.aliases.iter().any(|a| a == name))
        .map(|(id, _)| id.clone())
}

fn listing(state: &State) -> serde_json::Value {
    let data: Vec<serde_json::Value> = state
        .models
        .iter()
        .map(|(id, m)| {
            let value = match m.status {
                St::Unloaded => "unloaded",
                St::Loading => "loading",
                St::Loaded | St::Stopping => "loaded",
                St::Sleeping => "sleeping",
            };
            let args: Vec<&str> = if m.small {
                vec!["--embedding"]
            } else {
                vec!["--ctx-size", "4096"]
            };
            serde_json::json!({ "id": id, "aliases": m.aliases, "status": { "value": value, "args": args } })
        })
        .collect();
    serde_json::json!({ "data": data })
}

/// Starts stopping `model`: quickly when idle, after cutting its streams when busy, never while
/// stuck.
fn stop(state: &Arc<Mutex<State>>, changed: &Arc<Notify>, model: &str) {
    let (delay, busy) = {
        let mut s = state.lock().unwrap();
        let cfg = s.cfg.clone();
        let Some(m) = s.models.get_mut(model) else {
            return;
        };
        if !running(m.status) || m.status == St::Stopping {
            return;
        }
        m.status = St::Stopping;
        let busy = m.streams > 0;
        s.events.push(Event::Unload {
            model: model.to_owned(),
            busy,
        });
        (if busy { cfg.busy_stop_ms } else { cfg.stop_ms }, busy)
    };
    let state = Arc::clone(state);
    let changed = Arc::clone(changed);
    let model = model.to_owned();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        loop {
            {
                let mut s = state.lock().unwrap();
                if !s.stuck {
                    let m = s.models.get_mut(&model).unwrap();
                    m.status = St::Unloaded;
                    m.instance += 1;
                    if busy || m.streams > 0 {
                        m.streams = 0;
                    }
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        changed.notify_waiters();
    });
}

/// Waits until `model` can serve, loading (and evicting an idle model) as the router does.
async fn ensure_loaded(
    state: &Arc<Mutex<State>>,
    changed: &Arc<Notify>,
    model: &str,
) -> Result<u64, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let notified = changed.notified();
        let mut evict = None;
        {
            let mut s = state.lock().unwrap();
            let cfg = s.cfg.clone();
            let running_count = s.models.values().filter(|m| running(m.status)).count();
            let Some(m) = s.models.get(model).cloned() else {
                return Err("model is not found".into());
            };
            match m.status {
                St::Loaded => return Ok(m.instance),
                St::Sleeping => {
                    s.models.get_mut(model).unwrap().status = St::Loaded;
                    s.events.push(Event::Wake(model.to_owned()));
                    return Ok(m.instance);
                }
                St::Loading | St::Stopping => {}
                St::Unloaded => {
                    if running_count < cfg.models_max {
                        s.models.get_mut(model).unwrap().status = St::Loading;
                        s.events.push(Event::Autoload(model.to_owned()));
                        let chat = chat_running(&s);
                        s.peak_chat_running = s.peak_chat_running.max(chat);
                        let state = Arc::clone(state);
                        let changed = Arc::clone(changed);
                        let model = model.to_owned();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(cfg.load_ms)).await;
                            {
                                let mut s = state.lock().unwrap();
                                let m = s.models.get_mut(&model).unwrap();
                                if m.status == St::Loading {
                                    m.status = St::Loaded;
                                }
                            }
                            changed.notify_waiters();
                        });
                    } else {
                        // The router's LRU: an idle running model, never a busy one.
                        evict = s
                            .models
                            .iter()
                            .filter(|(id, m)| {
                                id.as_str() != model
                                    && matches!(m.status, St::Loaded | St::Sleeping)
                                    && m.streams == 0
                            })
                            .min_by_key(|(_, m)| m.last_used)
                            .map(|(id, _)| id.clone());
                        if let Some(victim) = &evict {
                            s.events.push(Event::Evict(victim.clone()));
                        }
                    }
                }
            }
        }
        if let Some(victim) = evict {
            stop(state, changed, &victim);
            continue;
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for the model".into());
        }
        let _ = tokio::time::timeout(Duration::from_millis(20), notified).await;
    }
}

async fn route(
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    req: Request<Incoming>,
) -> Response<BoxBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query: BTreeMap<String, String> = req
        .uri()
        .query()
        .map(|q| {
            let mut out = BTreeMap::new();
            for (k, v) in form_urlencoded_pairs(q) {
                out.entry(k).or_insert(v);
            }
            out
        })
        .unwrap_or_default();
    let headers = req.headers().clone();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    state
        .lock()
        .unwrap()
        .seen_headers
        .push((path.clone(), headers.clone()));
    let reload = query.get("reload").is_some_and(|v| !v.is_empty());
    match (method.as_str(), path.as_str()) {
        ("GET", "/models" | "/v1/models") if reload => {
            // The router's reload: after a while it restarts every running model (as if every
            // preset had changed), cutting whatever runs on them, then answers.
            let reload_ms = {
                let mut s = state.lock().unwrap();
                s.events.push(Event::Reload);
                s.cfg.reload_ms
            };
            if reload_ms > 0 {
                tokio::time::sleep(Duration::from_millis(reload_ms)).await;
                let running: Vec<String> = state
                    .lock()
                    .unwrap()
                    .models
                    .iter()
                    .filter(|(_, m)| running(m.status))
                    .map(|(id, _)| id.clone())
                    .collect();
                for model in running {
                    stop(&state, &changed, &model);
                }
            }
            json(200, serde_json::json!({ "success": true }))
        }
        ("GET", "/models") => {
            let s = state.lock().unwrap();
            json(200, listing(&s))
        }
        ("GET", "/v1/stream") => {
            let conv = query.get("conv_id").cloned().unwrap_or_default();
            let session = state.lock().unwrap().sessions.get(&conv).cloned();
            let Some((model, instance, _)) = session else {
                return error(404, "Stream not found or expired");
            };
            if let Some(status) = state.lock().unwrap().cfg.resume_status {
                // A complete JSON error body, sent without a length (chunked), so the gate sees
                // the body reach its end.
                let body = serde_json::json!({
                    "error": { "code": status, "message": "synthetic resume failure" }
                })
                .to_string();
                let frames = futures_util::stream::iter([Ok::<_, std::io::Error>(Frame::data(
                    Bytes::from(body),
                ))]);
                let mut response = Response::new(StreamBody::new(frames).boxed());
                *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                response
                    .headers_mut()
                    .insert("content-type", "application/json".parse().unwrap());
                return response;
            }
            state
                .lock()
                .unwrap()
                .events
                .push(Event::Resume(model.clone()));
            // The rest of the answer, while the generation goes on.
            resume_stream(state, model, instance)
        }
        ("DELETE", "/v1/stream") => {
            // The explicit stop: cancels the generation and forgets the session (idempotent).
            let conv = query.get("conv_id").cloned().unwrap_or_default();
            if conv.is_empty() {
                return error(400, "Missing conversation id in path");
            }
            let (chunk_ms, at_once) = {
                let mut s = state.lock().unwrap();
                if let Some(status) = s.cfg.stop_status {
                    return error(status, "synthetic stop failure");
                }
                s.sessions.remove(&conv);
                (s.cfg.chunk_ms, s.cfg.stop_answers_at_once)
            };
            if !at_once {
                // Answers once the generation has seen the stop (it checks once per chunk).
                tokio::time::sleep(Duration::from_millis(chunk_ms * 3)).await;
            }
            full(204, Bytes::new(), "application/json")
        }
        ("POST", "/v1/streams/lookup") => {
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let mut s = state.lock().unwrap();
            s.lookups += 1;
            let mode = s.cfg.lookup;
            if mode == Lookup::Error {
                return error(500, "synthetic lookup failure");
            }
            let asked_running = value
                .get("conversation_ids")
                .and_then(|v| v.as_array())
                .is_some_and(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str())
                        .any(|id| s.sessions.contains_key(id) && !s.done_sessions.contains(id))
                });
            if mode == Lookup::EmptyWhileRunning && asked_running {
                return json(200, serde_json::json!([]));
            }
            let rows: Vec<_> = value
                .get("conversation_ids")
                .and_then(|v| v.as_array())
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str())
                        .filter(|id| s.sessions.contains_key(*id))
                        .filter(|id| mode == Lookup::Full || s.done_sessions.contains(*id))
                        .map(|id| {
                            serde_json::json!({
                                "conversation_id": id,
                                "is_done": s.done_sessions.contains(id),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json(200, serde_json::Value::Array(rows))
        }
        ("GET", "/v1/models") => {
            let s = state.lock().unwrap();
            let data: Vec<_> = s
                .models
                .keys()
                .map(|id| serde_json::json!({ "id": id, "object": "model" }))
                .collect();
            json(200, serde_json::json!({ "object": "list", "data": data }))
        }
        ("GET", "/health") => json(200, serde_json::json!({ "status": "ok" })),
        ("GET", "/props" | "/metrics" | "/slots") => {
            let Some(name) = query.get("model").cloned() else {
                return error(400, "model is required");
            };
            let Some(model) = resolve(&state.lock().unwrap(), &name) else {
                return error(400, "model is not found");
            };
            let autoload = matches!(
                query.get("autoload").map(String::as_str),
                None | Some("true" | "1")
            );
            if autoload {
                if let Err(e) = ensure_loaded(&state, &changed, &model).await {
                    return error(400, &e);
                }
            } else {
                let mut s = state.lock().unwrap();
                match s.models.get(&model).map(|m| m.status) {
                    // The child wakes for its slots, even when asked not to load.
                    Some(St::Sleeping) if path == "/slots" => {
                        s.models.get_mut(&model).unwrap().status = St::Loaded;
                        s.events.push(Event::Wake(model.clone()));
                    }
                    Some(status) if running(status) => {}
                    _ => return error(400, "model is not running"),
                }
            }
            match path.as_str() {
                "/metrics" => full(200, "engine_tokens_predicted_total 7\n", "text/plain"),
                "/slots" => json(
                    200,
                    serde_json::json!([{ "id": 0, "is_processing": false }]),
                ),
                _ => json(
                    200,
                    serde_json::json!({ "default_generation_settings": { "n_ctx": 4096 }, "total_slots": 1 }),
                ),
            }
        }
        ("POST", "/models/load") => {
            let name = model_of(&body);
            let model = resolve(&state.lock().unwrap(), &name).unwrap_or(name);
            let mut s = state.lock().unwrap();
            match s.models.get(&model).map(|m| m.status) {
                None => error(404, "model is not found"),
                Some(St::Unloaded) => {
                    s.events.push(Event::Load(model.clone()));
                    drop(s);
                    let state = Arc::clone(&state);
                    let changed = Arc::clone(&changed);
                    tokio::spawn(async move {
                        let _ = ensure_loaded(&state, &changed, &model).await;
                    });
                    json(200, serde_json::json!({ "success": true }))
                }
                Some(_) => error(400, "model is already running"),
            }
        }
        ("POST", "/models/unload") => {
            let (answer_ms, unload_error) = {
                let s = state.lock().unwrap();
                (s.cfg.unload_answer_ms, s.cfg.unload_error)
            };
            if answer_ms > 0 {
                tokio::time::sleep(Duration::from_millis(answer_ms)).await;
            }
            if unload_error {
                return error(500, "synthetic unload failure");
            }
            let name = model_of(&body);
            let model = resolve(&state.lock().unwrap(), &name).unwrap_or(name);
            let status = state.lock().unwrap().models.get(&model).map(|m| m.status);
            match status {
                Some(s) if running(s) => {
                    stop(&state, &changed, &model);
                    json(200, serde_json::json!({ "success": true }))
                }
                _ => error(400, "model is not running"),
            }
        }
        ("DELETE", "/models") => {
            let model = query.get("model").cloned().unwrap_or_default();
            state
                .lock()
                .unwrap()
                .events
                .push(Event::Delete(model.clone()));
            stop(&state, &changed, &model);
            json(200, serde_json::json!({ "success": true }))
        }
        ("POST", "/v1/chat/completions" | "/v1/embeddings" | "/v1/completions") => {
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let name = value
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_owned();
            let Some(model) = resolve(&state.lock().unwrap(), &name) else {
                return error(400, "model is not found");
            };
            let autoload = matches!(
                query.get("autoload").map(String::as_str),
                None | Some("true" | "1")
            );
            if !autoload {
                let s = state.lock().unwrap();
                if !s.models.get(&model).is_some_and(|m| running(m.status)) {
                    return error(400, "model is not loaded");
                }
            }
            let instance = match ensure_loaded(&state, &changed, &model).await {
                Ok(instance) => instance,
                Err(e) => return error(400, &e),
            };
            let conv_id = headers
                .get("x-conversation-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
                .map(str::to_owned);
            let (chunks, chunk_ms) = {
                let mut s = state.lock().unwrap();
                s.events.push(Event::Request {
                    model: model.clone(),
                    path: path.clone(),
                });
                let cfg = s.cfg.clone();
                let m = s.models.get_mut(&model).unwrap();
                m.last_used = Instant::now();
                (
                    value
                        .get("fake_chunks")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(cfg.chunks),
                    value
                        .get("fake_chunk_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(cfg.chunk_ms),
                )
            };
            let stream = value
                .get("stream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !stream {
                tokio::time::sleep(Duration::from_millis(chunks * chunk_ms)).await;
                return json(
                    200,
                    serde_json::json!({ "model": model, "choices": [{ "message": { "content": "synthetic" } }] }),
                );
            }
            state
                .lock()
                .unwrap()
                .models
                .get_mut(&model)
                .unwrap()
                .streams += 1;
            if let Some(conv) = &conv_id {
                state
                    .lock()
                    .unwrap()
                    .sessions
                    .insert(conv.clone(), (model.clone(), instance, chunks));
            }
            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(2);
            let state2 = Arc::clone(&state);
            tokio::spawn(async move {
                let mut cut = false;
                let mut detached = false;
                let mut stopped = false;
                for i in 0..chunks {
                    tokio::time::sleep(Duration::from_millis(chunk_ms)).await;
                    let alive = {
                        let mut s = state2.lock().unwrap();
                        if let Some(conv) = &conv_id {
                            match s.sessions.get_mut(conv) {
                                Some(session) => session.2 = chunks - i - 1,
                                None => stopped = true,
                            }
                        }
                        if stopped {
                            s.events.push(Event::Stopped(model.clone()));
                            break;
                        }
                        let m = &s.models[&model];
                        m.instance == instance && m.status != St::Unloaded
                    };
                    if !alive {
                        cut = true;
                        break;
                    }
                    if detached {
                        continue;
                    }
                    let chunk = format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"t{i} \"}}}}]}}\n\n"
                    );
                    if tx.send(Ok(Frame::data(Bytes::from(chunk)))).await.is_err() {
                        let mut s = state2.lock().unwrap();
                        if conv_id.is_some() {
                            // A session generation goes on for a later resume.
                            s.events.push(Event::Detached(model.clone()));
                            detached = true;
                        } else {
                            // The router sees the connection close and stops the generation.
                            s.events.push(Event::ClientGone(model.clone()));
                            break;
                        }
                    }
                }
                {
                    let mut s = state2.lock().unwrap();
                    if cut {
                        s.events.push(Event::StreamCut(model.clone()));
                    } else if !stopped {
                        if let Some(conv) = &conv_id {
                            s.done_sessions.insert(conv.clone());
                        }
                    }
                    let m = s.models.get_mut(&model).unwrap();
                    if m.instance == instance {
                        m.streams = m.streams.saturating_sub(1);
                    }
                    m.last_used = Instant::now();
                }
                if cut {
                    let _ = tx
                        .send(Err(std::io::Error::other("instance stopped")))
                        .await;
                } else {
                    let _ = tx
                        .send(Ok(Frame::data(Bytes::from_static(b"data: [DONE]\n\n"))))
                        .await;
                }
            });
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            let mut response = Response::new(StreamBody::new(stream).boxed());
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            response
        }
        ("POST", "/echo") => full(200, body, "application/octet-stream"),
        _ => error(404, "not found"),
    }
}

/// The remaining chunks of a session generation, streamed while it goes on.
fn resume_stream(state: Arc<Mutex<State>>, model: String, instance: u64) -> Response<BoxBody> {
    let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(2);
    let chunk_ms = state.lock().unwrap().cfg.chunk_ms;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(chunk_ms)).await;
            let (alive, left) = {
                let s = state.lock().unwrap();
                let m = &s.models[&model];
                let left = s
                    .sessions
                    .values()
                    .find(|(sm, si, _)| *sm == model && *si == instance)
                    .map_or(0, |(_, _, left)| *left);
                (m.instance == instance && m.status != St::Unloaded, left)
            };
            if !alive {
                state
                    .lock()
                    .unwrap()
                    .events
                    .push(Event::StreamCut(model.clone()));
                let _ = tx
                    .send(Err(std::io::Error::other("instance stopped")))
                    .await;
                return;
            }
            if left == 0 {
                let _ = tx
                    .send(Ok(Frame::data(Bytes::from_static(b"data: [DONE]\n\n"))))
                    .await;
                return;
            }
            if tx
                .send(Ok(Frame::data(Bytes::from_static(b"data: {}\n\n"))))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut response = Response::new(StreamBody::new(stream).boxed());
    response
        .headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    response
}

fn model_of(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

fn form_urlencoded_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let mut it = pair.splitn(2, '=');
            (
                it.next().unwrap_or_default().to_owned(),
                it.next().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

// ---- the gate in front of it ----

pub struct GateHandle {
    pub addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GateHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

impl GateHandle {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

pub const ADMIN: &str = "synthetic-admin-token-0001";

/// Test-speed limits: every wait is short so the suite stays fast.
pub fn test_config(upstream: String) -> noevia_inference_gate::GateConfig {
    let mut cfg =
        noevia_inference_gate::GateConfig::defaults("127.0.0.1:0".parse().unwrap(), upstream);
    cfg.admin_token = Some(ADMIN.into());
    cfg.admission = inference_admission::Config {
        exempt: Default::default(),
        max_waiters: 16,
        max_leases: 16,
        max_wait_ms: 5_000,
        fair_hold_ms: 400,
        residency_grace_ms: 100,
        lease_idle_ms: 400,
        lease_hard_ms: 10_000,
        unload_timeout_ms: 700,
        router_down_grace_ms: 300,
        session_hold_ms: 1_500,
    };
    cfg.body_timeout = Duration::from_millis(400);
    cfg.header_timeout = Duration::from_secs(20);
    cfg.listing_idle_interval = Duration::from_millis(100);
    cfg.listing_busy_interval = Duration::from_millis(30);
    cfg.tick = Duration::from_millis(20);
    cfg.lease_event_interval = Duration::from_millis(100);
    cfg.header_read_timeout = Duration::from_millis(500);
    cfg.reload_timeout = Duration::from_secs(10);
    cfg.session_check_interval = Duration::from_millis(50);
    cfg
}

pub async fn start_gate(cfg: noevia_inference_gate::GateConfig) -> GateHandle {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = noevia_inference_gate::run(listener, cfg, async move {
            let _ = stopped.await;
        })
        .await;
    });
    // Ready once it answers.
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if client
            .get(format!("http://{addr}/gate/v1/health"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    GateHandle {
        addr,
        stop: Some(stop),
        task,
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

pub fn chat_body(model: &str, chunks: u64, chunk_ms: u64) -> String {
    serde_json::json!({
        "model": model,
        "stream": true,
        "messages": [{ "role": "user", "content": "synthetic question" }],
        "fake_chunks": chunks,
        "fake_chunk_ms": chunk_ms,
    })
    .to_string()
}

/// A streamed chat through the gate: (status, body, refusal code).
pub async fn chat(
    gate: &GateHandle,
    model: &str,
    chunks: u64,
    chunk_ms: u64,
    lease: Option<&str>,
) -> (u16, String) {
    let mut req = client()
        .post(gate.url("/v1/chat/completions"))
        .header("content-type", "application/json")
        .body(chat_body(model, chunks, chunk_ms));
    if let Some(lease) = lease {
        req = req.header("x-noevia-lease", lease);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_else(|_| "<cut>".into());
            (status, text)
        }
        Err(_) => (0, String::new()),
    }
}

pub async fn status(gate: &GateHandle) -> serde_json::Value {
    json_of(
        client()
            .get(gate.url("/gate/v1/status"))
            .send()
            .await
            .unwrap(),
    )
    .await
}

/// Reads an event stream into (event, data) pairs until it ends or `until` matches.
pub async fn read_events(
    resp: reqwest::Response,
    until: impl Fn(&str) -> bool,
) -> Vec<(String, serde_json::Value)> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buf.find("\n\n") {
            let block: String = buf.drain(..end + 2).collect();
            let mut name = String::new();
            let mut data = serde_json::Value::Null;
            for line in block.lines() {
                if let Some(n) = line.strip_prefix("event: ") {
                    name = n.to_owned();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(d).unwrap_or(serde_json::Value::Null);
                }
            }
            let done = until(&name);
            out.push((name, data));
            if done {
                return out;
            }
        }
    }
    out
}

/// Acquires a lease; `Ok(token)` or `Err(code)`.
pub async fn acquire_lease(gate: &GateHandle, model: &str) -> Result<String, String> {
    let resp = client()
        .post(gate.url("/gate/v1/leases"))
        .body(serde_json::json!({ "model": model, "holder": "test" }).to_string())
        .send()
        .await
        .unwrap();
    if !resp.status().is_success() {
        let v: serde_json::Value = json_of(resp).await;
        return Err(v["error"]["code"].as_str().unwrap_or_default().to_owned());
    }
    let events = read_events(resp, |e| e == "granted" || e == "error").await;
    match events.last() {
        Some((name, data)) if name == "granted" => Ok(data["lease"].as_str().unwrap().to_owned()),
        Some((_, data)) => Err(data["code"].as_str().unwrap_or_default().to_owned()),
        None => Err("no events".into()),
    }
}

pub async fn release_lease(gate: &GateHandle, token: &str) -> u16 {
    client()
        .delete(gate.url(&format!("/gate/v1/leases/{token}")))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

pub async fn json_of(resp: reqwest::Response) -> serde_json::Value {
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

/// Sends raw bytes and returns what came back before the server closed or `wait_ms` passed.
pub async fn raw(addr: SocketAddr, request: &[u8], wait_ms: u64) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket.write_all(request).await.unwrap();
    let mut out = Vec::new();
    let _ =
        tokio::time::timeout(Duration::from_millis(wait_ms), socket.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

/// Polls `check` every 10 ms for up to `ms`.
pub async fn eventually(ms: u64, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    check()
}
