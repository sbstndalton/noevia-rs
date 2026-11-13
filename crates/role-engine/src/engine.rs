//! The async layer of role-engine.cjs: `createRoleEngine` ([`RoleEngine`]), `pinModel` /
//! `pinOnMachine` ([`RoleEngine::pin_model`]) and `makeSession(...).call` ([`Session::call`]),
//! with plan-constrained-decoding.cjs `requestPlanArtifact` (one unconstrained retry on 400, 422
//! or 501) and stream-guard.cjs `runGuardedStream` (abort, one bounded correction) inlined.
//!
//! Everything outside is a [`Deps`] method: the engine endpoint, the transport (one POST or GET,
//! its body read chunk by chunk), the keep-list, the optional injected loaded-model list,
//! admission and budget refusal, the Code role's machines, the log sink and the clock. Nothing
//! here opens a socket or reads the wall clock, so tests run on scripted stubs.
//!
//! Cancellation: a call takes a [`Cancel`] (Node's `signal`). It is checked where Node checks
//! `signal.aborted`, and every transport await races it, as an aborted `fetch` rejects. A
//! blocking caller on a std thread (the Code executor with code-acp's `Cancel`) drives a call
//! with [`block_on`] and links its cancel with
//! `acp_cancel.on_cancel(Box::new(move || token.cancel()))`.
//!
//! `call` never fails other than with a [`Failure`] (`{ ok: false, code, reason }`); no model
//! text, engine message or prompt text is ever logged.

use crate::{
    build_messages, from_units, join_pairs, js_string, loaded_from_model_list, plain_json,
    prefix_hash, role_default, schema_constraint, stringify, units, Constraint, Meter, RoleBudget,
    SseDecoder, SseItem, DOSSIER_ROLES,
};
use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use stream_guard::{Schema, State, Validator, DEFAULT_MAX_DEPTH, MAX_GUARD_BYTES};
use tokio::sync::watch;

const ROUTER_TIMEOUT_MS: u64 = 8000;
const ROUTER_CAP: usize = 2 * 1024 * 1024;
const MACHINE_UNREACHABLE: &str = "The Code role\u{2019}s machine is unreachable.";

/// A cancel (Node's `AbortSignal`); clones share it, and a cancel is permanent.
#[derive(Clone, Debug)]
pub struct Cancel {
    me: Arc<watch::Sender<bool>>,
    parent: Option<Arc<watch::Sender<bool>>>,
}

impl Default for Cancel {
    fn default() -> Self {
        Cancel {
            me: Arc::new(watch::channel(false).0),
            parent: None,
        }
    }
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }
    pub fn cancel(&self) {
        self.me.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.me.borrow() || self.parent.as_ref().is_some_and(|p| *p.borrow())
    }
    /// Resolves once cancelled (at once when it already is).
    pub async fn cancelled(&self) {
        let mut a = self.me.subscribe();
        match &self.parent {
            None => {
                let _ = a.wait_for(|c| *c).await;
            }
            Some(p) => {
                let mut b = p.subscribe();
                tokio::select! {
                    _ = a.wait_for(|c| *c) => {}
                    _ = b.wait_for(|c| *c) => {}
                }
            }
        }
    }
    /// A per-attempt cancel: cancelled with this one, or on its own (the first violation).
    fn child(&self) -> Cancel {
        Cancel {
            me: Arc::new(watch::channel(false).0),
            parent: Some(self.me.clone()),
        }
    }
}

/// Runs a future to completion on this (std) thread with a current-thread runtime. Refuses
/// inside a runtime (where tokio cannot block) instead of panicking.
pub fn block_on<F: Future>(f: F) -> std::io::Result<F::Output> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(std::io::Error::other("block_on inside an async runtime"));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(rt.block_on(f))
}

/// One HTTP request: a POST to `/chat/completions` (body = the payload's JSON text) or a GET of
/// the router's `/models` (`timeout_ms` = 8000). Redirects must be refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub url: String,
    pub method: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// A transport failure (no answer at the socket, a broken body, an abort).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransportError {
    pub detail: String,
}

/// A response body, read chunk by chunk (`Ok(None)`: the end).
pub trait Body: Send {
    fn read(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, TransportError>> + Send;
    /// Stops the body: no further byte is pulled from the engine.
    fn cancel(&mut self);
}

pub struct Response<B> {
    pub status: u16,
    pub body: B,
}

/// What `engine()` answers. `machine` is the Code role's machine target (#1853) as Node holds
/// it (`{ machine: { id, name }, baseUrl, model }`).
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub provider: JValue,
    pub external: bool,
    pub machine: Option<JValue>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Endpoint {
            base_url: None,
            api_key: None,
            model: None,
            provider: JValue::Null,
            external: false,
            machine: None,
        }
    }
}

/// `machines.ready(target)`'s answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MachineReady {
    pub ok: bool,
    pub code: Option<String>,
    pub error: Option<String>,
}

/// A truthy `budgetRefusal(model)` answer; `error` is its public text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetRefusal {
    pub error: Option<String>,
}

/// An admission that refused or failed (`status` as the thrown error's).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdmissionError {
    pub status: Option<i64>,
}

