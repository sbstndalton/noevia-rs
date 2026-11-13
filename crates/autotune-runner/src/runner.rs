//! The runner's state and its entry points: `start`, the per-model `run` loop, and the job record
//! helpers every stage uses (`save`, `note`, `check`). Core llamacpp-full-autotune.cjs
//! `createFullAutotuner`, lines 278-310 and 1536-1652.

use crate::deps::{Abort, Deps, Fail, Release as _, Spawn};
use crate::{js, kv, phases};
use js_json::{wtf16, JValue};
use std::sync::{Arc, Mutex, MutexGuard};

/// Version 3 of the history entries (`VERSION`).
pub const VERSION: u32 = 3;
/// #1062: waits for another router client per model before the tune stops as interrupted.
pub const FOREIGN_YIELDS: u32 = 6;
/// #1079: a Long tune's section suffix.
pub const LONG_SUFFIX: &str = "-long";
pub const LONG_PROFILE_REASON: &str = "Long-context profile — tuned with Long mode from its model";
/// Kept free beside the compact model for the services that run alongside it (Laya).
pub const SERVICES_RESERVE_MIB: u64 = 2560;

/// The tunables Node's `createFullAutotuner` takes with defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub between_models_ms: u64,
    pub idle_timeout_ms: u64,
    pub memory_floor_gib: f64,
    pub foreign_wait_ms: u64,
    pub foreign_quiet_ms: u64,
    pub foreign_poll_ms: u64,
    pub services_reserve_mib: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            between_models_ms: 1000,
            idle_timeout_ms: 300_000,
            memory_floor_gib: 2.0,
            foreign_wait_ms: 15 * 60_000,
            foreign_quiet_ms: 30_000,
            foreign_poll_ms: 2000,
            services_reserve_mib: SERVICES_RESERVE_MIB,
        }
    }
}

/// What a running stage's calibrator is, for cancellation (`child`).
pub(crate) enum Child<C> {
    /// contextStage's calibrator: cancelled through its job controller.
    Stage(Arc<C>),
    /// fillCheck's private probe wrapper: `{ cancel: () => cal.cancelProbe() }`.
    Probe(Arc<C>),
}

impl<C> Clone for Child<C> {
    fn clone(&self) -> Self {
        match self {
            Child::Stage(c) => Child::Stage(Arc::clone(c)),
            Child::Probe(c) => Child::Probe(Arc::clone(c)),
        }
    }
}

/// The live actor's private state (Node's closure variables).
pub(crate) struct St<C> {
    /// The state file's object: `{ history, settings, job? }` in file order.
    pub state: JValue,
    pub cancelled: bool,
    pub starting: bool,
    /// The identity of `completion` (a new run or resume bumps it).
    pub run: u64,
    /// The identity of the `state.job` object (a start replaces it).
    pub job_object: u64,
    pub child: Option<Child<C>>,
    pub inflight: Option<Abort>,
    pub idle_abort: Option<Abort>,
    pub clear_generation: u64,
    pub settings_generation: u64,
    pub resume_generation: u64,
    /// The identity of the pending `scheduleReload` timer (`clearTimeout` bumps it).
    pub reload_timer: u64,
    /// `resumePreparing` is held (prepareResume in progress).
    pub resume_preparing: bool,
    /// The process salt of every token (Node: random bytes and UUIDs made at construction).
    pub salt: String,
    /// Identities of the current `child`, `inflight` and `idleAbort` objects (0: none).
    pub child_id: u64,
    pub inflight_id: u64,
    pub idle_id: u64,
    pub seq: u64,
}

impl<C> St<C> {
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
    pub fn set_child(&mut self, child: Option<Child<C>>) {
        self.child_id = if child.is_some() { self.next() } else { 0 };
        self.child = child;
    }
    pub fn set_inflight(&mut self, abort: Option<Abort>) {
        self.inflight_id = if abort.is_some() { self.next() } else { 0 };
        self.inflight = abort;
    }
    pub fn set_idle(&mut self, abort: Option<Abort>) {
        self.idle_id = if abort.is_some() { self.next() } else { 0 };
        self.idle_abort = abort;
    }
}

/// 128 random bits from the standard library's per-process hash keys.
fn process_salt() -> String {
    use std::hash::{BuildHasher, Hasher};
    (0..2)
        .map(|_| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u8(0);
            format!("{:016x}", h.finish())
        })
        .collect()
}

