//! #1863: the shared Fast/Smart model loaded beside the routing decision (core route-preload.cjs).
//!
//! Exclusion-settle: a pre-load's load is never abandoned unsettled. [`Preload::release_if_unused`]
//! cancels it, then waits for the load to settle before it reads the engine and may unload.
use crate::text::{as_str, get};
use js_json::JValue;
use std::cell::Cell;

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
    if PRELOAD_ROLES.iter().any(|r| roles.get(r).is_some() && remote_role(r)) {
        return None;
    }
    if roles.get("code").is_some_and(|c| c != model) && likely_code() {
        return None;
    }
    Some(model.to_owned())
}

/// The local engine a pre-load drives (the model manager; llama.cpp only).
#[allow(async_fn_in_trait)]
pub trait Engine {
    /// `manager.kind === 'llamacpp'` with load and listModels.
    fn can_preload(&self) -> bool;
    /// `busy()`: the engine has other work.
    fn busy(&self) -> bool;
    /// The turn's `signal.aborted`.
    fn aborted(&self) -> bool;
    /// `listModels()`: the rows (`body.data`), `None` when not ok or unreadable.
    async fn list_models(&self) -> Option<Vec<JValue>>;
    /// `load(model)` under the pre-load's own cancellation; true when it answered ok. Cancelled
    /// ([`Engine::cancel_load`]) or failed, false.
    async fn load(&self, model: &str) -> bool;
    /// Aborts the pre-load's load (`controller.abort`).
    fn cancel_load(&self);
    /// `unload(model)`: true when it answered ok. `None`: the manager has no unload.
    async fn unload(&self, model: &str) -> Option<bool>;
    /// `sleep(settleMs)`.
    async fn sleep(&self);
    /// Whether `waitMs` has passed since the release began.
    fn past_wait(&self) -> bool;
    fn log(&self, line: String);
}

fn status(row: &JValue) -> Option<&str> {
    as_str(get(get(row, "status"), "value"))
}

/// A started pre-load.
pub struct Preload {
    pub model: String,
    used: Cell<bool>,
    released: Cell<bool>,
    settled: Cell<Option<bool>>,
}

/// `startPreload({ manager, model, signal, busy, log })`, up to the load: `None` when no pre-load is
/// started (the turn is byte-identical). Then [`Preload::run`] is the load itself, polled beside the
/// routing decision.
pub async fn start(engine: &impl Engine, model: &str) -> Option<Preload> {
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
            && (get(m, "loaded").truthy() || matches!(status(m), Some("loaded" | "loading" | "sleeping")))
    }) {
        return None;
    }
    if engine.aborted() || engine.busy() {
        return None;
    }
    engine.log(format!("[preload] {model}: loading beside the routing decision"));
    Some(Preload { model: model.to_owned(), used: Cell::new(false), released: Cell::new(false), settled: Cell::new(None) })
}

impl Preload {
    /// The load (`settled`): true when the model is ready.
    pub async fn run(&self, engine: &impl Engine) -> bool {
        if let Some(v) = self.settled.get() {
            return v;
        }
        let ok = engine.load(&self.model).await;
        engine.log(format!("[preload] {}: {}", self.model, if ok { "ready" } else { "not loaded" }));
        self.settled.set(Some(ok));
        ok
    }

    /// `pending()`: the load has not settled.
    pub fn pending(&self) -> bool {
        self.settled.get().is_none()
    }

    /// `use()`: the turn answers on the pre-loaded model.
    pub fn use_it(&self) {
        self.used.set(true);
    }

    /// `cancel()`.
    pub fn cancel(&self, engine: &impl Engine) {
        engine.cancel_load();
    }

    /// `releaseIfUnused({ idle })` (#1863 D2): the turn ended before it used the pre-load; once the engine has
    /// finished loading it, unload it when nothing else runs. The load is cancelled and then awaited until it
    /// settles before anything is read or unloaded.
    pub async fn release_if_unused(&self, engine: &impl Engine, idle: &dyn Fn() -> bool) -> bool {
        if self.used.get() || self.released.get() {
            return false;
        }
        self.released.set(true);
        engine.cancel_load();
        self.run(engine).await;
        let st = loop {
            engine.sleep().await;
            if self.used.get() {
                return false;
            }
            let st: Option<String> = engine
                .list_models()
                .await
                .and_then(|rows| rows.iter().find(|m| as_str(get(m, "id")) == Some(&self.model)).and_then(|r| status(r).map(str::to_owned)));
            if st.as_deref() != Some("loading") || engine.past_wait() {
                break st;
            }
        };
        if st.as_deref() != Some("loaded") || self.used.get() || !idle() {
            return false;
        }
        let Some(ok) = engine.unload(&self.model).await else {
            return false;
        };
        engine.log(format!("[preload] {}: unused by its turn, {}", self.model, if ok { "unloaded" } else { "left loaded" }));
        ok
    }
}