pub type Work<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything the engine uses from outside (`createRoleEngine`'s options).
pub trait Deps: Send + Sync {
    type Body: Body;
    /// `engine()`; `None` when it throws or answers nothing.
    fn engine(&self) -> Option<Endpoint>;
    /// `fetch(url, init)` to this machine's engine.
    fn fetch(
        &self,
        req: Request,
        cancel: Cancel,
    ) -> impl Future<Output = Result<Response<Self::Body>, TransportError>> + Send;
    /// `keep()`; `None` when it is not a list (the call is refused as 503).
    fn keep(&self) -> impl Future<Output = Option<Vec<String>>> + Send;
    /// `loadedModels()`, when injected: `Some(Ok(ids))` (not an array: refused), `Some(Err)` when
    /// it throws; `None` reads the router's listing.
    fn loaded_models(&self) -> impl Future<Output = Option<Result<JValue, ()>>> + Send {
        async { None }
    }
    /// The explicit roles of models.roles.json (model-system.cjs), as [`crate::section_roles_of`].
    fn system_roles(&self) -> HashMap<String, String> {
        HashMap::new()
    }
    /// `admission(work, signal)`; the default runs the work (production has none).
    fn admit<'a, T: Send + 'a>(
        &'a self,
        work: Work<'a, T>,
        cancel: Option<Cancel>,
    ) -> impl Future<Output = Result<T, AdmissionError>> + Send + 'a {
        async move {
            drop(cancel);
            Ok(work.await)
        }
    }
    /// `budgetRefusal(model)`; `None` also when it throws.
    fn budget_refusal(&self, model: &str) -> impl Future<Output = Option<BudgetRefusal>> + Send {
        let _ = model;
        async { None }
    }
    /// Whether `machines` (with `ready` and `fetch`) is given.
    fn has_machines(&self) -> bool {
        false
    }
    /// `machines.ready(target)`; `Err` when it throws.
    fn machine_ready(
        &self,
        target: &JValue,
    ) -> impl Future<Output = Result<MachineReady, ()>> + Send {
        let _ = target;
        async { Err(()) }
    }
    /// `machines.fetch(url, init)` (no API key is ever sent there).
    fn machine_fetch(
        &self,
        req: Request,
        cancel: Cancel,
    ) -> impl Future<Output = Result<Response<Self::Body>, TransportError>> + Send {
        drop((req, cancel));
        async { Err(TransportError::default()) }
    }
    /// `machines.connectError(target, error)`; `None` when there is no such function.
    fn machine_connect_error(&self, target: &JValue, error: &TransportError) -> Option<String> {
        let _ = (target, error);
        None
    }
    /// `log(entry)`.
    fn log(&self, entry: JValue) {
        drop(entry);
    }
    /// `now()` in milliseconds.
    fn now(&self) -> f64;
}

/// A refusal: `{ ok: false, code, reason }` (and the constraint outcome once a call got that far).
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub code: String,
    pub reason: String,
    pub constraint: Option<Outcome>,
}

fn fail<T>(code: &str, reason: impl Into<String>) -> Result<T, Failure> {
    Err(Failure {
        code: code.to_owned(),
        reason: reason.into(),
        constraint: None,
    })
}

impl Failure {
    pub fn to_json(&self) -> JValue {
        let mut o = vec![
            ("ok".into(), false.into()),
            ("code".into(), self.code.clone().into()),
            ("reason".into(), self.reason.clone().into()),
        ];
        if let Some(c) = &self.constraint {
            o.push(("constraint".into(), c.to_json()));
        }
        JValue::Obj(o)
    }
}

/// What the call's json_schema constraint did.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub applied: bool,
    pub reason: Option<String>,
    pub fallback: bool,
}

impl Outcome {
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("applied", self.applied.into()),
            (
                "mode",
                if self.applied {
                    "json_schema".into()
                } else {
                    JValue::Null
                },
            ),
            (
                "reason",
                self.reason.clone().map_or(JValue::Null, JValue::from),
            ),
            ("fallback", self.fallback.into()),
        ])
    }
}

/// A valid answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub text: String,
    pub corrected: bool,
    pub attempts: u32,
    pub constraint: Outcome,
    pub prefix: String,
}

impl Answer {
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("ok", true.into()),
            ("text", self.text.clone().into()),
            ("corrected", self.corrected.into()),
            ("attempts", JValue::Num(f64::from(self.attempts))),
            ("constraint", self.constraint.to_json()),
            ("prefix", self.prefix.clone().into()),
        ])
    }
}

/// `pinModel({ taskId, model, thinking, roles, signal })`; `roles: None` pins [`DOSSIER_ROLES`].
#[derive(Debug, Clone, Default)]
pub struct PinRequest {
    pub task_id: String,
    pub model: Option<String>,
    pub thinking: bool,
    pub roles: Option<Vec<String>>,
    pub cancel: Option<Cancel>,
}

/// `session.call({ role, state, instructions, schema, constrain, schemaName, maxTokens, maxBytes, signal })`.
#[derive(Debug, Clone)]
pub struct CallRequest {
    pub role: String,
    pub state: JValue,
    pub instructions: String,
    pub schema: JValue,
    pub constrain: bool,
    /// `None`: "role_answer".
    pub schema_name: Option<String>,
    pub max_tokens: Option<f64>,
    pub max_bytes: Option<f64>,
    pub cancel: Option<Cancel>,
}