pub(crate) struct Inner<D: Deps> {
    pub deps: D,
    pub config: Config,
    pub st: Mutex<St<D::Context>>,
}

/// The native auto-tune actor. Cloning gives another handle to the same actor.
pub struct Runner<D: Deps>(pub(crate) Arc<Inner<D>>);

impl<D: Deps> Clone for Runner<D> {
    fn clone(&self) -> Self {
        Runner(Arc::clone(&self.0))
    }
}

/// The callbacks a stage's calibrator gets (`contextFactory(hooks)`), bound to the run that made
/// it.
pub struct StageHooks<D: Deps> {
    pub(crate) runner: Runner<D>,
    /// The run and job this stage belongs to (`contextCurrent`).
    pub(crate) run: u64,
    pub(crate) job_object: u64,
    pub(crate) model: String,
    /// The item and phase whose steps `onUpdate` mirrors, the label prefix (verifyContext) and
    /// how many of the phase's steps come before the calibrator's (fixed when the stage starts).
    pub(crate) phase: Option<(usize, usize, String, usize)>,
    /// Set when a wait for another client inside the calibrator gave up or was cancelled.
    pub(crate) foreign_stop: Arc<Mutex<Option<Fail>>>,
    /// fillCheck's calibrator: given no `contextCurrent` (Node's check then reads `undefined`).
    pub(crate) fill: bool,
    /// The first state-file save that failed inside a callback: Node's callback would have thrown
    /// into the calibrator and ended its stage; the stage ends with it here once the calibrator
    /// returns.
    pub(crate) save_error: Arc<Mutex<Option<Fail>>>,
}

impl<D: Deps> Clone for StageHooks<D> {
    fn clone(&self) -> Self {
        StageHooks {
            runner: self.runner.clone(),
            run: self.run,
            job_object: self.job_object,
            model: self.model.clone(),
            phase: self.phase.clone(),
            foreign_stop: Arc::clone(&self.foreign_stop),
            fill: self.fill,
            save_error: Arc::clone(&self.save_error),
        }
    }
}

/// Clears `starting` when dropped (start's `finally`).
struct Starting<'a, D: Deps>(&'a Runner<D>);

impl<D: Deps> Drop for Starting<'_, D> {
    fn drop(&mut self) {
        self.0.st().starting = false;
    }
}

/// Locks, recovering from a poisoned lock (a panic elsewhere leaves the plain data usable).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The start request after the route's coercion (`routes/models.cjs`): the raw JSON values of
/// `confirmPause`, `promptBudgetSeconds` and `mode` (`None`: absent), and `untuned === true`.
#[derive(Debug, Clone, Default)]
pub struct StartBody {
    pub confirm_pause: Option<JValue>,
    pub prompt_budget_seconds: Option<JValue>,
    pub untuned: bool,
    pub mode: Option<JValue>,
}

/// An answer the route sends as it is: status and JSON body.
pub type Answer = (u16, JValue);

pub(crate) fn error_body(message: &str) -> JValue {
    JValue::obj([("error", JValue::from(message))])
}

/// `clientMessage(e, fallback)`: a sentence written for people, a client error's own message, or
/// the fixed fallback (the original is logged).
pub(crate) fn client_message<D: Deps>(deps: &D, e: &Fail, fallback: &str) -> String {
    if let Some(m) = &e.public_message {
        return m.clone();
    }
    if e.status.is_some_and(|s| s < 500) && !e.message.is_empty() {
        return e.message.clone();
    }
    deps.log(&format!("[autotune] {}", e.message));
    fallback.to_string()
}

/// `{ ...error, message: error.message + suffix, unsafe: true }`.
pub(crate) fn unsafe_restore(e: &Fail) -> Fail {
    Fail {
        unsafe_: true,
        message: format!(
            "{} Settings changed or restoration failed; inspect models.ini before resuming.",
            e.message
        ),
        // A new Error: none of the original's flags travel with it.
        ..Fail::default()
    }
}

