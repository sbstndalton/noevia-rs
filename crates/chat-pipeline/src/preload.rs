//! #1863: the shared Fast/Smart model loaded beside the routing decision (core route-preload.cjs).
//!
//! Exclusion-settle: a pre-load has exactly one load, a [`LoadTask`] the engine owns outside any caller's
//! future (Node's one eagerly created `settled` promise). [`Preload::run`] and [`Preload::release_if_unused`]
//! only wait on it; cancelling signals it and still waits for its actual settlement before anything is read or
//! unloaded. A release dropped at any await hands the unfinished cleanup to the engine's settle owner
//! ([`Engine::adopt`], which runs [`settle_orphan`] to completion).
use crate::text::{as_str, get};
use js_json::JValue;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// route-preload.cjs `PRELOAD_ROLES`.
pub const PRELOAD_ROLES: [&str; 4] = ["fast", "smart", "code", "creative"];

/// The roles a pre-load reads (`autoRoles()`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roles {
    pub fast: Option<String>,
    pub smart: Option<String>,
    pub code: Option<String>,
    pub creative: Option<String>,
}

impl Roles {
    fn get(&self, role: &str) -> Option<&str> {
        match role {
            "fast" => self.fast.as_deref(),
            "smart" => self.smart.as_deref(),
            "code" => self.code.as_deref(),
            "creative" => self.creative.as_deref(),
            _ => None,
        }
        .filter(|m| !m.is_empty())
    }
}

/// `preloadTarget({ roles, remoteRole, hasImages, contextHigh, likelyCode })`: the shared local model when a
/// pre-load is safe for this turn. `likely_code` is asked only when Code has its own model.
pub fn preload_target(
    roles: Option<&Roles>,
    remote_role: &dyn Fn(&str) -> bool,
    has_images: bool,
    context_high: bool,
    likely_code: &dyn Fn() -> bool,
) -> Option<String> {
    let roles = roles?;
    if has_images || context_high {
        return None;
    }
    let model = roles.get("fast").or(roles.get("smart"))?;
    if let (Some(f), Some(s)) = (roles.get("fast"), roles.get("smart")) {
        if f != s {
            return None;
        }
    }
    if roles.get("creative").is_some_and(|c| c != model) {
        return None;
    }
    if PRELOAD_ROLES
        .iter()
        .any(|r| roles.get(r).is_some() && remote_role(r))
    {
        return None;
    }
    if roles.get("code").is_some_and(|c| c != model) && likely_code() {
        return None;
    }
    Some(model.to_owned())
}

/// The one load of a pre-load, started by [`Engine::begin_load`] and owned by the engine (a spawned task in
/// production): dropping a handle or a waiter never aborts or abandons it.
#[allow(async_fn_in_trait)]
pub trait LoadTask: Clone {
    /// Signals the load to stop (`controller.abort`). It may go on until it settles.
    fn cancel(&self);
    /// Resolves once the load has actually settled: true when it answered ok. Any number of waiters see the
    /// same outcome.
    async fn settled(&self) -> bool;
}

/// The local engine a pre-load drives (the model manager; llama.cpp only).
#[allow(async_fn_in_trait)]
pub trait Engine {
    type Load: LoadTask;
    /// `manager.kind === 'llamacpp'` with load and listModels.
    fn can_preload(&self) -> bool;
    /// `busy()`: the engine has other work.
    fn busy(&self) -> bool;
    /// The turn's `signal.aborted`.
    fn aborted(&self) -> bool;
    /// `listModels()`: the rows (`body.data`), `None` when not ok or unreadable.
    async fn list_models(&self) -> Option<Vec<JValue>>;
    /// Starts `load(model)` under the pre-load's own cancellation, once.
    fn begin_load(&self, model: &str) -> Self::Load;
    /// `unload(model)`: true when it answered ok. `None`: the manager has no unload.
    async fn unload(&self, model: &str) -> Option<bool>;
    /// `sleep(settleMs)`.
    async fn sleep(&self);
    /// Whether `waitMs` has passed since the release began.
    fn past_wait(&self) -> bool;
    fn log(&self, line: String);
    /// Takes over a release its turn dropped before it finished: the engine's settle owner must run
    /// [`settle_orphan`] on it to completion.
    fn adopt(&self, orphan: Orphan<Self::Load>);
}

/// A release handed to the settle owner.
pub struct Orphan<L> {
    pub model: String,
    pub load: L,
    used: Arc<AtomicBool>,
}