impl CallRequest {
    /// A call of `role` on `state` with the defaults for everything else.
    pub fn new(role: &str, state: JValue, instructions: &str, schema: JValue) -> Self {
        CallRequest {
            role: role.to_owned(),
            state,
            instructions: instructions.to_owned(),
            schema,
            constrain: false,
            schema_name: None,
            max_tokens: None,
            max_bytes: None,
            cancel: None,
        }
    }
}

struct Inner<D> {
    deps: D,
    role_defaults: HashMap<String, RoleBudget>,
}

/// `createRoleEngine(deps)`.
pub struct RoleEngine<D> {
    inner: Arc<Inner<D>>,
}

/// One task's pin; every role call goes through it.
pub struct Session<D> {
    inner: Arc<Inner<D>>,
    pub task_id: String,
    pub model: String,
    /// #1853: the machine key this task is pinned to (`None`: this machine's engine).
    pub machine: Option<String>,
    pub thinking: bool,
    pub roles: Vec<String>,
    timings: Mutex<Vec<JValue>>,
}

impl<D> Session<D> {
    /// The text-free timing entries of every attempt so far.
    pub fn timings(&self) -> Vec<JValue> {
        self.timings.lock().map(|t| t.clone()).unwrap_or_default()
    }
    /// The pin as the fixtures record it.
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("taskId", self.task_id.clone().into()),
            ("model", self.model.clone().into()),
            (
                "machine",
                self.machine.clone().map_or(JValue::Null, JValue::from),
            ),
            ("thinking", self.thinking.into()),
            (
                "roles",
                JValue::Arr(self.roles.iter().map(|r| r.clone().into()).collect()),
            ),
        ])
    }
}

fn aborted(c: &Option<Cancel>) -> bool {
    c.as_ref().is_some_and(Cancel::is_cancelled)
}

fn entry<const N: usize>(pairs: [(&str, JValue); N]) -> JValue {
    JValue::obj(pairs)
}

fn opt_status(s: Option<i64>) -> JValue {
    s.map_or(JValue::Null, |n| JValue::Num(n as f64))
}

/// `${endpoint.machine.machine?.id}|${baseUrl}|${model}`.
fn machine_key(e: &Endpoint) -> Option<String> {
    e.machine.as_ref().filter(|m| m.truthy()).map(|m| {
        format!(
            "{}|{}|{}",
            js_string(m.get("machine").get("id")),
            js_string(m.get("baseUrl")),
            js_string(m.get("model"))
        )
    })
}

fn base(e: &Endpoint) -> String {
    e.base_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_owned()
}