impl<D: Deps> Runner<D> {
    /// `createFullAutotuner(...)`: reads the state file text (`None`, or text that does not parse
    /// as an object, starts empty, as Node's `catch { state = {} }`), then arms the startup
    /// recovery of a trial an earlier process left unrestored.
    pub fn new(deps: D, config: Config, state_text: Option<&str>) -> Result<Self, Fail> {
        // Node: `try { state = JSON.parse(text) } catch { state = {} }`, then `state.history ||= {}`,
        // which throws for null, a number, a string or a boolean (the auto-tuner is then not
        // built). An array would pass in Node and every later save would write `[]`, losing the
        // job: refused here too (a named difference).
        let mut state = match state_text.map(wtf16::parse) {
            None | Some(Err(_)) => JValue::Obj(Vec::new()),
            Some(Ok(v)) if v.is_object() => v,
            Some(Ok(_)) => return Err(Fail::new("The auto-tune state file is not a JSON object.")),
        };
        js::or_insert(&mut state, "history", JValue::Obj(Vec::new()));
        js::or_insert(&mut state, "settings", JValue::Obj(Vec::new()));
        let runner = Runner(Arc::new(Inner {
            deps,
            config,
            st: Mutex::new(St {
                state,
                cancelled: false,
                starting: false,
                run: 0,
                job_object: 1,
                child: None,
                inflight: None,
                idle_abort: None,
                clear_generation: 0,
                settings_generation: 0,
                resume_generation: 0,
                reload_timer: 0,
                resume_preparing: false,
                salt: process_salt(),
                child_id: 0,
                inflight_id: 0,
                idle_id: 0,
                seq: 0,
            }),
        }));
        runner.arm_startup_recovery()?;
        Ok(runner)
    }

    /// The dependencies this actor was built with.
    pub fn deps(&self) -> &D {
        &self.0.deps
    }

    pub(crate) fn config(&self) -> &Config {
        &self.0.config
    }

