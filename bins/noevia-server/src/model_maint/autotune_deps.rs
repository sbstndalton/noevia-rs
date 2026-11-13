//! The native auto-tune runner's dependencies (`autotune_runner::Deps`) over this front's model
//! maintenance (sbstndalton/noevia#2516): what core llamacpp-manager.cjs hands
//! `createFullAutotuner` (lines 730-790), served by the router client, the models.ini store, the
//! maintenance hold, the reload guard, the evidence store and the Rust decisions Node reached
//! through dav-parse.wasm. Built only when `AUTOTUNE_RUNNER_IMPL=rust`; nothing here runs otherwise.

use super::autotune_context::{Limits, PrivateCalibrator};
use super::{Hold, Maint};
use autotune_runner::{Abort, Call, Deps, Fail, Quirks, Release, Spawn, StageHooks};
use js_json::{wtf16, JValue};
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The variable both processes read (core rust-autotune-runner.cjs `IMPL_ENV`).
pub const IMPL_ENV: &str = "AUTOTUNE_RUNNER_IMPL";

/// `frontOwnsRunner(env)`: exactly `rust`.
pub fn front_owns_runner(get: &impl Fn(&str) -> Option<String>) -> bool {
    get(IMPL_ENV).as_deref() == Some("rust")
}

/// The poll while chat is busy (`holdWhenIdle` waits for an idle engine).
const IDLE_POLL: Duration = Duration::from_millis(500);
/// How long a facts read from Core may take before the last answer stands.
const FACTS_LIMIT: Duration = Duration::from_secs(3);
/// How often a call in flight checks that its hold still excludes everyone.
const LOSS_POLL: Duration = Duration::from_millis(500);
/// llamacpp-manager.cjs `PRESET_STEP_MS`: a write and its 120 s router reload.
const PRESET_STEP: Duration = Duration::from_secs(150);
/// The longest `recoverNow` waits for the owed repairs.
const RECOVER_NOW_LIMIT: Duration = Duration::from_secs(15 * 60);
/// A run's lease (Node's maintenance hold has no limit of its own; the authority caps it).
const RUN_LEASE: Duration = Duration::from_secs(24 * 3600);

fn to_j(v: &Value) -> JValue {
    wtf16::parse(&v.to_string()).unwrap_or(JValue::Null)
}