fn headers_for(e: &Endpoint) -> Vec<(String, String)> {
    let mut h = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    if let Some(k) = e
        .api_key
        .as_deref()
        .filter(|k| !k.is_empty() && *k != "local")
    {
        h.push(("Authorization".to_owned(), format!("Bearer {k}")));
    }
    h
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

/// Why a call stopped (Node's thrown errors).
enum Stop {
    Mismatch,
    GuardAbort,
    CorrectionFailed,
    Budget(String),
    Router,
    Error {
        status: Option<i64>,
        code: &'static str,
    },
}

impl Stop {
    fn transport() -> Stop {
        Stop::Error {
            status: None,
            code: "transport",
        }
    }
}

/// `send`'s answer: a response, or a non-2xx status (its body already cancelled).
enum Sent<B> {
    Ok(Response<B>),
    Status(u16),
}

impl<D: Deps> RoleEngine<D> {
    pub fn new(deps: D) -> Self {
        Self::with_role_defaults(deps, HashMap::new())
    }

    /// `createRoleEngine({ roleDefaults })`: per-role overrides of [`role_default`].
    pub fn with_role_defaults(deps: D, role_defaults: HashMap<String, RoleBudget>) -> Self {
        RoleEngine {
            inner: Arc::new(Inner {
                deps,
                role_defaults,
            }),
        }
    }

    pub fn deps(&self) -> &D {
        &self.inner.deps
    }

    /// Pins one model for one task. Refuses (never swaps) when another chat model is resident; a
    /// cold pin loads nothing itself.
    pub async fn pin_model(&self, req: PinRequest) -> Result<Session<D>, Failure> {
        let i = &self.inner;
        if req.task_id.is_empty() {
            return fail("invalid", "A task id is required to pin a model.");
        }
        if aborted(&req.cancel) {
            return fail("aborted", "The task was cancelled.");
        }
        let endpoint = i.deps.engine().unwrap_or_default();
        if endpoint.external {
            return fail("external", "The task pipeline runs only on a local model.");
        }
        if nonempty(&endpoint.base_url).is_none() {
            return fail(
                "unavailable",
                "No model is configured for the task pipeline.",
            );
        }
        if endpoint.machine.as_ref().is_some_and(JValue::truthy) {
            return self.pin_on_machine(req, endpoint).await;
        }
        let task = || JValue::from(req.task_id.clone());
        let loaded = match i
            .deps
            .admit(Box::pin(i.read_loaded(&endpoint)), req.cancel.clone())
            .await
        {
            Ok(Ok(l)) => l,
            Ok(Err(status)) | Err(AdmissionError { status }) => {
                if aborted(&req.cancel) {
                    return fail("aborted", "The task was cancelled.");
                }
                i.deps.log(entry([
                    ("event", "role.pin_failed".into()),
                    ("taskId", task()),
                    ("code", "router_unavailable".into()),
                    ("status", opt_status(status)),
                ]));
                return fail(
                    "router_unavailable",
                    "The model router could not say which model is loaded, so no model was pinned.",
                );
            }
        };
        if aborted(&req.cancel) {
            return fail("aborted", "The task was cancelled.");
        }
        let wanted = match (
            nonempty(&req.model),
            nonempty(&endpoint.model),
            loaded.as_slice(),
        ) {
            (Some(m), _, _) | (None, Some(m), _) => m.to_owned(),
            (None, None, [one]) => one.clone(),
            _ => return fail("no_model", "No model is configured for the task pipeline."),
        };
        let others = loaded.iter().filter(|id| **id != wanted).count();
        if others > 0 {
            i.deps.log(entry([
                ("event", "role.model_refused".into()),
                ("taskId", task()),
                ("stage", "pin".into()),
                ("pinned", wanted.clone().into()),
                ("others", JValue::Num(others as f64)),
            ]));
            return fail("model_mismatch", "A different model is loaded. The task was not started, because switching models would unload it.");
        }
        if let Some(r) = i.deps.budget_refusal(&wanted).await {
            i.deps.log(entry([
                ("event", "role.budget_refused".into()),
                ("taskId", task()),
                ("stage", "pin".into()),
            ]));
            return fail(
                "over_budget",
                nonempty(&r.error).unwrap_or("The model does not fit the inference memory budget."),
            );
        }
        let cold = !loaded.contains(&wanted);
        i.deps.log(entry([
            ("event", "role.pinned".into()),
            ("taskId", task()),
            ("model", wanted.clone().into()),
            ("thinking", req.thinking.into()),
            ("cold", cold.into()),
        ]));
        Ok(self.session(req, wanted, None))
    }

    fn session(&self, req: PinRequest, model: String, machine: Option<String>) -> Session<D> {
        let roles = req
            .roles
            .unwrap_or_else(|| DOSSIER_ROLES.iter().map(|r| (*r).to_owned()).collect());
        Session {
            inner: self.inner.clone(),
            task_id: req.task_id,
            model,
            machine,
            thinking: req.thinking,
            roles,
            timings: Mutex::new(Vec::new()),
        }
    }

    /// #1853: the Code role's machine: its readiness and served model, never residency, admission or budget.
    async fn pin_on_machine(
        &self,
        req: PinRequest,
        endpoint: Endpoint,
    ) -> Result<Session<D>, Failure> {
        let i = &self.inner;
        let t = endpoint.machine.clone().unwrap_or(JValue::Undefined);
        if !i.deps.has_machines() {
            return fail(
                "unavailable",
                "This server cannot reach the Code role\u{2019}s machine.",
            );
        }
        let task = || JValue::from(req.task_id.clone());
        let name = t.get("machine").get("name");
        if let Some(m) = nonempty(&req.model).filter(|m| t.get("model").as_str() != Some(*m)) {
            i.deps.log(entry([
                ("event", "role.model_refused".into()),
                ("taskId", task()),
                ("stage", "pin".into()),
                ("machine", true.into()),
            ]));
            let on = if name.truthy() {
                js_string(name)
            } else {
                "its machine".to_owned()
            };
            return fail("model_mismatch", format!("This task names {m}, but the Code role runs {} on {on}. One task runs on one model.", js_string(t.get("model"))));
        }
        let ready = i
            .deps
            .machine_ready(&t)
            .await
            .unwrap_or_else(|()| MachineReady {
                ok: false,
                code: Some("machineUnreachable".into()),
                error: Some(format!(
                    "The Code role\u{2019}s machine ({}) is unreachable.",
                    if name.truthy() {
                        js_string(name)
                    } else {
                        String::new()
                    }
                )),
            });
        if aborted(&req.cancel) {
            return fail("aborted", "The task was cancelled.");
        }
        if !ready.ok {
            let code = nonempty(&ready.code)
                .unwrap_or("machineUnreachable")
                .to_owned();
            i.deps.log(entry([
                ("event", "role.pin_failed".into()),
                ("taskId", task()),
                ("code", code.clone().into()),
            ]));
            return fail(&code, nonempty(&ready.error).unwrap_or(MACHINE_UNREACHABLE));
        }
        let model = js_string(t.get("model"));
        i.deps.log(entry([
            ("event", "role.pinned".into()),
            ("taskId", task()),
            ("model", t.get("model").clone()),
            ("thinking", req.thinking.into()),
            ("machine", true.into()),
        ]));
        let key = machine_key(&endpoint);
        Ok(self.session(req, model, key))
    }
}

impl<D: Deps> Inner<D> {
    /// The chat models resident (`readLoaded`): `Err(status)` when that cannot be said.
    async fn read_loaded(&self, endpoint: &Endpoint) -> Result<Vec<String>, Option<i64>> {
        let kept = self.deps.keep().await.ok_or(Some(503))?;
        if let Some(r) = self.deps.loaded_models().await {
            let JValue::Arr(ids) = r.map_err(|()| None)? else {
                return Err(None);
            };
            return Ok(ids
                .into_iter()
                .filter_map(|v| {
                    v.as_str()
                        .filter(|s| !s.is_empty() && !kept.iter().any(|k| k == s))
                        .map(str::to_owned)
                })
                .collect());
        }
        let b = base(endpoint);
        let url = format!("{}/models", b.strip_suffix("/v1").unwrap_or(&b));
        let req = Request {
            url,
            method: "GET",
            headers: headers_for(endpoint),
            body: None,
            timeout_ms: Some(ROUTER_TIMEOUT_MS),
        };
        let mut res = self
            .deps
            .fetch(req, Cancel::new())
            .await
            .map_err(|_| None)?;
        if !(200..300).contains(&res.status) {
            res.body.cancel();
            return Err(Some(i64::from(res.status)));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.body.read().await.map_err(|_| None)? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > ROUTER_CAP {
                res.body.cancel();
                return Err(Some(502));
            }
        }
        let body = wtf16::parse(&String::from_utf8_lossy(&bytes)).map_err(|_| None)?;
        Ok(loaded_from_model_list(
            &body,
            &kept,
            &self.deps.system_roles(),
        ))
    }

    /// One POST through the endpoint's transport, raced with the attempt's cancel.
    async fn send(
        &self,
        on_machine: bool,
        req: Request,
        cancel: &Cancel,
        connect_failed: &mut Option<TransportError>,
    ) -> Result<Sent<D::Body>, TransportError> {
        let aborted = || TransportError {
            detail: "aborted".into(),
        };
        let r = if on_machine {
            tokio::select! { biased; r = self.deps.machine_fetch(req, cancel.clone()) => r, () = cancel.cancelled() => Err(aborted()) }
        } else {
            tokio::select! { biased; r = self.deps.fetch(req, cancel.clone()) => r, () = cancel.cancelled() => Err(aborted()) }
        };
        match r {
            Err(e) => {
                if on_machine && !cancel.is_cancelled() {
                    *connect_failed = Some(e.clone());
                }
                Err(e)
            }
            Ok(mut res) if !(200..300).contains(&res.status) => {
                res.body.cancel();
                Ok(Sent::Status(res.status))
            }
            Ok(res) => Ok(Sent::Ok(res)),
        }
    }
}

/// What one call carries through its attempts.
struct Call<'a> {
    on_machine: bool,
    endpoint: &'a Endpoint,
    url: String,
    headers: Vec<(String, String)>,
    role: &'a str,
    instructions: &'a str,
    constraint: Constraint,
    outcome: Outcome,
    connect_failed: Option<TransportError>,
}