fn status(row: &JValue) -> Option<&str> {
    as_str(get(get(row, "status"), "value"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Release {
    Open,
    Running,
    Done,
}

/// A started pre-load.
pub struct Preload<'e, E: Engine> {
    pub model: String,
    engine: &'e E,
    load: E::Load,
    used: Arc<AtomicBool>,
    release: Cell<Release>,
    settled: Cell<Option<bool>>,
}

/// `startPreload({ manager, model, signal, busy, log })`: `None` when no pre-load is started (the turn is
/// byte-identical). Otherwise its one load has begun; [`Preload::run`] waits for it beside the routing decision.
pub async fn start<'e, E: Engine>(engine: &'e E, model: &str) -> Option<Preload<'e, E>> {
    if model.is_empty() || !engine.can_preload() || engine.aborted() || engine.busy() {
        return None;
    }
    let rows = engine.list_models().await?;
    let row = rows.iter().find(|m| as_str(get(m, "id")) == Some(model))?;
    if get(row, "loaded").truthy() || status(row) != Some("unloaded") {
        return None;
    }
    if rows.iter().any(|m| {
        as_str(get(m, "id")) != Some(model)
            && (get(m, "loaded").truthy()
                || matches!(status(m), Some("loaded" | "loading" | "sleeping")))
    }) {
        return None;
    }
    if engine.aborted() || engine.busy() {
        return None;
    }
    engine.log(format!(
        "[preload] {model}: loading beside the routing decision"
    ));
    let load = engine.begin_load(model);
    Some(Preload {
        model: model.to_owned(),
        engine,
        load,
        used: Arc::new(AtomicBool::new(false)),
        release: Cell::new(Release::Open),
        settled: Cell::new(None),
    })
}

/// Clears a running release that was dropped: its cleanup goes to the settle owner.
struct ReleaseGuard<'p, 'e, E: Engine> {
    pre: &'p Preload<'e, E>,
    armed: bool,
}

impl<E: Engine> Drop for ReleaseGuard<'_, '_, E> {
    fn drop(&mut self) {
        if self.armed {
            self.pre.engine.adopt(Orphan {
                model: self.pre.model.clone(),
                load: self.pre.load.clone(),
                used: Arc::clone(&self.pre.used),
            });
        }
        self.pre.release.set(Release::Done);
    }
}

impl<E: Engine> Preload<'_, E> {
    /// The load (`settled`): true when the model is ready. Waits on the one load; never starts another.
    pub async fn run(&self) -> bool {
        if let Some(v) = self.settled.get() {
            return v;
        }
        let ok = self.load.settled().await;
        if self.settled.get().is_none() {
            self.settled.set(Some(ok));
            self.engine.log(format!(
                "[preload] {}: {}",
                self.model,
                if ok { "ready" } else { "not loaded" }
            ));
        }
        ok
    }

    /// `pending()`: the load has not been seen to settle.
    pub fn pending(&self) -> bool {
        self.settled.get().is_none()
    }

    /// `use()`: the turn answers on the pre-loaded model.
    pub fn use_it(&self) {
        self.used.store(true, Ordering::SeqCst);
    }

    /// `cancel()`.
    pub fn cancel(&self) {
        self.load.cancel();
    }

    /// `releaseIfUnused({ idle })` (#1863 D2): the turn ended before it used the pre-load; once the engine has
    /// finished loading it, unload it when nothing else runs. Dropped at any await, the rest of the release
    /// goes to [`Engine::adopt`].
    pub async fn release_if_unused(&self, idle: &dyn Fn() -> bool) -> bool {
        if self.used.load(Ordering::SeqCst) || self.release.get() != Release::Open {
            return false;
        }
        self.release.set(Release::Running);
        let mut guard = ReleaseGuard { pre: self, armed: true };
        self.load.cancel();
        self.run().await;
        let out = release_rest(self.engine, &self.model, &self.used, idle).await;
        guard.armed = false;
        drop(guard);
        out
    }
}

/// The settle owner's half of a dropped release: waits for the one load, then unloads the model when it
/// stayed unused and the engine is idle.
pub async fn settle_orphan<E: Engine>(engine: &E, orphan: Orphan<E::Load>, idle: &dyn Fn() -> bool) -> bool {
    if orphan.used.load(Ordering::SeqCst) {
        return false;
    }
    orphan.load.cancel();
    orphan.load.settled().await;
    release_rest(engine, &orphan.model, &orphan.used, idle).await
}

async fn release_rest<E: Engine>(engine: &E, model: &str, used: &AtomicBool, idle: &dyn Fn() -> bool) -> bool {
    let st = loop {
        engine.sleep().await;
        if used.load(Ordering::SeqCst) {
            return false;
        }
        let st: Option<String> = engine.list_models().await.and_then(|rows| {
            rows.iter()
                .find(|m| as_str(get(m, "id")) == Some(model))
                .and_then(|r| status(r).map(str::to_owned))
        });
        if st.as_deref() != Some("loading") || engine.past_wait() {
            break st;
        }
    };
    if st.as_deref() != Some("loaded") || used.load(Ordering::SeqCst) || !idle() {
        return false;
    }
    let Some(ok) = engine.unload(model).await else {
        return false;
    };
    engine.log(format!(
        "[preload] {model}: unused by its turn, {}",
        if ok { "unloaded" } else { "left loaded" }
    ));
    ok
}