    pub(crate) fn st(&self) -> MutexGuard<'_, St<D::Context>> {
        lock(&self.0.st)
    }

    /// `save()`: the whole state, atomically; every token generation moves on.
    pub(crate) fn save(&self) -> Result<(), Fail> {
        let mut st = self.st();
        self.save_locked(&mut st)
    }

    /// `save()` with the state already locked (a compare-and-change keeps the lock throughout).
    pub(crate) fn save_locked(&self, st: &mut St<D::Context>) -> Result<(), Fail> {
        let text = wtf16::stringify(&st.state).unwrap_or_default();
        self.0.deps.save(&text)?;
        st.clear_generation += 1;
        st.settings_generation += 1;
        st.resume_generation += 1;
        Ok(())
    }

    /// `check()`: a cancelled run stops at its next step.
    pub(crate) fn check(&self) -> Result<(), Fail> {
        if self.st().cancelled {
            Err(Fail::cancelled())
        } else {
            Ok(())
        }
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.st().cancelled
    }

    /// Reads `state.job` (a clone; `Undefined` when there is none).
    pub(crate) fn job(&self) -> JValue {
        self.st().state.get("job").clone()
    }

    /// Mutates `state.job` in place (no-op without one).
    pub(crate) fn with_job<R>(&self, f: impl FnOnce(&mut JValue) -> R) -> Option<R> {
        let mut st = self.st();
        js::get_mut(&mut st.state, "job").map(f)
    }

    /// Mutates queue item `mi` of the job.
    pub(crate) fn with_item<R>(&self, mi: usize, f: impl FnOnce(&mut JValue) -> R) -> Option<R> {
        self.with_job(|j| {
            js::get_mut(j, "models")
                .and_then(js::items_mut)
                .and_then(|items| items.get_mut(mi))
                .map(f)
        })
        .flatten()
    }

    /// Mutates phase `pi` of queue item `mi`.
    pub(crate) fn with_phase<R>(
        &self,
        mi: usize,
        pi: usize,
        f: impl FnOnce(&mut JValue) -> R,
    ) -> Option<R> {
        self.with_item(mi, |item| {
            js::get_mut(item, "phases")
                .and_then(js::items_mut)
                .and_then(|p| p.get_mut(pi))
                .map(f)
        })
        .flatten()
    }

    pub(crate) fn item(&self, mi: usize) -> JValue {
        js::items(self.job().get("models"))
            .get(mi)
            .cloned()
            .unwrap_or(JValue::Undefined)
    }

    pub(crate) fn phase(&self, mi: usize, pi: usize) -> JValue {
        js::items(self.item(mi).get("phases"))
            .get(pi)
            .cloned()
            .unwrap_or(JValue::Undefined)
    }

    /// `phaseOf(item, id)`: the index of the phase with that id.
    pub(crate) fn phase_index(&self, mi: usize, id: &str) -> Option<usize> {
        js::items(self.item(mi).get("phases"))
            .iter()
            .position(|p| p.get("id").as_str() == Some(id))
    }

    /// The index of the queue item `j.model` names (`j.models.find(m => m.model === j.model)`).
    pub(crate) fn current_item(&self) -> Option<usize> {
        let j = self.job();
        js::items(j.get("models"))
            .iter()
            .position(|m| m.get("model") == j.get("model"))
    }

    /// `note(j, text)`: one log line (at most 400 kept), then a save.
    pub(crate) fn note(&self, text: &str) -> Result<(), Fail> {
        let at = self.0.deps.now();
        self.with_job(|j| {
            if let Some(log) =
                js::or_insert(j, "log", JValue::Arr(Vec::new())).and_then(js::items_mut)
            {
                log.push(JValue::obj([
                    ("at", JValue::Num(at)),
                    ("text", JValue::from(text)),
                ]));
                if log.len() > 400 {
                    log.remove(0);
                }
            }
        });
        self.save()
    }

    /// `presets.snapshot().revision !== j._revision`.
    pub(crate) fn revision_moved(&self) -> bool {
        let (_, revision) = self.0.deps.presets_snapshot();
        self.job().get("_revision").as_str() != Some(revision.as_str())
    }

    /// The revision guard every stage starts with.
    pub(crate) fn guard_revision(&self) -> Result<(), Fail> {
        if self.revision_moved() {
            Err(Fail::outside_edit())
        } else {
            Ok(())
        }
    }

    /// `settingsOf(model).allowQ5Kv`.
    pub(crate) fn allow_q5(&self, model: &str) -> bool {
        self.st().state.get("settings").get(model).get("allowQ5Kv") == &JValue::Bool(true)
    }

    /// `candidatesFor(model)`.
    pub(crate) fn candidates_for(&self, model: &str) -> Vec<JValue> {
        kv::candidates(self.allow_q5(model), self.0.deps.allow_below_q5())
            .into_iter()
            .map(JValue::from)
            .collect()
    }

    /// `itemKv(item)`: the item's own list, or (a job from before #1057) today's.
    pub(crate) fn item_kv(&self, item: &JValue) -> Vec<JValue> {
        match item.get("kv") {
            JValue::Arr(list) if !list.is_empty() => list.clone(),
            _ => self.candidates_for(item.get("model").as_str().unwrap_or("")),
        }
    }

    /// `start(model, { confirmPause, promptBudgetSeconds, untuned, mode })`. `Err` is an
    /// exception Node's `start` does not catch (the route answers it as an internal error).
    pub async fn start(&self, model: &str, body: StartBody) -> Result<Answer, Fail> {
        let bulk = body.untuned;
        // Node's checks up to `starting = true` run in one synchronous turn, so no second start or
        // resume can slip between the "already running" check and the flag: one lock here.
        let plan = {
            let mut st = self.st();
            let run_active =
                st.starting || st.state.get("job").get("status").as_str() == Some("running");
            let admission = autotune_start::admit(&autotune_start::Start {
                confirm_pause: body.confirm_pause.as_ref(),
                mode: body.mode.as_ref(),
                prompt_budget_seconds: body.prompt_budget_seconds.as_ref(),
                bulk,
                run_active,
                // Asked below, only where Node asks it (a single model, every earlier check passed).
                system_model: false,
            });
            let plan = match admission {
                Ok(plan) => plan,
                Err(r) => return Ok((r.status, error_body(r.message))),
            };
            if !bulk && self.0.deps.is_system_model(model, None) {
                return Ok((400, error_body(autotune_start::SYSTEM_MODEL_REASON)));
            }
            // #223: the context ladder must be readable before any hold or write.
            if let Err(err) = self.0.deps.ladder(0) {
                if err.code.as_deref() != Some("autoconfig_impl") {
                    return Err(err);
                }
                return Ok((
                    // `err.status || 503`.
                    err.status.filter(|s| *s != 0).unwrap_or(503),
                    JValue::obj([
                        (
                            "error",
                            JValue::from(format!("Auto-tune is unavailable: {}", err.message)),
                        ),
                        ("code", JValue::from("autoconfig_impl")),
                    ]),
                ));
            }
            st.starting = true;
            plan
        };
        // Node's `finally { starting = false }`, also when this future is dropped mid-start.
        let _starting = Starting(self);
        let mode = match plan.mode {
            autotune_start::Mode::Fast => "fast",
            autotune_start::Mode::Long => "long",
        };
        let result = self
            .start_prepared(model, bulk, mode, f64::from(plan.prompt_budget_seconds))
            .await;
        Ok(match result {
            Ok(answer) => answer,
            Err(e) => (
                409,
                error_body(&client_message(
                    &self.0.deps,
                    &e,
                    "Auto-tune could not start.",
                )),
            ),
        })
    }

    async fn start_prepared(
        &self,
        model: &str,
        bulk: bool,
        mode: &str,
        prompt_budget_seconds: f64,
    ) -> Result<Answer, Fail> {
        self.flush_reload(None).await?;
        let scan = self.candidates().await?;
        let skipped = js::items(scan.get("skipped"));
        if !bulk
            && skipped.iter().any(|s| {
                s.get("model").as_str() == Some(model)
                    && s.get("reason").as_str() == Some(LONG_PROFILE_REASON)
            })
        {
            return Ok((
                400,
                error_body(
                    "This is a long-context profile. Open its model and run a Long tune there.",
                ),
            ));
        }
        let scan_models = js::items(scan.get("models"));
        let models: Vec<JValue> = if bulk {
            scan_models.to_vec()
        } else {
            vec![JValue::from(model)]
        };
        let listed = scan_models.iter().any(|m| m.as_str() == Some(model))
            || skipped.iter().any(|s| {
                s.get("reason").as_str() == Some("Current tune already applied")
                    && s.get("model").as_str() == Some(model)
            });
        if !bulk && !listed {
            return Ok((400, error_body("Choose a configured chat model.")));
        }
        if models.is_empty() {
            return Ok((
                409,
                error_body("All configured chat models already have current tunes."),
            ));
        }
        let mut items: Vec<JValue> = models
            .iter()
            .map(|m| {
                let kv = self.candidates_for(&js_json::to_js_string(m).unwrap_or_default());
                phases::new_model(m, true, &kv)
            })
            .collect();
        if mode == "long" {
            let long_id = format!("{model}{LONG_SUFFIX}");
            let (_, pairs) = self.router_pairs().await?;
            let paired = js::items(&pairs).iter().any(|p| {
                p.get("base").as_str() == Some(model) && p.get("long").as_str() == Some(&long_id)
            });
            let exists = self.0.deps.presets_get(&long_id).get("exists").truthy();
            if exists && !paired {
                return Ok((409, error_body(&crate::long::clash(&long_id))));
            }
            if !exists {
                let listing = self.raw_models().await?;
                let row = js::items(listing.get("body").get("data"))
                    .iter()
                    .find(|r| r.get("id").as_str() == Some(model))
                    .cloned();
                let dry = match row {
                    Some(row) => {
                        let (text, _) = self.0.deps.presets_snapshot();
                        self.0.deps.long_section(&text, &row)
                    }
                    None => JValue::obj([
                        ("ok", JValue::Bool(false)),
                        ("reason", JValue::from("no_base")),
                    ]),
                };
                if !dry.get("ok").truthy() {
                    return Ok((409, error_body(crate::long::refusal(dry.get("reason")))));
                }
            }
            let mut item = phases::new_model(
                &JValue::from(long_id.as_str()),
                true,
                &self.candidates_for(model),
            );
            js::set(&mut item, "base", model);
            js::set(&mut item, "mode", "long");
            items = vec![item];
        }
        let (_, revision) = self.0.deps.presets_snapshot();
        let total = items.len();
        let first = items
            .first()
            .map(|i| i.get("model").clone())
            .unwrap_or(JValue::Undefined);
        let j = JValue::obj([
            ("id", JValue::from(self.0.deps.random_uuid())),
            ("model", first),
            ("bulk", JValue::Bool(bulk)),
            ("mode", JValue::from(mode)),
            ("promptBudgetSeconds", JValue::Num(prompt_budget_seconds)),
            ("status", JValue::from("running")),
            ("phase", JValue::from("Preparing")),
            ("startedAt", JValue::Num(self.0.deps.now())),
            ("log", JValue::Arr(Vec::new())),
            ("models", JValue::Arr(items)),
            ("_revision", JValue::from(revision)),
            (
                "queueProgress",
                JValue::obj([
                    ("done", JValue::Num(0.0)),
                    ("total", JValue::Num(total as f64)),
                ]),
            ),
        ]);
        {
            let mut st = self.st();
            st.cancelled = false;
            js::set(&mut st.state, "job", j);
            st.job_object += 1;
        }
        self.save()?;
        self.launch();
        Ok((202, phases::public_job(&self.job())))
    }

    /// `completion = run(j)`: a new run identity. Node runs `run(j)` synchronously up to its first
    /// `await` (the hold for the first unfinished item) before `start` returns, so that much runs
    /// here too; a run with nothing to wait for ends here entirely.
    pub(crate) fn launch(&self) {
        self.st().run += 1;
        match self.run_prologue(0) {
            Ok(Some(index)) => {
                let me = self.clone();
                self.0.deps.spawn(Spawn::Run, async move {
                    let outcome = me.run_items(index).await;
                    me.finish_run(outcome);
                });
            }
            Ok(None) => {
                self.all_passed();
                self.finish_run(Ok(()));
            }
            Err(e) => self.finish_run(Err(e)),
        }
    }

    /// The loop head of `run(j)` from item `from`: the next unfinished item named and announced
    /// (`None`: every item has passed).
    fn run_prologue(&self, from: usize) -> Result<Option<usize>, Fail> {
        let count = js::items(self.job().get("models")).len();
        for index in from..count {
            self.check()?;
            let item = self.item(index);
            if item.get("status").as_str() == Some("passed") {
                continue;
            }
            let model = item.get("model").clone();
            self.with_job(|j| {
                js::set(j, "model", model);
                js::set(j, "phase", "Waiting for chat to become idle");
                js::set(j, "waiting", true);
            });
            self.save()?;
            self.st().set_idle(Some(Abort::default()));
            return Ok(Some(index));
        }
        Ok(None)
    }

    fn all_passed(&self) {
        self.with_job(|j| {
            js::set(j, "status", "passed");
            js::set(j, "phase", "Done");
        });
    }

    /// The end of `run(j)` (its `catch` and `finally`): the job's end state, then a save.
    fn finish_run(&self, outcome: Result<(), Fail>) {
        if let Err(e) = outcome {
            let cancelled = self.cancelled() || e.cancelled;
            let status = if cancelled {
                "cancelled"
            } else if e.idle_timeout || e.foreign_timeout {
                "interrupted"
            } else {
                "failed"
            };
            self.with_job(|j| {
                js::set(j, "status", status);
                js::set(
                    j,
                    "phase",
                    match status {
                        "cancelled" => "Cancelled",
                        "interrupted" => "Waiting timed out",
                        _ => "Failed",
                    },
                );
                js::set(j, "error", e.message.as_str());
                if e.unsafe_ {
                    js::set(j, "_unsafe", true);
                }
                let model = j.get("model").clone();
                if let Some(items) = js::get_mut(j, "models").and_then(js::items_mut) {
                    if let Some(item) = items.iter_mut().find(|m| {
                        m.get("model") == &model && m.get("status").as_str() == Some("running")
                    }) {
                        js::set(
                            item,
                            "status",
                            if status == "cancelled" {
                                "interrupted"
                            } else {
                                "failed"
                            },
                        );
                        js::set(item, "error", e.message.as_str());
                    }
                }
                phases::stopped_at(j);
            });
        }
        let now = self.0.deps.now();
        self.with_job(|j| {
            js::set(j, "finishedAt", JValue::Num(now));
            js::set(j, "waiting", false);
        });
        {
            let mut st = self.st();
            st.set_child(None);
            st.set_idle(None);
        }
        // The final save has nowhere to report a failure (Node's `finally` would reject the
        // completion promise, which nothing awaits); the file keeps its last good state.
        if let Err(e) = self.save() {
            self.0
                .deps
                .log(&format!("[autotune] final save failed: {}", e.message));
        }
    }

    /// The rest of `run(j)` from item `index`, whose prologue has run.
    async fn run_items(&self, first: usize) -> Result<(), Fail> {
        let mut next = Some(first);
        while let Some(index) = next {
            let count = js::items(self.job().get("models")).len();
            let model_name = self
                .item(index)
                .get("model")
                .as_str()
                .unwrap_or("")
                .to_string();
            let idle = self.st().idle_abort.clone().unwrap_or_default();
            let mut release: Option<D::Release> = None;
            let outcome = self.run_item(index, &model_name, idle, &mut release).await;
            self.st().set_idle(None);
            self.with_job(|j| js::set(j, "waiting", false));
            if let Some(r) = &release {
                r.release();
            }
            self.save()?;
            outcome?;
            self.with_job(|j| {
                let done = js::items(j.get("models"))
                    .iter()
                    .filter(|m| m.get("status").as_str() == Some("passed"))
                    .count();
                let total = js::items(j.get("models")).len();
                js::set(
                    j,
                    "queueProgress",
                    JValue::obj([
                        ("done", JValue::Num(done as f64)),
                        ("total", JValue::Num(total as f64)),
                    ]),
                );
            });
            self.save()?;
            if index + 1 < count {
                self.with_job(|j| js::set(j, "phase", "Chat is available between models"));
                self.save()?;
                self.0.deps.sleep(self.0.config.between_models_ms).await;
                self.check()?;
            }
            next = self.run_prologue(index + 1)?;
        }
        self.all_passed();
        Ok(())
    }

    async fn run_item(
        &self,
        index: usize,
        model: &str,
        idle: Abort,
        release_slot: &mut Option<D::Release>,
    ) -> Result<(), Fail> {
        let release = self
            .0
            .deps
            .hold_when_idle(
                &format!("Chat is paused while noevia tunes {model}: preparing."),
                self.0.config.idle_timeout_ms,
                idle,
            )
            .await?;
        // Kept before the save, as Node assigns `release` first: a failed save still gives the
        // hold back (the loop's `finally`).
        *release_slot = Some(release);
        self.st().set_idle(None);
        self.with_job(|j| js::set(j, "waiting", false));
        self.with_item(index, |item| js::set(item, "status", "running"));
        self.save()?;
        let release = release_slot.as_ref().ok_or_else(|| Fail::new("no hold"))?;
        if self.item(index).get("mode").as_str() == Some("long") {
            self.ensure_long_section(index).await?;
        }
        let identity = self
            .0
            .deps
            .identity_for(model)
            .await?
            .filter(JValue::truthy)
            .ok_or_else(|| Fail::fatal("Model identity could not be read."))?;
        let stable = crate::stable_identity(&identity);
        let known = self.item(index).get("_identity").clone();
        if known.truthy() && known.as_str() != Some(stable.as_str()) {
            return Err(Fail::fatal("Model identity changed since tuning began."));
        }
        self.with_item(index, |item| js::set(item, "_identity", stable.as_str()));
        self.guard_revision()?;
        let planned =
            self.item(index).get("plan").truthy() && self.run_planned(index, release).await?;
        if !planned {
            let count = js::items(self.item(index).get("phases")).len();
            for pi in 0..count {
                let p = self.phase(index, pi);
                if p.get("status").as_str() == Some("passed") {
                    continue;
                }
                self.check()?;
                let label = p.get("label").as_str().unwrap_or("").to_string();
                self.with_job(|j| js::set(j, "phase", label.as_str()));
                release.set_reason(&format!(
                    "Chat is paused while noevia tunes {model}: {}.",
                    label.to_lowercase()
                ));
                self.run_phase(index, pi).await?;
            }
            self.guard_revision()?;
            self.with_job(|j| js::set(j, "phase", "Verifying saved profile"));
            self.save()?;
            self.unload_all(false).await?;
            self.load().await?;
            let mut fin = self.validate(model, None).await?;
            self.check()?;
            self.with_job(|j| js::set(j, "phase", "Checking a realistic chat request"));
            self.save()?;
            let serving = self.serving_check(model).await?;
            js::set(&mut fin, "serving", serving);
            self.check()?;
            self.finish_model(index, &fin, None).await?;
        }
        Ok(())
    }
}