fn to_v(v: &JValue) -> Value {
    wtf16::stringify(v)
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn reply_j(r: &super::router::Reply) -> JValue {
    let mut out = JValue::obj([
        ("ok", JValue::Bool(r.ok)),
        ("status", JValue::Num(f64::from(r.status))),
    ]);
    if let Some(body) = &r.body {
        if let JValue::Obj(items) = &mut out {
            wtf16::set(items, "body", to_j(body));
        }
    }
    out
}

/// Node's `fetch failed` for a router call that did not answer (the runner's engine boundary
/// turns it into its fixed sentence).
fn fetch_failed(abort: Option<&Abort>) -> Fail {
    if abort.is_some_and(Abort::aborted) {
        return Fail {
            abort: true,
            ..Fail::new("This operation was aborted")
        };
    }
    Fail::new("fetch failed")
}

/// `assertCovered`'s refusal (inference-maintenance.cjs `coverageError`).
fn coverage_error() -> Fail {
    Fail {
        coverage_lost: true,
        status: Some(503),
        code: Some("maintenanceCoverageLost".into()),
        ..Fail::new("The maintenance lease was ended by the lease authority, so this work stopped before changing anything else.")
    }
}

/// The hold a run is under, shared by its router calls (the hold's token rides on every request)
/// and its release.
#[derive(Default)]
struct Current(Mutex<Option<Arc<Hold>>>);

impl Current {
    fn get(&self) -> Option<Arc<Hold>> {
        self.0.lock().ok().and_then(|h| h.clone())
    }
    fn set(&self, hold: Option<Arc<Hold>>) {
        if let Ok(mut h) = self.0.lock() {
            *h = hold;
        }
    }
}

/// Reads Core's inference budget (0 when it cannot be read) and its sidecars' model names.
pub type ReadFacts = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn Future<Output = (f64, Option<std::collections::HashSet<String>>)> + Send>,
        > + Send
        + Sync,
>;

pub struct FrontDeps {
    maint: Arc<Maint>,
    state_file: PathBuf,
    current: Arc<Current>,
    /// Core's facts as last read: its inference budget in GiB (0: none) and the sidecars' model
    /// names (Node reads both in memory, synchronously).
    facts: Arc<Mutex<(f64, Option<std::collections::HashSet<String>>)>>,
    /// Signalled when an owed repair has run (`recoverNow` waits for them).
    repaired: tokio::sync::Notify,
    /// Reads those facts from Core.
    read_facts: ReadFacts,
    /// The repairs `requireRecovery` asks for, run by the wiring under a fresh hold.
    recoveries: tokio::sync::mpsc::UnboundedSender<String>,
    /// Each owed repair's fences, kept until it has run (Node's gate keeps chat and new maintenance
    /// closed meanwhile): the lease authority's recovery ticket and this process's admission fence.
    owed: Mutex<std::collections::HashMap<String, (String, Option<super::gate::AdmissionFence>)>>,
    allow_below_q5: bool,
    /// The private calibrator's free-memory floor (index.cjs:
    /// `Number(LLAMACPP_CALIBRATION_MEMORY_FLOOR_GIB) || 2`).
    memory_floor_gib: f64,
    /// EMBEDDING_MODEL (or EMBED_MODEL), EMBEDDING_NEXT_MODEL and RERANK_MODEL, until Core answers.
    env_sidecars: Vec<String>,
}

impl FrontDeps {
    pub fn new(
        maint: Arc<Maint>,
        state_file: PathBuf,
        read_facts: ReadFacts,
        recoveries: tokio::sync::mpsc::UnboundedSender<String>,
        get: &impl Fn(&str) -> Option<String>,
    ) -> Self {
        FrontDeps {
            maint,
            state_file,
            current: Arc::new(Current::default()),
            facts: Arc::new(Mutex::new((0.0, None))),
            repaired: tokio::sync::Notify::new(),
            read_facts,
            recoveries,
            owed: Mutex::new(Default::default()),
            env_sidecars: {
                let embed = get("EMBEDDING_MODEL")
                    .filter(|v| !v.trim().is_empty())
                    .or_else(|| get("EMBED_MODEL"));
                [embed, get("EMBEDDING_NEXT_MODEL"), get("RERANK_MODEL")]
                    .into_iter()
                    .flatten()
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
                    .collect()
            },
            allow_below_q5: autotune_runner::kv::allow_below_q5(
                get(autotune_runner::kv::KV_FLOOR_OVERRIDE_ENV).as_deref(),
            ),
            memory_floor_gib: get("LLAMACPP_CALIBRATION_MEMORY_FLOOR_GIB")
                .map(|v| {
                    llamacpp_autoconfig::string_to_number(&v.encode_utf16().collect::<Vec<_>>())
                        .unwrap_or(f64::NAN)
                })
                .filter(|n| *n != 0.0 && !n.is_nan())
                .unwrap_or(2.0),
        }
    }

    /// Reads Core's facts again (before a start or a scan, and before each model's hold), within
    /// a few seconds. False when the sidecars' model names are still unknown (never read): a start
    /// then waits, since the scan must not offer a sidecar's model. A failed read keeps the last.
    pub async fn refresh(&self) -> bool {
        if let Ok((budget, sidecars)) = tokio::time::timeout(FACTS_LIMIT, (self.read_facts)()).await
        {
            if let Ok(mut f) = self.facts.lock() {
                f.0 = budget;
                if let Some(s) = sidecars {
                    f.1 = Some(s);
                }
            }
        }
        self.facts.lock().is_ok_and(|f| f.1.is_some())
    }

    /// Core's inference budget in GiB as last read (0: none).
    pub fn inference_budget(&self) -> f64 {
        self.facts.lock().map(|f| f.0).unwrap_or(0.0)
    }

    /// Before a repair's hold: this process's admission fence, if the run that owed the repair
    /// still had local admission when it was asked for.
    pub(crate) fn fence_owed(&self, key: &str) {
        if let Ok(mut owed) = self.owed.lock() {
            if let Some((_, fence @ None)) = owed.get_mut(key) {
                *fence = super::gate::AdmissionFence::reserve(Arc::clone(&self.maint.local));
            }
        }
    }

    /// A repair ran: its fences open again.
    pub(crate) fn settle_owed(&self, key: &str) {
        let entry = self.owed.lock().ok().and_then(|mut o| o.remove(key));
        if let Some((ticket, fence)) = entry {
            self.maint.authority.complete_recovery(&ticket);
            if let Some(fence) = fence {
                fence.release();
            }
        }
        self.repaired.notify_waiters();
    }

    /// Puts the runner's calls under `hold` (a recovery's fresh hold), or under none.
    pub fn cover_with(&self, hold: Option<Arc<Hold>>) {
        self.current.set(hold);
    }

    /// `maintenance.assertCovered(stepMs)`: the hold (if any) still excludes everyone and covers a
    /// step of `step`.
    fn covered_for(&self, step: Duration) -> Result<(), Fail> {
        covered(&self.current, step)
    }

    fn router(&self) -> Result<&super::router::Router, Fail> {
        self.maint
            .router
            .as_ref()
            .ok_or_else(|| Fail::new("The model server is not configured."))
    }

    /// The router's own request, carrying the hold's token, with Node's budget check before a
    /// load (llamacpp-manager.cjs `request`: `overBudget(model)` answers a 409).
    async fn send(&self, call: &Call, abort: Option<&Abort>) -> Result<JValue, Fail> {
        self.engine().send(call, abort).await
    }

    /// The private calibrator's view of the same engine, store and hold.
    pub(crate) fn engine(&self) -> FrontEngine {
        FrontEngine {
            maint: Arc::clone(&self.maint),
            current: Arc::clone(&self.current),
            facts: Arc::clone(&self.facts),
        }
    }

    /// long-profile.cjs `fileOf(row, presets)`: the row's `--model`, else its models.ini line.
    fn file_of(&self, row: &JValue) -> Option<String> {
        autotune_runner::model_path_from_args(row.get("status").get("args")).or_else(|| {
            let id = row.get("id").as_str()?;
            presets_of(&self.maint).ok()?.files(id).0
        })
    }
}

/// The engine, models.ini store and hold the runner works under, as the auto-tune's private
/// calibrator uses them (`autotune_context::Engine`, the manager's `createCalibrator` deps for a
/// private stage): the same covered request (`coveredRequest`: coverage for the whole step, the
/// load decision, the hold's token, the end of a call when the hold is lost).
#[derive(Clone)]
pub struct FrontEngine {
    maint: Arc<Maint>,
    current: Arc<Current>,
    facts: Arc<Mutex<(f64, Option<std::collections::HashSet<String>>)>>,
}

/// `assertCovered(step)` for the hold in `current`.
fn covered(current: &Current, step: Duration) -> Result<(), Fail> {
    match current.get() {
        Some(hold) if !hold.may_start(step) => Err(coverage_error()),
        _ => Ok(()),
    }
}

/// The live models.ini, as the auto-tune's preset store reads it.
fn presets_of(maint: &Maint) -> Result<crate::model_sizing::Presets, Fail> {
    let ini = maint
        .ini
        .as_ref()
        .ok_or_else(|| Fail::new("Native preset editing; configure LLAMACPP_PRESET_PATH"))?;
    ini.read().map_err(|t| Fail::new(t.message))
}

/// `presets.get(model)`.
fn presets_get_of(maint: &Maint, model: &str) -> JValue {
    let Ok(p) = presets_of(maint) else {
        return JValue::obj([
            ("model", JValue::from(model)),
            ("exists", JValue::Bool(false)),
        ]);
    };
    let opts = |name: &str| {
        p.span(name).map_or(JValue::Obj(Vec::new()), |(_, _, o)| {
            JValue::Obj(
                o.iter()
                    .map(|(k, v)| ((*k).to_string(), JValue::from(v.as_str())))
                    .collect(),
            )
        })
    };
    JValue::obj([
        ("model", JValue::from(model)),
        ("revision", JValue::from(p.revision.as_str())),
        ("exists", JValue::Bool(p.span(model).is_some())),
        ("options", opts(model)),
        ("defaults", opts("*")),
    ])
}

/// `presets.snapshot()`: `(text, revision)`.
fn snapshot_of(maint: &Maint) -> (String, String) {
    match maint.ini.as_ref().and_then(|ini| ini.snapshot().ok()) {
        Some(s) => (s.text, s.revision),
        None => (String::new(), String::new()),
    }
}

/// `presets.commit(candidate, { backup })` under the hold in `current`.
async fn commit_of(
    maint: &Maint,
    current: &Current,
    candidate: &JValue,
    backup: bool,
) -> Result<(), Fail> {
    let ini = maint
        .ini
        .clone()
        .ok_or_else(|| Fail::new("Native preset editing; configure LLAMACPP_PRESET_PATH"))?;
    covered(current, Duration::ZERO)?;
    let c = super::ini::Candidate {
        base_revision: candidate
            .get("baseRevision")
            .as_str()
            .unwrap_or("")
            .to_owned(),
        text: candidate.get("text").as_str().unwrap_or("").to_owned(),
        before: snapshot_of(maint).0,
    };
    ini.commit_reconciled(&c, backup).await.map_err(|e| Fail {
        status: Some(e.status),
        ..Fail::new(e.message)
    })
}

impl FrontEngine {
    async fn send(&self, call: &Call, abort: Option<&Abort>) -> Result<JValue, Fail> {
        // Row J: every router step first checks the lease covers the whole step (`coveredRequest`:
        // `assertCovered(timeout)`), and a step in flight ends when the lease is lost.
        covered(&self.current, Duration::from_millis(call.timeout_ms))?;
        if call.method == "POST" && call.path == "/models/load" {
            let model = call
                .body
                .as_ref()
                .map(|b| b.get("model").clone())
                .unwrap_or(JValue::Undefined);
            if let Some(model) = model.as_str() {
                use super::node_lease::LoadDecision;
                match self.maint.node.load_refusal(model).await {
                    LoadDecision::Proceed => {}
                    LoadDecision::Refused(body) => {
                        return Ok(JValue::obj([
                            ("ok", JValue::Bool(false)),
                            ("status", JValue::Num(409.0)),
                            ("body", to_j(&body)),
                        ]))
                    }
                    LoadDecision::Unavailable => {
                        return Ok(JValue::obj([
                            ("ok", JValue::Bool(false)),
                            ("status", JValue::Num(503.0)),
                            (
                                "body",
                                JValue::obj([(
                                    "error",
                                    JValue::from("The load could not be checked against the inference memory budget."),
                                )]),
                            ),
                        ]))
                    }
                }
            }
        }
        let router = self
            .maint
            .router
            .as_ref()
            .ok_or_else(|| Fail::new("The model server is not configured."))?;
        let method = if call.method == "POST" {
            reqwest::Method::POST
        } else {
            reqwest::Method::GET
        };
        let body = call.body.as_ref().map(to_v);
        let token = self.current.get().and_then(|h| h.token());
        let send = router.request(
            method,
            &call.path,
            body.as_ref(),
            token.as_deref(),
            Duration::from_millis(call.timeout_ms),
        );
        let hold = self.current.get();
        let lost = async {
            match &hold {
                Some(h) => loop {
                    if !h.excluded() {
                        return;
                    }
                    tokio::time::sleep(LOSS_POLL).await;
                },
                None => std::future::pending::<()>().await,
            }
        };
        let aborted = async {
            match abort {
                Some(a) => a.wait().await,
                None => std::future::pending::<()>().await,
            }
        };
        let reply = tokio::select! {
            biased;
            r = send => r,
            () = aborted => return Err(fetch_failed(abort)),
            () = lost => return Err(coverage_error()),
        };
        match reply {
            Ok(r) => Ok(reply_j(&r)),
            Err(_) => Err(fetch_failed(abort)),
        }
    }
}

/// A streamed engine reply (the long-prompt test): its chunks until it ends or the step aborts.
pub struct FrontBody {
    response: Option<reqwest::Response>,
    ok: bool,
    status: u16,
    abort: Abort,
}

impl super::autotune_context::Body for FrontBody {
    fn ok(&self) -> bool {
        self.ok
    }
    fn status(&self) -> f64 {
        f64::from(self.status)
    }
    fn has_body(&self) -> bool {
        self.response.is_some()
    }
    async fn read(&mut self) -> Result<Option<Vec<u8>>, Fail> {
        let Some(response) = self.response.as_mut() else {
            return Ok(None);
        };
        tokio::select! {
            biased;
            chunk = response.chunk() => match chunk {
                Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
                Ok(None) => Ok(None),
                Err(_) => Err(fetch_failed(Some(&self.abort))),
            },
            () = self.abort.wait() => Err(fetch_failed(Some(&self.abort))),
        }
    }
}

/// How long one streamed engine reply may run (llamacpp-calibration.cjs `limits.long`).
const STREAM_LIMIT: Duration = Duration::from_secs(7200);

impl super::autotune_context::Engine for FrontEngine {
    type Body = FrontBody;

    async fn raw_models(&self, abort: Option<Abort>) -> Result<JValue, Fail> {
        self.send(&Call::get("/models", 8000), abort.as_ref()).await
    }

    async fn request(&self, call: Call, abort: Option<Abort>) -> Result<JValue, Fail> {
        self.send(&call, abort.as_ref()).await
    }

    async fn stream(&self, path: &str, body: JValue, abort: Abort) -> Result<FrontBody, Fail> {
        covered(&self.current, Duration::ZERO)?;
        let router = self
            .maint
            .router
            .as_ref()
            .ok_or_else(|| Fail::new("The model server is not configured."))?;
        let token = self.current.get().and_then(|h| h.token());
        let body = to_v(&body);
        let sent = router.stream(path, &body, token.as_deref(), STREAM_LIMIT);
        let response = tokio::select! {
            biased;
            r = sent => r.map_err(|_| fetch_failed(Some(&abort)))?,
            () = abort.wait() => return Err(fetch_failed(Some(&abort))),
        };
        let status = response.status().as_u16();
        Ok(FrontBody {
            ok: response.status().is_success(),
            status,
            response: Some(response),
            abort,
        })
    }

    fn presets_get(&self, model: &str) -> JValue {
        presets_get_of(&self.maint, model)
    }

    fn presets_snapshot(&self) -> (String, String) {
        snapshot_of(&self.maint)
    }

    async fn presets_commit(&self, candidate: JValue) -> Result<(), Fail> {
        commit_of(&self.maint, &self.current, &candidate, true).await
    }

    async fn invalidate_baseline(&self, tag: &str) -> Result<bool, Fail> {
        Ok(self.maint.guard.invalidate(tag).saved)
    }

    async fn conservative_for(&self, model: &str) -> Result<JValue, Fail> {
        let inference = self.facts.lock().map(|f| f.0).unwrap_or(0.0);
        let sizing = Arc::clone(&self.maint.sizing);
        let listing = sizing.listing().await;
        let m = model.to_owned();
        Ok(
            tokio::task::spawn_blocking(move || sizing.conservative_for(&m, inference, &listing))
                .await
                .ok()
                .flatten()
                .unwrap_or(JValue::Null),
        )
    }

    async fn identity_for(&self, model: &str) -> Result<JValue, Fail> {
        Ok(super::calibration_facts::tune_identity(&self.maint, model)
            .await
            .unwrap_or(JValue::Null))
    }

    fn held(&self) -> bool {
        self.current.get().is_some_and(|h| h.excluded())
    }

    fn read_memory(&self) -> Option<f64> {
        super::calibration_worker::read_mem_available_gib()
    }

    fn now(&self) -> f64 {
        super::calibration_worker::now_ms()
    }

    async fn sleep(&self, ms: f64) {
        tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64)).await;
    }

    async fn tick(&self, ms: f64) {
        tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64)).await;
    }

    fn random_int(&self, min: u32, max: u32) -> u32 {
        let span = max.saturating_sub(min).max(1);
        let raw = u32::from_str_radix(&super::calibration_worker::random_hex(4), 16).unwrap_or(0);
        min + raw % span
    }

    fn random_uuid(&self) -> String {
        super::calibration_worker::uuid()
    }
}