impl<D: Deps> Session<D> {
    fn log(&self, pairs: Vec<(&str, JValue)>) {
        let mut o = vec![
            ("event".to_owned(), JValue::Null),
            ("taskId".to_owned(), self.task_id.clone().into()),
        ];
        for (k, v) in pairs {
            if k == "event" {
                if let Some(slot) = o.first_mut() {
                    slot.1 = v;
                }
            } else {
                o.push((k.to_owned(), v));
            }
        }
        self.inner.deps.log(JValue::Obj(o));
    }

    /// One role call under the pinned model.
    pub async fn call(&self, req: CallRequest) -> Result<Answer, Failure> {
        let deps = &self.inner.deps;
        let role = req.role.as_str();
        if !self.roles.iter().any(|r| r == role) {
            return fail("role_not_pinned", "That role is not part of this task.");
        }
        let (dt, db) = role_default(role);
        let over = self
            .inner
            .role_defaults
            .get(role)
            .copied()
            .unwrap_or_default();
        let positive =
            |v: Option<f64>| v.filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 1.0);
        let max_tokens = positive(req.max_tokens).unwrap_or(over.max_tokens.unwrap_or(dt));
        let max_bytes = positive(req.max_bytes).unwrap_or(over.max_bytes.unwrap_or(db));
        if aborted(&req.cancel) {
            return fail("aborted", "The task was cancelled.");
        }
        if req.state.get("taskId").as_str() != Some(self.task_id.as_str()) {
            return fail("task_mismatch", "That call belongs to a different task.");
        }
        let endpoint = deps.engine().unwrap_or_default();
        if endpoint.external {
            return fail("external", "The task pipeline runs only on a local model.");
        }
        if nonempty(&endpoint.base_url).is_none() {
            return fail(
                "unavailable",
                "No model is configured for the task pipeline.",
            );
        }
        let on_machine = endpoint.machine.as_ref().is_some_and(JValue::truthy);
        if machine_key(&endpoint) != self.machine {
            self.log(vec![
                ("event", "role.model_refused".into()),
                ("role", role.into()),
                ("stage", "call".into()),
                ("machine", on_machine.into()),
            ]);
            return fail("model_mismatch", "The Code role moved to another machine or model during this task. This step was not sent, because one task runs on one model.");
        }
        let target = endpoint.machine.clone().unwrap_or(JValue::Undefined);
        if on_machine {
            let ready = if deps.has_machines() {
                deps.machine_ready(&target).await.ok()
            } else {
                None
            };
            let ready = ready.unwrap_or(MachineReady {
                ok: false,
                code: None,
                error: None,
            });
            if !ready.ok {
                return fail(
                    nonempty(&ready.code).unwrap_or("machineUnreachable"),
                    nonempty(&ready.error).unwrap_or(MACHINE_UNREACHABLE),
                );
            }
        }
        let (dossier, fields) = match self.context(&req) {
            Ok(x) => x,
            Err(Projection::Leak(classes)) => {
                self.log(vec![
                    ("event", "role.context_refused".into()),
                    ("role", role.into()),
                    ("classes", JValue::Arr(classes)),
                ]);
                return fail(
                    "context_refused",
                    "The task was not sent because its context would have carried private data.",
                );
            }
            Err(Projection::Refused(code)) if code == "too_large" => {
                return fail("too_large", "The task is too large for this step.")
            }
            Err(Projection::Refused(_)) => {
                return fail(
                    "context_invalid",
                    "The task could not be prepared for this step.",
                )
            }
        };
        let name = req
            .schema_name
            .clone()
            .unwrap_or_else(|| "role_answer".to_owned());
        let constraint = schema_constraint(
            req.constrain,
            &endpoint.provider,
            &self.model,
            self.thinking,
            &req.schema,
            &name,
        );
        let mut call = Call {
            on_machine,
            endpoint: &endpoint,
            url: format!("{}/chat/completions", base(&endpoint)),
            headers: if on_machine {
                vec![("Content-Type".to_owned(), "application/json".to_owned())]
            } else {
                headers_for(&endpoint)
            },
            role,
            instructions: &req.instructions,
            outcome: Outcome {
                applied: false,
                reason: constraint.reason.clone(),
                fallback: false,
            },
            constraint,
            connect_failed: None,
        };
        let parent = req.cancel.clone().unwrap_or_default();
        let mut prefix = String::new();
        let r = self
            .guarded(
                &req,
                &mut call,
                &parent,
                &dossier,
                &fields,
                max_tokens,
                max_bytes,
                &mut prefix,
            )
            .await;
        let constraint = Some(call.outcome.clone());
        let stop = match r {
            Ok((text, attempts)) => {
                return Ok(Answer {
                    text,
                    corrected: attempts == 2,
                    attempts,
                    constraint: call.outcome,
                    prefix,
                });
            }
            Err(s) => s,
        };
        let f = |code: &str, reason: &str| {
            Err(Failure {
                code: code.into(),
                reason: reason.into(),
                constraint: constraint.clone(),
            })
        };
        match stop {
            Stop::Mismatch => f("model_mismatch", "A different model is loaded. This step was not sent, because switching models would unload it."),
            _ if aborted(&req.cancel) => f("aborted", "The task was cancelled."),
            Stop::GuardAbort => f("aborted", "The task was cancelled."),
            Stop::CorrectionFailed => {
                self.log(vec![("event", "role.invalid".into()), ("role", role.into()), ("corrected", true.into())]);
                f("invalid", "The answer did not match the expected format, even after one correction.")
            }
            Stop::Budget(msg) => f("over_budget", if msg.is_empty() { "The model does not fit the inference memory budget, so this step was not sent." } else { &msg }),
            Stop::Router => f("router_unavailable", "The model router could not say which model is loaded, so this step was not sent."),
            Stop::Error { status, code } => {
                let unreachable = if on_machine { call.connect_failed.as_ref().and_then(|e| deps.machine_connect_error(&target, e)) } else { None };
                if let Some(text) = unreachable {
                    self.log(vec![("event", "role.call_failed".into()), ("role", role.into()), ("status", JValue::Null), ("code", "machineUnreachable".into())]);
                    return f("machineUnreachable", &text);
                }
                self.log(vec![("event", "role.call_failed".into()), ("role", role.into()), ("status", opt_status(status)), ("code", code.into())]);
                f("error", "The model could not be reached.")
            }
        }
    }

    /// The dossier (shared by the task's roles) and this role's own fields, serialised.
    fn context(&self, req: &CallRequest) -> Result<(String, String), Projection> {
        let roles = JValue::Arr(self.roles.iter().map(|r| r.clone().into()).collect());
        let shared = project(2, roles, &req.state, "dossier")?;
        let own = project(1, req.role.clone().into(), &req.state, "projection")?;
        let JValue::Obj(own) = own else {
            return Err(impl_refused());
        };
        let persona: Vec<(String, JValue)> = own
            .into_iter()
            .filter(|(k, _)| matches!(shared.get(k), JValue::Undefined))
            .collect();
        Ok((stringify(&shared), stringify(&JValue::Obj(persona))))
    }

    #[allow(clippy::too_many_arguments)]
    async fn guarded(
        &self,
        req: &CallRequest,
        call: &mut Call<'_>,
        parent: &Cancel,
        dossier: &str,
        fields: &str,
        max_tokens: f64,
        max_bytes: f64,
        prefix: &mut String,
    ) -> Result<(String, u32), Stop> {
        let mut correction: Option<JValue> = None;
        for attempt in 1..=2u32 {
            if parent.is_cancelled() {
                return Err(Stop::GuardAbort);
            }
            let schema_v = if req.schema.truthy() {
                req.schema.clone()
            } else {
                JValue::Null
            };
            // The validator refuses as dav-parse-wasm.cjs does: a DavParseError (status 502).
            let refused = || Stop::Error {
                status: Some(502),
                code: "transport",
            };
            if !plain_json(&schema_v, 0) || max_bytes.abs() > MAX_GUARD_BYTES as f64 {
                return Err(refused());
            }
            let schema = Schema::parse(stringify(&schema_v).as_bytes()).map_err(|_| refused())?;
            let state = State::new(DEFAULT_MAX_DEPTH, max_bytes as i64).map_err(|_| refused())?;
            let mut validator = Validator::new(&schema, state);
            let cancel = parent.child();
            let (text, violation) = self
                .attempt(
                    call,
                    attempt,
                    correction.as_ref(),
                    &cancel,
                    parent,
                    dossier,
                    fields,
                    max_tokens,
                    prefix,
                    &mut validator,
                )
                .await?;
            let violation = match violation {
                Some(v) => v,
                None => {
                    if parent.is_cancelled() {
                        return Err(Stop::GuardAbort);
                    }
                    match validator.end().cloned() {
                        None => return Ok((join_pairs(&text), attempt)),
                        Some(v) => v,
                    }
                }
            };
            if attempt == 2 {
                return Err(Stop::CorrectionFailed);
            }
            if parent.is_cancelled() {
                return Err(Stop::GuardAbort);
            }
            let path = if violation.path.is_empty() {
                JValue::Null
            } else {
                from_units(&violation.path).into()
            };
            let v = JValue::obj([
                ("message", from_units(&violation.message).into()),
                ("path", path),
            ]);
            correction = Some(JValue::obj([
                ("type", "schema_violation_correction".into()),
                ("violation", v),
            ]));
        }
        Err(Stop::CorrectionFailed)
    }

    /// One attempt (`createStream` and the guard's loop): the text read and the first violation.
    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        call: &mut Call<'_>,
        attempt: u32,
        correction: Option<&JValue>,
        cancel: &Cancel,
        parent: &Cancel,
        dossier: &str,
        fields: &str,
        max_tokens: f64,
        prefix: &mut String,
        validator: &mut Validator<'_>,
    ) -> Result<(String, Option<stream_guard::Violation>), Stop> {
        let deps = &self.inner.deps;
        let messages = build_messages(dossier, call.instructions, fields, correction);
        *prefix = prefix_hash(&self.model, self.thinking, dossier);
        let payload = vec![
            ("model".to_owned(), JValue::from(self.model.clone())),
            ("messages".to_owned(), messages),
            ("stream".to_owned(), true.into()),
            ("cache_prompt".to_owned(), true.into()),
            (
                "stream_options".to_owned(),
                JValue::obj([("include_usage", true.into())]),
            ),
            ("temperature".to_owned(), JValue::Num(0.0)),
            ("max_tokens".to_owned(), JValue::Num(max_tokens)),
            (
                "chat_template_kwargs".to_owned(),
                JValue::obj([("enable_thinking", self.thinking.into())]),
            ),
        ];
        let mut m = Meter {
            start: deps.now(),
            ..Meter::default()
        };
        let res = if call.on_machine {
            self.ask(call, payload, cancel).await?
        } else {
            if let Some(r) = deps.budget_refusal(&self.model).await {
                return Err(Stop::Budget(r.error.unwrap_or_default()));
            }
            let work = async {
                self.assert_resident(call.endpoint, call.role).await?;
                self.ask(call, payload, cancel).await
            };
            match deps.admit(Box::pin(work), Some(cancel.clone())).await {
                Ok(r) => r?,
                Err(AdmissionError { status }) => {
                    return Err(Stop::Error {
                        status,
                        code: "transport",
                    })
                }
            }
        };
        let base_entry = [
            ("event", JValue::from("role.call")),
            ("taskId", self.task_id.clone().into()),
            ("role", call.role.into()),
            ("attempt", JValue::Num(f64::from(attempt))),
            ("model", self.model.clone().into()),
            ("thinking", self.thinking.into()),
            ("prefix", prefix.clone().into()),
            ("constrained", call.outcome.applied.into()),
        ];
        let mut body = res.body;
        let mut dec = SseDecoder::default();
        let mut text = String::new();
        let now = || deps.now();
        let outcome = loop {
            let item = match dec.next(&mut m, &now) {
                Some(item) => item,
                None => {
                    let read = tokio::select! { biased; r = body.read() => r, () = cancel.cancelled() => Err(TransportError::default()) };
                    match read {
                        Ok(Some(bytes)) => {
                            dec.push(&bytes, &mut m);
                            continue;
                        }
                        Ok(None) => {
                            m.complete = true;
                            break Ok(None);
                        }
                        Err(_) => break Err(Stop::transport()),
                    }
                }
            };
            match item {
                SseItem::Done => break Ok(None),
                SseItem::EngineError => {
                    break Err(Stop::Error {
                        status: None,
                        code: "engine_error",
                    })
                }
                SseItem::Delta(d) => {
                    if parent.is_cancelled() {
                        break Err(Stop::GuardAbort);
                    }
                    text.push_str(&d);
                    if let Some(v) = validator.feed(&units(&d)).cloned() {
                        cancel.cancel();
                        break Ok(Some(v));
                    }
                }
            }
        };
        body.cancel();
        let mut t: Vec<(String, JValue)> = base_entry
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        for (k, v) in m.timings {
            wtf16::set(&mut t, &k, v);
        }
        let wall = deps.now() - m.start;
        let tail = [
            ("bytes", JValue::Num(m.bytes as f64)),
            ("deltas", JValue::Num(m.deltas as f64)),
            ("reasoning_deltas", JValue::Num(m.reasoning_deltas as f64)),
            (
                "first_delta_ms",
                m.first_delta_ms.map_or(JValue::Null, JValue::Num),
            ),
            ("wall_ms", JValue::Num(wall)),
            ("halted", (!m.complete).into()),
        ];
        for (k, v) in tail {
            wtf16::set(&mut t, k, v);
        }
        let t = JValue::Obj(t);
        if let Ok(mut all) = self.timings.lock() {
            all.push(t.clone());
        }
        deps.log(t);
        outcome.map(|v| (text, v))
    }

    async fn assert_resident(&self, endpoint: &Endpoint, role: &str) -> Result<(), Stop> {
        let loaded = self
            .inner
            .read_loaded(endpoint)
            .await
            .map_err(|_| Stop::Router)?;
        let others = loaded.iter().filter(|id| **id != self.model).count();
        if others > 0 {
            self.log(vec![
                ("event", "role.model_refused".into()),
                ("role", role.into()),
                ("stage", "call".into()),
                ("pinned", self.model.clone().into()),
                ("others", JValue::Num(others as f64)),
            ]);
            return Err(Stop::Mismatch);
        }
        Ok(())
    }

    /// `requestPlanArtifact`: constrained when allowed; a 400/422/501 retries once without.
    async fn ask(
        &self,
        call: &mut Call<'_>,
        payload: Vec<(String, JValue)>,
        cancel: &Cancel,
    ) -> Result<Response<D::Body>, Stop> {
        let inner = &self.inner;
        let (url, headers) = (call.url.clone(), call.headers.clone());
        let post = |p: &Vec<(String, JValue)>| Request {
            url: url.clone(),
            method: "POST",
            headers: headers.clone(),
            body: Some(stringify(&JValue::Obj(p.clone()))),
            timeout_ms: None,
        };
        let unwrap = |r: Result<Sent<D::Body>, TransportError>| match r {
            Ok(Sent::Ok(res)) => Ok(res),
            Ok(Sent::Status(s)) => Err(Stop::Error {
                status: Some(i64::from(s)),
                code: "transport",
            }),
            Err(_) => Err(Stop::transport()),
        };
        let (res, outcome) = if !call.constraint.applied {
            let req = post(&payload);
            let res = unwrap(
                inner
                    .send(call.on_machine, req, cancel, &mut call.connect_failed)
                    .await,
            )?;
            (
                res,
                Outcome {
                    applied: false,
                    reason: Some(
                        call.constraint
                            .reason
                            .clone()
                            .unwrap_or_else(|| "flag_off".into()),
                    ),
                    fallback: false,
                },
            )
        } else {
            let mut constrained = payload.clone();
            if let JValue::Obj(fields) = &call.constraint.fields {
                for (k, v) in fields {
                    wtf16::set(&mut constrained, k, v.clone());
                }
            }
            let req = post(&constrained);
            match inner
                .send(call.on_machine, req, cancel, &mut call.connect_failed)
                .await
            {
                Ok(Sent::Ok(res)) => (
                    res,
                    Outcome {
                        applied: true,
                        reason: None,
                        fallback: false,
                    },
                ),
                Ok(Sent::Status(s)) if matches!(s, 400 | 422 | 501) => {
                    let e = [
                        ("event", "plan.constraint_rejected".into()),
                        ("status", JValue::Num(f64::from(s))),
                        ("taskId", self.task_id.clone().into()),
                        ("role", call.role.into()),
                    ];
                    inner.deps.log(JValue::obj(e));
                    let req = post(&payload);
                    let res = unwrap(
                        inner
                            .send(call.on_machine, req, cancel, &mut call.connect_failed)
                            .await,
                    )?;
                    (
                        res,
                        Outcome {
                            applied: false,
                            reason: Some(format!("engine_rejected_{s}")),
                            fallback: true,
                        },
                    )
                }
                other => return Err(unwrap(other).err().unwrap_or_else(Stop::transport)),
            }
        };
        if outcome.fallback {
            call.constraint = Constraint::none(outcome.reason.as_deref().unwrap_or_default());
            call.outcome = outcome;
        } else if !call.outcome.fallback {
            call.outcome = outcome;
        }
        Ok(res)
    }
}

