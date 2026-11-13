//! The native auto-tune runner's dependencies (`autotune_runner::Deps`) over this front's model
//! maintenance (sbstndalton/noevia#2516): what core llamacpp-manager.cjs hands
//! `createFullAutotuner` (lines 730-790), served by the router client, the models.ini store, the
//! maintenance hold, the reload guard, the evidence store and the Rust decisions Node reached
//! through dav-parse.wasm. Built only when `AUTOTUNE_RUNNER_IMPL=rust`; nothing here runs otherwise.

use super::{Hold, Maint};
use autotune_runner::{Abort, Call, Context, Deps, Fail, Quirks, Release, Spawn, StageHooks};
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

/// What the runner cannot use until the private calibrator is native (step C1 of #2516).
const CONTEXT_UNAVAILABLE: &str = "The context worker state changed or is unavailable.";
/// The poll while chat is busy (`holdWhenIdle` waits for an idle engine).
const IDLE_POLL: Duration = Duration::from_millis(500);
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
    facts: Arc<Mutex<(f64, std::collections::HashSet<String>)>>,
    /// Reads those facts from Core.
    read_facts: ReadFacts,
    /// The repairs `requireRecovery` asks for, run by the wiring under a fresh hold.
    recoveries: tokio::sync::mpsc::UnboundedSender<String>,
    allow_below_q5: bool,
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
            facts: Arc::new(Mutex::new((0.0, Default::default()))),
            read_facts,
            recoveries,
            allow_below_q5: autotune_runner::kv::allow_below_q5(
                get(autotune_runner::kv::KV_FLOOR_OVERRIDE_ENV).as_deref(),
            ),
        }
    }

    /// Reads Core's facts again (before a start or a scan, and before each model's hold).
    pub async fn refresh(&self) {
        let (budget, sidecars) = (self.read_facts)().await;
        if let Ok(mut f) = self.facts.lock() {
            f.0 = budget;
            if let Some(s) = sidecars {
                f.1 = s;
            }
        }
    }

    /// The hold the runner is under, if any.
    pub fn hold(&self) -> Option<Arc<Hold>> {
        self.current.get()
    }

    /// Puts the runner's calls under `hold` (a recovery's fresh hold), or under none.
    pub fn cover_with(&self, hold: Option<Arc<Hold>>) {
        self.current.set(hold);
    }

    fn token(&self) -> Option<String> {
        self.current.get().and_then(|h| h.token())
    }

    fn router(&self) -> Result<&super::router::Router, Fail> {
        self.maint
            .router
            .as_ref()
            .ok_or_else(|| Fail::new("The model server is not configured."))
    }

    fn presets(&self) -> Result<crate::model_sizing::Presets, Fail> {
        let ini =
            self.maint.ini.as_ref().ok_or_else(|| {
                Fail::new("Native preset editing; configure LLAMACPP_PRESET_PATH")
            })?;
        ini.read().map_err(|t| Fail::new(t.message))
    }

    /// The router's own request, carrying the hold's token, with Node's budget check before a
    /// load (llamacpp-manager.cjs `request`: `overBudget(model)` answers a 409).
    async fn send(&self, call: &Call, abort: Option<&Abort>) -> Result<JValue, Fail> {
        // Row J: every router step first checks the lease still covers it (`coveredRequest`).
        self.assert_covered()?;
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
        let router = self.router()?;
        let method = if call.method == "POST" {
            reqwest::Method::POST
        } else {
            reqwest::Method::GET
        };
        let body = call.body.as_ref().map(to_v);
        let token = self.token();
        let send = router.request(
            method,
            &call.path,
            body.as_ref(),
            token.as_deref(),
            Duration::from_millis(call.timeout_ms),
        );
        let reply = match abort {
            Some(abort) => {
                tokio::select! {
                    biased;
                    r = send => r,
                    () = abort.wait() => return Err(fetch_failed(Some(abort))),
                }
            }
            None => send.await,
        };
        match reply {
            Ok(r) => Ok(reply_j(&r)),
            Err(_) => Err(fetch_failed(abort)),
        }
    }

    /// long-profile.cjs `fileOf(row, presets)`: the row's `--model`, else its models.ini line.
    fn file_of(&self, row: &JValue) -> Option<String> {
        autotune_runner::model_path_from_args(row.get("status").get("args")).or_else(|| {
            let id = row.get("id").as_str()?;
            self.presets().ok()?.files(id).0
        })
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

/// Until the private calibrator is native (step C1): every context stage and fill probe is
/// refused, as Node's worker answers when its state changed.
pub struct NoContext;

impl Context for NoContext {
    async fn start(&self, _model: &str, _budget: f64) -> Result<JValue, Fail> {
        Ok(JValue::obj([
            ("ok", JValue::Bool(false)),
            (
                "body",
                JValue::obj([("error", JValue::from(CONTEXT_UNAVAILABLE))]),
            ),
        ]))
    }
    async fn completion(&self) -> Result<(), Fail> {
        Ok(())
    }
    fn status_job(&self, _model: &str) -> JValue {
        JValue::Null
    }
    fn abort(&self) {}
    async fn probe(&self, _model: &str, _opts: JValue) -> Result<JValue, Fail> {
        Err(Fail::new(CONTEXT_UNAVAILABLE))
    }
    fn cancel_probe(&self) {}
}

/// A decision crate's reply, or the module's refusal as Node's DavParseError.
fn decided(code: u32, reply: &str, what: &str) -> Result<JValue, Fail> {
    if code != 0 {
        return Err(Fail::new(format!("{what} is unavailable")));
    }
    wtf16::parse(reply).map_err(|_| Fail::new(format!("{what} reply has an unexpected shape")))
}

/// dav-parse-wasm.cjs: text sent to a module is made well formed first (lone surrogates as U+FFFD).
fn well_formed(v: &JValue) -> String {
    let text = wtf16::stringify(v).unwrap_or_default();
    text.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

impl Deps for FrontDeps {
    type Release = FrontRelease;
    type Context = NoContext;

    async fn raw_models(&self) -> Result<JValue, Fail> {
        self.send(&Call::get("/models", 8000), None).await
    }

    async fn request(&self, call: Call, abort: Option<Abort>) -> Result<JValue, Fail> {
        self.send(&call, abort.as_ref()).await
    }

    fn presets_get(&self, model: &str) -> JValue {
        let Ok(p) = self.presets() else {
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

    fn presets_snapshot(&self) -> (String, String) {
        match self.maint.ini.as_ref().and_then(|ini| ini.snapshot().ok()) {
            Some(s) => (s.text, s.revision),
            None => (String::new(), String::new()),
        }
    }

    async fn presets_commit(&self, candidate: JValue, options: JValue) -> Result<(), Fail> {
        let ini =
            self.maint.ini.clone().ok_or_else(|| {
                Fail::new("Native preset editing; configure LLAMACPP_PRESET_PATH")
            })?;
        self.assert_covered()?;
        let text = candidate.get("text").as_str().unwrap_or("").to_owned();
        let base = candidate
            .get("baseRevision")
            .as_str()
            .unwrap_or("")
            .to_owned();
        let before = self.presets_snapshot().0;
        let c = super::ini::Candidate {
            base_revision: base,
            text,
            before,
        };
        // `backup: false` only when asked (auto-tune's later writes); the writer's default otherwise.
        let backup = options.get("backup") != &JValue::Bool(false);
        ini.commit_reconciled(&c, backup).await.map_err(|e| Fail {
            status: Some(e.status),
            ..Fail::new(e.message)
        })
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
        let (status, error) = self
            .maint
            .apply_unlocked_reply(&hold, &model, &base, &options, backup)
            .await;
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
        self.facts.lock().is_ok_and(|f| f.1.contains(id.trim()))
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
        let inference = self.facts.lock().map(|f| f.0).unwrap_or(0.0);
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
        self.refresh().await;
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
                Err(super::HoldRefusal::Busy) | Err(super::HoldRefusal::Gate { .. }) => {}
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
        match self.current.get() {
            Some(hold) if !hold.may_start(Duration::ZERO) => Err(Fail {
                coverage_lost: true,
                status: Some(503),
                code: Some("maintenanceCoverageLost".into()),
                ..Fail::new("The maintenance lease was ended by the lease authority, so this work stopped before changing anything else.")
            }),
            _ => Ok(()),
        }
    }

    fn coverage_lost(&self) -> bool {
        self.current.get().is_some_and(|h| !h.excluded())
    }

    fn recoverable(&self) -> bool {
        true
    }

    fn require_recovery(&self, key: &str, _message: &str) {
        let _ = self.recoveries.send(key.to_owned());
    }

    async fn recover_now(&self) {}

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

    fn context(&self, _hooks: StageHooks<Self>, _fill_only: bool) -> NoContext {
        NoContext
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
        // this front yet, so a rule tie stays the step's own cause (`advisor: unavailable`).
        let cause = request
            .get("cause")
            .as_str()
            .filter(|c| matches!(*c, "oom" | "load" | "timeout" | "time" | "recall"))
            .unwrap_or("load");
        let ev = request.get("evidence");
        let int = |v: &JValue, lo: f64, hi: f64| match v.as_num() {
            Some(n) if n.fract() == 0.0 && n >= lo && n <= hi => JValue::Num(n),
            _ => JValue::Null,
        };
        let evidence = if ev.is_object() {
            let text = ev.get("text").as_str().unwrap_or("");
            let flat: String = text
                .chars()
                .map(|c| {
                    if matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}') {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            JValue::obj([
                ("status", int(ev.get("status"), 0.0, 999.0)),
                ("exitCode", int(ev.get("exitCode"), -1024.0, 1024.0)),
                ("text", JValue::from(wtf16::slice(&flat, 2048))),
                (
                    "crash",
                    JValue::Bool(ev.get("crash") == &JValue::Bool(true)),
                ),
            ])
        } else {
            JValue::Null
        };
        let ask = JValue::obj([("cause", JValue::from(cause)), ("evidence", evidence)]);
        let (code, reply) = load_verdict::verdict_json(&well_formed(&ask));
        let Ok(v) = decided(code, &reply, "load verdict") else {
            return Ok(JValue::Null);
        };
        let advisor = if v.get("ask").truthy() {
            "unavailable"
        } else {
            "not_asked"
        };
        Ok(JValue::obj([
            ("outcome", v.get("outcome").clone()),
            ("reason", v.get("reason").clone()),
            (
                "classification",
                JValue::obj([
                    ("source", v.get("source").clone()),
                    ("rule", v.get("rule").clone()),
                    ("ruleId", v.get("ruleId").clone()),
                    ("advisor", JValue::from(advisor)),
                ]),
            ),
        ]))
    }

    fn plan(&self, request: &JValue) -> Result<JValue, Fail> {
        let (code, reply) = autotune_plan::plan_json(&well_formed(request));
        decided(code, &reply, "autotune plan")
    }

    fn contention(&self, request: &JValue) -> Result<JValue, Fail> {
        let (code, reply) = tune_contention::decide_json(&well_formed(request));
        decided(code, &reply, "tune contention")
    }

    fn template_caps(&self, template: &str) -> Result<JValue, Fail> {
        let reply = chat_template_caps::reply_json(&chat_template_caps::analyze(template));
        decided(0, &reply, "template caps")
    }

    fn serving_verdict(&self, status: f64, body_text: &str) -> Result<JValue, Fail> {
        if !(status.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&status)) {
            return Err(Fail::new("provider error input has the wrong type"));
        }
        let v = chat_template_caps::serving_verdict(status as u32, body_text);
        decided(0, &chat_template_caps::verdict_json(&v), "serving verdict")
    }

    fn scan(&self, request: JValue) -> Result<JValue, Fail> {
        let (code, reply) =
            autotune_plan::scan::run_json(&wtf16::stringify(&request).unwrap_or_default());
        decided(code, &reply, "Auto-tune scan")
    }

    fn long_pairs(&self, rows: &JValue) -> JValue {
        let mut seen = std::collections::HashSet::new();
        let mut input = Vec::new();
        for row in autotune_runner::js::items(rows) {
            let Some(id) = row.get("id").as_str().filter(|i| !i.is_empty()) else {
                continue;
            };
            if seen.contains(id) || id.len() > 512 || input.len() >= 512 {
                continue;
            }
            seen.insert(id.to_owned());
            let model = self.file_of(row).filter(|m| m.len() <= 4096);
            input.push(JValue::obj([
                ("id", JValue::from(id)),
                ("model", model.map_or(JValue::Null, JValue::from)),
            ]));
        }
        let ask = JValue::obj([("op", JValue::from("pairs")), ("rows", JValue::Arr(input))]);
        let (code, reply) = long_profile::run_json(&well_formed(&ask));
        decided(code, &reply, "long profile")
            .map(|r| r.get("pairs").clone())
            .ok()
            .filter(|p| matches!(p, JValue::Arr(_)))
            .unwrap_or(JValue::Arr(Vec::new()))
    }

    fn long_section(&self, text: &str, row: &JValue) -> JValue {
        let model = self.file_of(row);
        let args = row.get("status").get("args");
        let mmproj = autotune_runner::js::items(args)
            .iter()
            .position(|a| matches!(a.as_str(), Some("--mmproj" | "-mm")))
            .and_then(|i| autotune_runner::js::items(args).get(i + 1))
            .and_then(JValue::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let id = row.get("id").as_str()?;
                self.presets().ok()?.files(id).1
            });
        let ask = JValue::obj([
            ("op", JValue::from("section")),
            ("text", JValue::from(text)),
            ("base", row.get("id").clone()),
            ("model", model.map_or(JValue::Null, JValue::from)),
            ("mmproj", mmproj.map_or(JValue::Null, JValue::from)),
        ]);
        let (code, reply) = long_profile::run_json(&well_formed(&ask));
        match decided(code, &reply, "long profile") {
            Ok(r) if r.get("ok") == &JValue::Bool(true) => JValue::obj([
                ("ok", JValue::Bool(true)),
                ("id", r.get("id").clone()),
                ("text", r.get("text").clone()),
            ]),
            Ok(r) => JValue::obj([
                ("ok", JValue::Bool(false)),
                ("reason", r.get("reason").clone()),
            ]),
            Err(_) => JValue::obj([
                ("ok", JValue::Bool(false)),
                ("reason", JValue::from("unavailable")),
            ]),
        }
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