/// The release handle `holdWhenIdle` gives the runner.
pub struct FrontRelease {
    hold: Mutex<Option<Arc<Hold>>>,
    current: Arc<Current>,
}

impl Release for FrontRelease {
    fn set_reason(&self, reason: &str) {
        if let Some(hold) = self.hold.lock().ok().and_then(|h| h.clone()) {
            let reason = reason.to_owned();
            tokio::spawn(async move { hold.set_reason(&reason).await });
        }
    }

    fn release(&self) {
        let taken = self.hold.lock().ok().and_then(|mut h| h.take());
        self.current.set(None);
        if let Some(hold) = taken {
            // The run holds the only other reference until it is told to let go; dropping the last
            // one gives the hold back (and its Drop does so if the task dies first).
            tokio::spawn(async move {
                if let Ok(hold) = Arc::try_unwrap(hold) {
                    hold.release().await;
                }
            });
        }
    }
}

impl Deps for FrontDeps {
    type Release = FrontRelease;
    type Context = PrivateCalibrator<FrontEngine, StageHooks<FrontDeps>>;

    async fn raw_models(&self) -> Result<JValue, Fail> {
        self.send(&Call::get("/models", 8000), None).await
    }

    async fn request(&self, call: Call, abort: Option<Abort>) -> Result<JValue, Fail> {
        self.send(&call, abort.as_ref()).await
    }

