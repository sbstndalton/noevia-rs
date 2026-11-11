//! The one-model-at-a-time scheduler.
//!
//! Every request takes a ticket. A request for the loaded model is admitted at once unless an
//! older request waits for a different model (then it queues behind it, so a busy model cannot
//! starve the others), or it was already queued when its model finished loading (that batch is
//! served together). The oldest request for another model starts a swap once nothing is in
//! flight. A swap runs in its own task, so a client that disconnects mid-swap cannot leave it half
//! done: drain, stop (verified), start, health. A failed swap fails every request that was queued
//! for that model when it failed, instead of each retrying the whole start.

use crate::backend::{remove_containers, round2, Backend, StartError, StopReport};
use crate::config::Config;
use crate::events::Events;
use crate::gpu_lock::{self, GpuLock};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Model(usize),
    Unload,
}

#[derive(Debug, Clone)]
struct Failure {
    model: usize,
    /// Tickets below this were queued when the swap failed: they share its error.
    before: u64,
    error: String,
}

struct State {
    /// The running backend; taken by a swap task while it stops it.
    active: Option<Backend>,
    /// The model that is loaded and healthy.
    loaded: Option<usize>,
    /// The target of the swap in progress (`Some(None)`: an unload).
    swapping: Option<Option<usize>>,
    inflight: usize,
    waiters: BTreeMap<u64, Target>,
    next_ticket: u64,
    /// Tickets below this were queued when the loaded model became ready.
    batch_before: u64,
    last_used: Instant,
    failure: Option<Failure>,
    /// Containers a stop could not remove: no backend starts until they are gone.
    stuck: Vec<String>,
    lock: Option<GpuLock>,
    shutting_down: bool,
    swaps: u64,
}

struct Inner {
    config: Config,
    state: Mutex<State>,
    notify: Notify,
    client: reqwest::Client,
    events: Events,
    shutdown: watch::Sender<bool>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

/// Why a request could not be given a backend.
#[derive(Debug, Clone, PartialEq)]
pub enum AcquireError {
    Failed(String),
    ShuttingDown,
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::Failed(e) => f.write_str(e),
            AcquireError::ShuttingDown => f.write_str("router is shutting down"),
        }
    }
}

/// A request's hold on the loaded backend: while any lease lives, no swap or unload starts.
pub struct Lease {
    inner: Arc<Inner>,
    model: crate::config::Model,
}

impl Lease {
    pub fn model(&self) -> &crate::config::Model {
        &self.model
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.inner.state();
        s.inflight = s.inflight.saturating_sub(1);
        s.last_used = Instant::now();
        drop(s);
        self.inner.notify.notify_waiters();
    }
}

/// Removes a queued ticket when its request goes away (client disconnect included).
struct Ticket {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let removed = self.inner.state().waiters.remove(&self.id).is_some();
        if removed {
            self.inner.notify.notify_waiters();
        }
    }
}