/// role-context.cjs `askPort`'s outcomes other than a projection.
enum Projection {
    Leak(Vec<JValue>),
    Refused(String),
}

fn impl_refused() -> Projection {
    Projection::Leak(vec!["impl_refused".into()])
}

/// `projectRoleContext` (op 1) / `projectSharedDossier` (op 2) through the `role-context` crate,
/// on the same wire the wasm module takes: the projection, already canonical.
fn project(op: u8, first: JValue, state: &JValue, key: &str) -> Result<JValue, Projection> {
    let text = stringify(&JValue::Arr(vec![first, state.clone()]));
    let mut input = Vec::with_capacity(text.len() + 1);
    input.push(op);
    input.extend_from_slice(text.as_bytes());
    let (status, reply) = role_context::call(&input);
    if status != 0 {
        return Err(impl_refused());
    }
    let Ok(JValue::Obj(items)) = wtf16::parse(&reply) else {
        return Err(impl_refused());
    };
    let reply = JValue::Obj(items);
    if let JValue::Obj(_) = reply.get(key) {
        return Ok(reply.get(key).clone());
    }
    if let Some(code) = reply.get("refused").as_str() {
        return Err(Projection::Refused(code.to_owned()));
    }
    match reply.get("leak") {
        JValue::Arr(classes) if !classes.is_empty() => Err(Projection::Leak(classes.clone())),
        _ => Err(impl_refused()),
    }
}