    fn presets_get(&self, model: &str) -> JValue {
        presets_get_of(&self.maint, model)
    }

    fn presets_snapshot(&self) -> (String, String) {
        snapshot_of(&self.maint)
    }

    async fn presets_commit(&self, candidate: JValue, options: JValue) -> Result<(), Fail> {
        // `backup: false` only when asked (auto-tune's later writes); the writer's default otherwise.
        let backup = options.get("backup") != &JValue::Bool(false);
        commit_of(&self.maint, &self.current, &candidate, backup).await
    }

    async fn apply_unlocked(&self, body: JValue, write_options: JValue) -> Result<JValue, Fail> {
        let Some(hold) = self.current.get() else {
            return Err(Fail::new("No maintenance hold covers this write."));
        };
        let model = body.get("model").as_str().unwrap_or("").to_owned();
        let base = body.get("baseRevision").as_str().unwrap_or("").to_owned();
        let options: Vec<(String, Option<String>)> = js_entries(body.get("options"))
            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned)))
            .collect();
        let backup = write_options.get("backup") != &JValue::Bool(false);
        // applyUnlocked: `maintenance.assertCovered(PRESET_STEP_MS)` before the write and its reload.
        self.covered_for(PRESET_STEP)?;
        let (status, error) = self
            .maint
            .apply_unlocked_reply(&hold, &model, &base, &options, backup)
            .await;
        // A lease lost under the write: nothing more runs on it (the phase's restore becomes the
        // recovery, as Node's loss branch makes it).
        if !hold.excluded() {
            return Err(coverage_error());
        }
        let mut out = JValue::obj([
            ("ok", JValue::Bool(status == 200)),
            ("status", JValue::Num(f64::from(status))),
        ]);
        let reply = match error {
            Some(e) => JValue::obj([("error", JValue::from(e))]),
            None => self.presets_get(&model),
        };
        if let JValue::Obj(items) = &mut out {
            wtf16::set(items, "body", reply);
        }
        Ok(out)
    }

    fn revision_of(&self, text: &str) -> String {
        super::ini::sha(text)
    }

    async fn identity_for(&self, model: &str) -> Result<Option<JValue>, Fail> {
        Ok(super::calibration_facts::tune_identity(&self.maint, model).await)
    }

    async fn plan_facts(&self, model: &str) -> Result<JValue, Fail> {
        Ok(self
            .maint
            .sizing
            .plan_facts(model)
            .await
            .unwrap_or(JValue::Null))
    }

    async fn sampling_for(&self, model: &str) -> Result<JValue, Fail> {
        Ok(self.maint.sampling_for(model).await)
    }

    fn file_missing(&self, row: &JValue) -> bool {
        // #545: a preset row whose GGUF is not on the models mount (unknown never counts).
        if row.get("source").as_str() != Some("preset") || !self.maint.sizing.has_models_mount() {
            return false;
        }
        match self.file_of(row) {
            Some(file) if file.starts_with("/models/") => {
                self.maint.sizing.local_file_path(&file).is_none()
            }
            _ => false,
        }
    }

    fn is_system_model(&self, id: &str, path: Option<&str>) -> bool {
        let roles = crate::model_kind::roles_beside(self.maint.ini.as_ref().map(|i| i.file()));
        crate::model_kind::is_system_model_at(&roles, id, path)
    }

    fn is_sidecar_model(&self, id: &str) -> bool {
        let known = self.facts.lock().ok().and_then(|f| f.1.clone());
        match known {
            Some(names) => names.contains(id.trim()),
            // Core not read yet: the names Node starts from (model-system.cjs sidecarNames), taken
            // conservatively (RERANK_MODEL whether or not the reranker is on).
            None => self.env_sidecars.iter().any(|n| n == id.trim()),
        }
    }

    fn quirks(&self, model: &str) -> Quirks {
        let mut q = Quirks::default();
        if let Some(f) = super::evidence::family_of(model) {
            for (k, v) in f.quirks {
                match (*k, v) {
                    ("harmony", super::evidence::Quirk::Bool(b)) => q.harmony = *b,
                    ("reasoningEffort", super::evidence::Quirk::Str(s)) => {
                        q.reasoning_effort = Some((*s).to_owned())
                    }
                    ("qualityBudget", super::evidence::Quirk::Num(n))
                        if n.is_finite() && *n >= 0.0 =>
                    {
                        q.quality_budget = Some(*n as u32);
                    }
                    _ => {}
                }
            }
        }
        q
    }

    fn read_memory(&self) -> Option<f64> {
        super::calibration_worker::read_mem_available_gib()
    }

    fn budget_gib(&self) -> Result<f64, Fail> {
        let inference = self.inference_budget();
        let sized = self.maint.sizing.calibration_budget_gib(inference);
        if sized > 0.0 {
            return Ok(sized);
        }
        if let Some(fault) = self.maint.sizing.budget_fault() {
            return Err(Fail {
                code: Some("autoconfig_impl".into()),
                status: Some(503),
                ..Fail::new(fault)
            });
        }
        // inference-budget.cjs DEFAULT_BUDGET_GIB.
        Ok(16.0)
    }

    fn ladder(&self, native: u64) -> Result<Vec<u64>, Fail> {
        Ok(super::calibration::ladder(native as f64)
            .into_iter()
            .map(|c| c as u64)
            .collect())
    }

    fn cache_ram_limits(&self) -> (f64, f64) {
        self.maint.sizing.cache_ram_limits()
    }

    fn allow_below_q5(&self) -> bool {
        self.allow_below_q5
    }

    async fn hold_when_idle(
        &self,
        reason: &str,
        timeout_ms: u64,
        abort: Abort,
    ) -> Result<FrontRelease, Fail> {
        // Core's facts are read once per model, before its planning (Node reads them in memory).
        tokio::select! {
            _ = self.refresh() => {}
            () = abort.wait() => return Err(Fail::cancelled()),
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if abort.aborted() {
                return Err(Fail::cancelled());
            }
            match self.maint.acquire_for_run(reason, RUN_LEASE).await {
                Ok(hold) => {
                    let hold = Arc::new(hold);
                    self.current.set(Some(Arc::clone(&hold)));
                    return Ok(FrontRelease {
                        hold: Mutex::new(Some(hold)),
                        current: Arc::clone(&self.current),
                    });
                }
                // inference-maintenance.cjs remoteBusy: 409, engine_busy or maintenance_held is
                // busy (wait again); any other gate refusal is thrown at once.
                Err(super::HoldRefusal::Busy) => {}
                Err(super::HoldRefusal::Gate { status, code, .. })
                    if status == 409 || code == "engine_busy" || code == "maintenance_held" => {}
                Err(super::HoldRefusal::Gate {
                    status, message, ..
                }) => {
                    return Err(Fail {
                        status: Some(status),
                        ..Fail::new(message)
                    })
                }
                Err(_) => return Err(Fail::new("The maintenance hold is unavailable.")),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Fail {
                    idle_timeout: true,
                    ..Fail::new("Timed out waiting for chat to become idle.")
                });
            }
            tokio::select! {
                () = tokio::time::sleep(IDLE_POLL) => {}
                () = abort.wait() => return Err(Fail::cancelled()),
            }
        }
    }

    fn assert_covered(&self) -> Result<(), Fail> {
        self.covered_for(Duration::ZERO)
    }

    fn coverage_lost(&self) -> bool {
        self.current.get().is_some_and(|h| !h.excluded())
    }

    fn recoverable(&self) -> bool {
        true
    }

    fn require_recovery(&self, key: &str, _message: &str) {
        let Ok(mut owed) = self.owed.lock() else {
            return;
        };
        if owed.contains_key(key) {
            return;
        }
        // Closed at once, before the repair runs: the authority's recovery flag (no new lease) and
        // this process's own admission (taken again by the repair if the run still had it).
        let ticket = self.maint.authority.require_recovery();
        let fence = super::gate::AdmissionFence::reserve(Arc::clone(&self.maint.local));
        owed.insert(key.to_owned(), (ticket, fence));
        drop(owed);
        let _ = self.recoveries.send(key.to_owned());
    }

    /// `maintenance.recoverNow()`: the owed repairs run now (by the repair worker); this waits for
    /// them, at most [`RECOVER_NOW_LIMIT`], so `recover()` reports whether the restore happened.
    async fn recover_now(&self) {
        let deadline = tokio::time::Instant::now() + RECOVER_NOW_LIMIT;
        loop {
            let notified = self.repaired.notified();
            if self.owed.lock().map_or(true, |o| o.is_empty()) {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    async fn invalidate_baseline(&self, tag: &str) -> bool {
        self.maint.guard.invalidate(tag).saved
    }

    fn reload_router(&self) -> Option<impl Future<Output = Result<JValue, Fail>> + Send> {
        Some(async move {
            let router = self.router()?;
            let Some(hold) = self.current.get() else {
                return Err(Fail::new("No maintenance hold covers this reload."));
            };
            match self.maint.router_reload_reply(router, &hold).await {
                Some(r) => Ok(reply_j(&r)),
                None => Err(Fail::new("fetch failed")),
            }
        })
    }

    fn context(&self, hooks: StageHooks<Self>, fill_only: bool) -> Self::Context {
        PrivateCalibrator::new(
            self.engine(),
            hooks,
            fill_only,
            self.memory_floor_gib,
            Limits::default(),
        )
    }

    async fn on_result(&self, model: &str, result: &JValue) -> Result<(), Fail> {
        // llamacpp-manager.cjs onResult: context capacity, throughput and (when drafting) MTP
        // acceptance, each filed under the live configuration.
        let slots = {
            let n = self
                .presets_get(model)
                .get("options")
                .get("parallel")
                .as_str()
                .map(autotune_runner::js::number_of_str)
                .unwrap_or(f64::NAN);
            if n.is_nan() || n == 0.0 {
                1.0
            } else {
                n
            }
        };
        let mut limitations = vec![
            JValue::from(
                "Three deterministic quality smoke probes, not a general quality benchmark",
            ),
            JValue::from("120 s default prompt budget; existing MTP head only"),
        ];
        let skipped: Vec<String> =
            autotune_runner::js::items(result.get("baseline").get("skipped"))
                .iter()
                .map(|s| js_json::to_js_string(s.get("id")).unwrap_or_default())
                .collect();
        if !skipped.is_empty() {
            limitations.push(JValue::from(format!(
                "Probes not used (failed at the model's reference settings): {}",
                skipped.join(", ")
            )));
        }
        let mut records = vec![
            (
                "context_capacity",
                JValue::obj([
                    ("ctx", result.get("context").clone()),
                    ("appliedCtx", result.get("context").clone()),
                    ("slots", JValue::Num(slots)),
                ]),
            ),
            (
                "throughput",
                JValue::obj([("rate", result.get("generation").clone())]),
            ),
        ];
        if !result.get("acceptance").is_nullish() {
            let rate = autotune_runner::js::number(result.get("acceptance")) / 100.0;
            records.push(("mtp_acceptance", JValue::obj([("rate", JValue::Num(rate))])));
        }
        for (category, value) in records {
            let record = JValue::obj([
                ("category", JValue::from(category)),
                ("result", JValue::from("passed")),
                ("value", value),
                (
                    "suite",
                    JValue::obj([
                        ("name", JValue::from("full-autotune")),
                        ("version", JValue::Num(3.0)),
                    ]),
                ),
                ("source", JValue::from("autotune")),
                ("limitations", JValue::Arr(limitations.clone())),
            ]);
            self.maint
                .record_evidence(model, &record)
                .await
                .map_err(|_| Fail::new("Evidence could not be recorded."))?;
        }
        Ok(())
    }

    async fn judge(&self, request: JValue) -> Result<JValue, Fail> {
        // load-advisor.cjs judge: Rust's rules (load-verdict). No decision service is asked from
        // this front yet, so a rule tie keeps the rule's verdict (`advisor: unavailable`).
        Ok(autotune_runner::decide::judge(&request))
    }

    fn plan(&self, request: &JValue) -> Result<JValue, Fail> {
        autotune_runner::decide::plan(request)
    }

    fn contention(&self, request: &JValue) -> Result<JValue, Fail> {
        autotune_runner::decide::contention(request)
    }

    fn template_caps(&self, template: &str) -> Result<JValue, Fail> {
        autotune_runner::decide::template_caps(template)
    }

    fn serving_verdict(&self, status: f64, body_text: &str) -> Result<JValue, Fail> {
        autotune_runner::decide::serving_verdict(status, body_text)
    }

    fn scan(&self, request: JValue) -> Result<JValue, Fail> {
        let (code, reply) =
            autotune_plan::scan::run_json(&wtf16::stringify(&request).unwrap_or_default());
        if code != 0 {
            return Err(Fail::new("Auto-tune scan is unavailable."));
        }
        wtf16::parse(&reply).map_err(|_| Fail::new("Auto-tune scan is unavailable."))
    }

    fn long_pairs(&self, rows: &JValue) -> JValue {
        autotune_runner::decide::long_pairs(rows, |row| self.file_of(row))
    }

    fn long_section(&self, text: &str, row: &JValue) -> JValue {
        let mmproj =
            autotune_runner::decide::arg_of(row.get("status").get("args"), &["--mmproj", "-mm"])
                .or_else(|| {
                    let id = row.get("id").as_str()?;
                    presets_of(&self.maint).ok()?.files(id).1
                });
        autotune_runner::decide::long_section(text, row.get("id"), self.file_of(row), mmproj)
    }

    fn now(&self) -> f64 {
        super::calibration_worker::now_ms()
    }

    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    async fn tick(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    fn spawn(&self, _kind: Spawn, task: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(task);
    }

    fn random_uuid(&self) -> String {
        super::calibration_worker::uuid()
    }

    fn save(&self, text: &str) -> Result<(), Fail> {
        write_atomic(&self.state_file, text).map_err(|e| Fail::new(e.to_string()))
    }

    fn log(&self, message: &str) {
        eprintln!("{message}");
    }
}

fn js_entries(v: &JValue) -> impl Iterator<Item = &(String, JValue)> {
    autotune_runner::js::entries(v).into_iter().flatten()
}

/// `save()`: a temp file beside the state file, mode 0600, then a rename over it.
fn write_atomic(file: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = file.with_extension(format!("json.{}", super::calibration_worker::uuid()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