impl Router {
    pub fn new(config: Config) -> std::io::Result<Router> {
        std::fs::create_dir_all(&config.logdir)?;
        let events = Events::open(&config.logdir)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(std::io::Error::other)?;
        let (shutdown, _) = watch::channel(false);
        let state = State {
            active: None,
            loaded: None,
            swapping: None,
            inflight: 0,
            waiters: BTreeMap::new(),
            next_ticket: 0,
            batch_before: 0,
            last_used: Instant::now(),
            failure: None,
            stuck: Vec::new(),
            lock: None,
            shutting_down: false,
            swaps: 0,
        };
        Ok(Router {
            inner: Arc::new(Inner {
                config,
                state: Mutex::new(state),
                notify: Notify::new(),
                client,
                events,
                shutdown,
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.inner.client
    }

    pub fn loaded_name(&self) -> Option<String> {
        let s = self.inner.state();
        s.loaded
            .and_then(|i| self.inner.config.models.get(i))
            .map(|m| m.name.clone())
    }

    /// Wait until model `index` is loaded and healthy; the lease counts the caller as in flight.
    pub async fn acquire(&self, index: usize) -> Result<Lease, AcquireError> {
        let model = self
            .inner
            .config
            .models
            .get(index)
            .cloned()
            .ok_or_else(|| AcquireError::Failed("no such model".into()))?;
        self.wait_turn(Target::Model(index)).await.map(|_| Lease {
            inner: self.inner.clone(),
            model,
        })
    }

    /// Unload whatever is loaded once in-flight requests finish. Returns the unloaded model's name.
    pub async fn unload(&self) -> Result<Option<String>, AcquireError> {
        self.wait_turn(Target::Unload).await
    }

    async fn wait_turn(&self, target: Target) -> Result<Option<String>, AcquireError> {
        let inner = &self.inner;
        let id = {
            let mut s = inner.state();
            if s.shutting_down {
                return Err(AcquireError::ShuttingDown);
            }
            let id = s.next_ticket;
            s.next_ticket += 1;
            s.waiters.insert(id, target);
            id
        };
        let _ticket = Ticket {
            inner: inner.clone(),
            id,
        };
        let mut unloading: Option<Option<String>> = None;
        loop {
            let notified = inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = inner.state();
                if s.shutting_down {
                    return Err(AcquireError::ShuttingDown);
                }
                if let (Target::Model(m), Some(f)) = (target, &s.failure) {
                    if f.model == m && id < f.before {
                        return Err(AcquireError::Failed(f.error.clone()));
                    }
                }
                if s.swapping.is_none() {
                    if let Some(done) = unloading.take() {
                        return Ok(done);
                    }
                    // A backend that died while loaded is cleaned up by the next swap.
                    if s.loaded.is_some() {
                        if let Some(status) = s.active.as_mut().and_then(Backend::exited) {
                            let name = s
                                .loaded
                                .and_then(|i| inner.config.models.get(i))
                                .map(|m| m.name.clone());
                            s.loaded = None;
                            inner
                                .events
                                .emit("died", obj(json!({"model": name, "status": status})));
                        }
                    }
                    let loaded = s.loaded;
                    let wants_other = |t: &Target| match t {
                        Target::Model(i) => Some(*i) != loaded,
                        Target::Unload => {
                            loaded.is_some() || s.active.is_some() || !s.stuck.is_empty()
                        }
                    };
                    let oldest_other = s
                        .waiters
                        .iter()
                        .find(|(_, t)| wants_other(t))
                        .map(|(k, _)| *k);
                    match target {
                        Target::Model(m) if loaded == Some(m) => {
                            if id < s.batch_before || oldest_other.is_none_or(|o| o > id) {
                                s.waiters.remove(&id);
                                s.inflight += 1;
                                return Ok(None);
                            }
                        }
                        Target::Unload if !wants_other(&target) => return Ok(None),
                        _ => {
                            // Requests the loaded model may still serve (its batch, or older
                            // than this one) go first; they are about to be admitted.
                            let admissible = loaded.is_some_and(|l| {
                                s.waiters.iter().any(|(k, t)| {
                                    *t == Target::Model(l) && (*k < s.batch_before || *k < id)
                                })
                            });
                            if s.inflight == 0 && oldest_other == Some(id) && !admissible {
                                let to = match target {
                                    Target::Model(m) => Some(m),
                                    Target::Unload => None,
                                };
                                if to.is_none() {
                                    let name = loaded
                                        .and_then(|i| inner.config.models.get(i))
                                        .map(|m| m.name.clone());
                                    unloading = Some(name);
                                }
                                s.swapping = Some(to);
                                drop(s);
                                let router = self.clone();
                                let reason = if to.is_some() { "request" } else { "unload" };
                                tokio::spawn(async move { router.swap(to, reason).await });
                            }
                        }
                    }
                }
            }
            notified.await;
        }
    }

    /// Run one swap to `to` (`None`: unload). The caller has set `swapping`.
    async fn swap(&self, to: Option<usize>, reason: &str) {
        let inner = &self.inner;
        let cfg = &inner.config;
        let t0 = Instant::now();
        let (prev, from) = {
            let mut s = inner.state();
            let from = s.loaded.or_else(|| {
                s.active
                    .as_ref()
                    .and_then(|b| cfg.names.get(&b.model.name).copied())
            });
            s.loaded = None;
            s.swaps += 1;
            (s.active.take(), from)
        };
        let name = |i: Option<usize>| i.and_then(|i| cfg.models.get(i)).map(|m| m.name.clone());
        let mut rec = obj(json!({"from": name(from), "to": name(to), "reason": reason}));
        let mut stuck = std::mem::take(&mut inner.state().stuck);
        if let Some(prev) = prev {
            let report = prev.stop(&cfg.docker).await;
            put_stop(&mut rec, "stop", &report);
            stuck.extend(report.containers_left);
        }
        let outcome = self.start(to, &mut stuck, &mut rec, t0).await;
        rec.insert("total_s".into(), json!(round2(t0.elapsed())));
        let mut s = inner.state();
        s.stuck = stuck;
        match outcome {
            Ok(Some(backend)) => {
                s.active = Some(backend);
                s.loaded = to;
                s.batch_before = s.next_ticket;
                s.last_used = Instant::now();
                s.failure = None;
            }
            Ok(None) => {}
            Err(error) => {
                rec.insert("error".into(), json!(error));
                if let Some(m) = to {
                    s.failure = Some(Failure {
                        model: m,
                        before: s.next_ticket,
                        error,
                    });
                }
            }
        }
        if s.active.is_none() && s.stuck.is_empty() {
            s.lock = None; // nothing of ours can be on the GPU any more
        }
        rec.insert("stuck_containers".into(), json!(s.stuck));
        s.swapping = None;
        drop(s);
        inner.events.emit("swap", rec);
        inner.notify.notify_waiters();
    }

    /// The start half of a swap: refuse while containers are stuck, verify the target's own
    /// containers are absent, take the GPU lock, spawn and wait for health.
    async fn start(
        &self,
        to: Option<usize>,
        stuck: &mut Vec<String>,
        rec: &mut Map<String, Value>,
        t0: Instant,
    ) -> Result<Option<Backend>, String> {
        let inner = &self.inner;
        let cfg = &inner.config;
        if !stuck.is_empty() {
            *stuck = remove_containers(&cfg.docker, stuck).await;
            if !stuck.is_empty() {
                return Err(format!("containers {stuck:?} are still present after the stop; refusing to start another backend"));
            }
        }
        let Some(model) = to.and_then(|i| cfg.models.get(i)) else {
            return Ok(None);
        };
        let left = remove_containers(&cfg.docker, &model.containers).await;
        if !left.is_empty() {
            *stuck = left;
            return Err(format!(
                "containers {stuck:?} are present and could not be removed; refusing to start {}",
                model.name
            ));
        }
        rec.insert("stopped_s".into(), json!(round2(t0.elapsed())));
        if let Some(path) = &cfg.gpu_lock {
            let have = inner.state().lock.is_some();
            if !have {
                let t = Instant::now();
                let lock = gpu_lock::acquire(path, cfg.gpu_lock_timeout).await?;
                rec.insert("gpu_lock_wait_s".into(), json!(round2(t.elapsed())));
                inner.state().lock = Some(lock);
            }
        }
        let t1 = Instant::now();
        let mut backend = Backend::spawn(model, &cfg.logdir).map_err(|e| e.to_string())?;
        let mut shutdown = inner.shutdown.subscribe();
        let ready = backend.wait_ready(&inner.client, &mut shutdown).await;
        rec.insert("ready_s".into(), json!(round2(t1.elapsed())));
        match ready {
            Ok(()) => Ok(Some(backend)),
            Err(e) => {
                let report = backend.stop(&cfg.docker).await;
                put_stop(rec, "failed_stop", &report);
                stuck.extend(report.containers_left);
                Err(match e {
                    StartError::Cancelled => e.to_string(),
                    _ => format!("{}: {e}", model.name),
                })
            }
        }
    }

    /// Idle unloads (per-model `ttl`) and cleanup of a backend that died while idle. Call about
    /// once a second.
    pub fn tick(&self) {
        let inner = &self.inner;
        let mut s = inner.state();
        if s.swapping.is_some() || s.shutting_down || s.inflight > 0 || !s.waiters.is_empty() {
            return;
        }
        let Some(i) = s.loaded else { return };
        let died = s.active.as_mut().and_then(Backend::exited);
        let ttl = inner.config.models.get(i).and_then(|m| m.ttl);
        let reason = if let Some(status) = died {
            let name = inner.config.models.get(i).map(|m| m.name.clone());
            inner
                .events
                .emit("died", obj(json!({"model": name, "status": status})));
            "died"
        } else if ttl.is_some_and(|t| s.last_used.elapsed() > t) {
            "ttl"
        } else {
            return;
        };
        s.swapping = Some(None);
        drop(s);
        let router = self.clone();
        tokio::spawn(async move { router.swap(None, reason).await });
    }

    /// Refuse new and queued requests and cancel a start in progress; in-flight requests go on.
    pub fn begin_shutdown(&self) {
        let inner = &self.inner;
        inner.state().shutting_down = true;
        let _ = inner.shutdown.send(true);
        inner.notify.notify_waiters();
    }

    /// Refuse new requests, cancel a start in progress, wait for the running swap, then stop the
    /// loaded backend.
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        self.begin_shutdown();
        loop {
            let notified = inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if inner.state().swapping.is_none() {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(500), notified).await;
        }
        let (active, mut stuck) = {
            let mut s = inner.state();
            s.loaded = None;
            (s.active.take(), std::mem::take(&mut s.stuck))
        };
        let mut rec = Map::new();
        if let Some(b) = active {
            rec.insert("model".into(), json!(b.model.name));
            let report = b.stop(&inner.config.docker).await;
            put_stop(&mut rec, "stop", &report);
            stuck.extend(report.containers_left);
        }
        if !stuck.is_empty() {
            stuck = remove_containers(&inner.config.docker, &stuck).await;
        }
        rec.insert("stuck_containers".into(), json!(stuck));
        let mut s = inner.state();
        if stuck.is_empty() {
            s.lock = None;
        }
        s.stuck = stuck;
        drop(s);
        inner.events.emit("shutdown", rec);
    }

    pub fn status(&self) -> Value {
        let inner = &self.inner;
        let cfg = &inner.config;
        let s = inner.state();
        let name = |i: Option<usize>| i.and_then(|i| cfg.models.get(i));
        let loaded = name(s.loaded);
        json!({
            "loaded": loaded.map(|m| &m.name),
            "engine": loaded.map(|m| &m.engine),
            "swapping": s.swapping.is_some(),
            "swapping_to": s.swapping.and_then(|t| name(t).map(|m| m.name.clone())),
            "inflight": s.inflight,
            "waiting": s.waiters.len(),
            "swaps": s.swaps,
            "stuck_containers": s.stuck,
            "gpu_lock": cfg.gpu_lock.as_ref().map(|p| json!({"path": p, "held": s.lock.is_some()})),
            "history": inner.events.recent(20),
            "models": cfg.models.iter().map(|m| json!({
                "id": m.name, "engine": m.engine, "aliases": m.aliases, "devices": m.devices,
                "ttl": m.ttl.map(|t| t.as_secs_f64()), "unlisted": m.unlisted,
            })).collect::<Vec<_>>(),
        })
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn put_stop(rec: &mut Map<String, Value>, prefix: &str, report: &StopReport) {
    for (k, v) in obj(report.to_json()) {
        rec.insert(format!("{prefix}_{k}"), v);
    }
}
