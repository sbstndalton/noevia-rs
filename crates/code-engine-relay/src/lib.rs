//! Core's Code engine relay (code-machine.cjs `createEngineRelay`, noevia#1853 and the #1876
//! hardening), ported for the Rust front. Inert: nothing in the front uses it yet.
//!
//! When the Code role (or Smart, its fallback) runs on an inference machine, every stage of a Code
//! task runs on that machine's model. The sandbox must never reach a LAN machine itself, so web
//! listens on one more port on its own code-network address and forwards exactly two things for a
//! running task:
//! - `GET /v1/models` (or `/models`), answered here: the one model the task runs on;
//! - `POST /v1/chat/completions` (or `/chat/completions`), forwarded to the machine through the
//!   caller's guarded client ([`Machines::fetch`]), `model` forced to the task's, no API key.
//!
//! A request needs the task's relay token (`Authorization: Bearer ncr_…`), minted by [`Relay::open`]
//! when the Executor starts and revoked by [`Opened::close`] when it ends. Every request re-checks
//! that the role is still on the same machine with the same model, and the machine's health.
//!
//! What this crate is: the lease authority, the request handler with Node's answers (statuses,
//! `relay_error` bodies, log events) and, in [`bind`], the listener's address rules and the
//! environment it is read from, and in [`listener`] the HTTP/1.1 server with the #1876 limits
//! (or the caller's own: it turns a request into a [`Request`] and writes the [`Reply`],
//! `Cache-Control: no-store` on every answer). What it is not: the machine client; the caller
//! supplies [`Machines`] (in core: chat-machines.cjs `ready`, `fetch` =
//! serviceFetch with its on-this-network rules, `connectError`, `contextTokens`, `beginTurn`).
//!
//! Named differences from Node (each a refusal or a cleanup, never a different success):
//! - an [`Opened`] lease that is dropped without [`Opened::close`] is closed (Node's lease outlived
//!   a caller that forgot to close it);
//! - a request whose body does not arrive within `requestTimeout` is dropped without an answer
//!   (Node's server answers 408 and closes); an idle keep-alive connection lasts up to the head
//!   timeout (10 s) rather than `keepAliveTimeout` (5 s);
//! - the `Accept` header forwarded is cut at 200 UTF-16 units of its text as the caller decoded it
//!   (Node slices the Latin-1 string it read);
//! - when the system's random source fails, [`Relay::open`] refuses with `machineRelayOff` ("The
//!   relay could not make a token for this task.") before any lease exists (Node's
//!   `crypto.randomBytes` throws there, an error without a public message).
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub mod bind;
pub mod listener;

/// code-machine.cjs `BODY_CAP`: the largest request body the relay reads.
pub const BODY_CAP: usize = 16 * 1024 * 1024;
/// `MAX_CONCURRENT_PER_TASK`: model requests one task may have in flight.
pub const MAX_CONCURRENT_PER_TASK: usize = 4;
/// `REQUEST_TIMEOUT_MS`: one forwarded request, its streamed answer included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The token's prefix (`ncr_` + 24 random bytes, base64url).
pub const TOKEN_PREFIX: &str = "ncr_";

/// The machine a Code task runs on: chat-machines.cjs `target(role)` as the relay reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub machine_id: String,
    pub machine_name: String,
    /// The machine's engine, without `/v1`.
    pub base_url: String,
    pub model: String,
}

/// `targetKey`: a stable identity for "this role on this machine serving this model".
pub fn target_key(t: Option<&Target>) -> Option<String> {
    t.map(|t| format!("{}|{}|{}", t.machine_id, t.base_url, t.model))
}

/// chat-machines.cjs `ready(target)`: `{ ok }`, else the #1840 code and text. `error` is `None`
/// where Node's answer has no `error` (the relay's 502 then has no `message`, as
/// `JSON.stringify` drops an undefined key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub ok: bool,
    pub code: Option<String>,
    pub error: Option<String>,
}

impl Ready {
    pub fn ok() -> Ready {
        Ready {
            ok: true,
            code: None,
            error: None,
        }
    }
}

/// One forwarded request: `POST {url}` with `Content-Type: application/json`, this `Accept` and this
/// body, nothing else (no credentials, no relay token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub url: String,
    pub accept: String,
    pub body: String,
}

/// The machine's answer, its body read chunk by chunk.
pub trait Upstream: Send + 'static {
    fn status(&self) -> u16;
    fn content_type(&self) -> Option<String>;
    /// The next chunk; `None` at the end; `Some(Err(()))` when the machine hung up.
    fn next(&mut self) -> impl Future<Output = Option<Result<Vec<u8>, ()>>> + Send;
}

/// What the relay needs of the inference machines (core chat-machines.cjs).
pub trait Machines: Send + Sync + 'static {
    type Upstream: Upstream;
    type Error: Send;
    fn ready(&self, target: &Target) -> impl Future<Output = Ready> + Send;
    /// `contextTokens(target)`; a failure is `None`.
    fn context_tokens(&self, _target: &Target) -> impl Future<Output = Option<f64>> + Send {
        async { None }
    }
    /// #1880 `beginTurn(machineId)`: the release, called once the answer has been relayed.
    fn begin_turn(&self, _machine_id: &str) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }
    /// serviceFetch: no redirects, link-local refused, the on-this-network check at connect time.
    fn fetch(
        &self,
        request: Forward,
    ) -> impl Future<Output = Result<Self::Upstream, Self::Error>> + Send;
    /// `connectError(target, err)`: the #1840 "unreachable" text; `error` is `None` when the request
    /// ran out of time before the machine answered (Node's `AbortSignal.timeout`).
    fn connect_error(&self, target: &Target, error: Option<&Self::Error>) -> String;
}

pub type TargetFn = Arc<dyn Fn() -> Option<Target> + Send + Sync>;
pub type LogFn = Arc<dyn Fn(JValue) + Send + Sync>;

/// `createEngineRelay`'s options.
pub struct Options<M> {
    pub machines: Arc<M>,
    /// The Code task's machine now (`codeTargetFrom`); `None` is this machine.
    pub target: TargetFn,
    /// The relay as the sandbox reaches it, e.g. `http://egress:8095/v1`; `None`: no relay.
    pub url: Option<String>,
    pub log: LogFn,
    pub body_cap: usize,
    pub max_concurrent: usize,
    pub request_timeout: Duration,
}

impl<M> Options<M> {
    pub fn new(machines: Arc<M>, target: TargetFn, url: Option<String>) -> Options<M> {
        Options {
            machines,
            target,
            url,
            log: Arc::new(|_| {}),
            body_cap: BODY_CAP,
            max_concurrent: MAX_CONCURRENT_PER_TASK,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

/// Why [`Relay::open`] refused: the code and the public words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: String,
    pub message: String,
}

fn refusal(code: &str, message: String) -> Refusal {
    Refusal {
        code: code.to_string(),
        message,
    }
}

struct Lease {
    task_id: String,
    target: Target,
    key: Option<String>,
    active: Mutex<usize>,
    /// Cancelled when the lease closes: every request in flight is cut.
    cancel: CancellationToken,
}

struct Table {
    leases: Mutex<HashMap<String, Arc<Lease>>>,
    log: LogFn,
}

impl Table {
    fn record(&self, entry: JValue) {
        // Never changes the outcome (Node: a log that throws is ignored).
        let log = Arc::clone(&self.log);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log(entry)));
    }

    /// The lease map. A panic elsewhere while it was held does not leave it half-changed (each
    /// use is one insert, remove or read), so a poisoned lock is taken as it is: otherwise `open`
    /// would hand out a token that never authenticates and `close` would not revoke (noevia#2676).
    fn leases(&self) -> MutexGuard<'_, HashMap<String, Arc<Lease>>> {
        self.leases.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, key: &str) -> Option<Arc<Lease>> {
        self.leases().get(key).cloned()
    }
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn mint() -> Option<String> {
    use base64::Engine as _;
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).ok()?;
    Some(format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn event(name: &str, task_id: &str) -> Vec<(String, JValue)> {
    vec![
        ("event".into(), JValue::Str(name.into())),
        ("taskId".into(), JValue::Str(task_id.into())),
    ]
}

/// One Executor run's relay lease: the engine the harness is given.
pub struct Opened {
    /// The relay's URL (the harness's engine).
    pub base_url: String,
    /// The harness's API key for the relay.
    pub token: String,
    pub model: String,
    pub context_tokens: Option<f64>,
    pub target: Target,
    table: Arc<Table>,
    key: String,
    task_id: String,
    closed: AtomicBool,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("model", &self.model)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Opened {
    /// Revoke the token and cut every request of this run still in flight. Idempotent.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let lease = self.table.leases().remove(&self.key);
        if let Some(lease) = lease {
            lease.cancel.cancel();
        }
        self.table
            .record(JValue::Obj(event("relay.closed", &self.task_id)));
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.close();
    }
}

/// A request the listener received, as the relay reads it.
pub struct Request<B> {
    pub method: String,
    /// The request target as sent (`req.url`); its query is ignored.
    pub url: String,
    pub authorization: Option<String>,
    pub accept: Option<String>,
    pub body: B,
}

/// Why a body could not be read whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFail {
    /// Over the cap: the relay answers 413.
    TooLarge,
    /// The connection failed while reading.
    Failed,
    /// The whole request did not arrive in time (`requestTimeout`): the connection is dropped.
    TimedOut,
}

/// The request body, read only once the token and the path passed (Node `req.resume()`s the
/// others): `read(cap)` must stop at `cap` bytes with [`BodyFail::TooLarge`].
pub trait BodySource: Send {
    fn read(self, cap: usize) -> impl Future<Output = Result<Vec<u8>, BodyFail>> + Send;
}

impl BodySource for Vec<u8> {
    async fn read(self, cap: usize) -> Result<Vec<u8>, BodyFail> {
        if self.len() > cap {
            Err(BodyFail::TooLarge)
        } else {
            Ok(self)
        }
    }
}

/// The relay's answer to one request.
pub enum Reply<U> {
    /// A JSON answer (`Content-Type: application/json`, `Cache-Control: no-store`).
    Json { status: u16, body: String },
    /// The machine's answer, relayed as it comes.
    Stream(Relayed<U>),
    /// The lease closed (or the client left) while the request was in flight: drop the connection
    /// without an answer (Node `res.destroy()`).
    Cut,
}

impl<U> std::fmt::Debug for Reply<U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reply::Json { status, body } => write!(f, "Json({status}, {body})"),
            Reply::Stream(_) => write!(f, "Stream"),
            Reply::Cut => write!(f, "Cut"),
        }
    }
}

/// One slot of a lease's concurrency, and the machine turn it holds: both given back on drop.
struct Slot {
    lease: Arc<Lease>,
    turn: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(release) = self.turn.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(release));
        }
        if let Ok(mut n) = self.lease.active.lock() {
            *n = n.saturating_sub(1);
        }
    }
}

/// What [`Relayed::next`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chunk {
    Data(Vec<u8>),
    /// The answer ended (also when the machine hung up or the request's time ran out: Node ends the
    /// response then).
    End,
    /// The lease closed: drop the connection (Node `res.destroy()`).
    Cut,
}

/// A forwarded request's answer in flight. Dropping it (the client left) drops the machine's
/// request too, and gives the lease's slot and the machine turn back.
pub struct Relayed<U> {
    status: u16,
    content_type: String,
    upstream: Option<U>,
    cancel: CancellationToken,
    deadline: Instant,
    _slot: Slot,
}

impl<U: Upstream> Relayed<U> {
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The machine's `content-type`, else `application/json`.
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    pub async fn next(&mut self) -> Chunk {
        let Some(up) = self.upstream.as_mut() else {
            return Chunk::End;
        };
        let got = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                self.upstream = None;
                return Chunk::Cut;
            }
            () = tokio::time::sleep_until(self.deadline) => None,
            got = up.next() => got,
        };
        match got {
            Some(Ok(data)) => Chunk::Data(data),
            _ => {
                self.upstream = None;
                Chunk::End
            }
        }
    }
}

/// `jsonError(res, status, message, code)`.
pub fn json_error(
    status: u16,
    message: &str,
    code: Option<&str>,
) -> Reply<std::convert::Infallible> {
    error_body(status, Some(message), code)
}

/// `jsonError` with `message` possibly undefined (then the key is left out).
fn error_body<U>(status: u16, message: Option<&str>, code: Option<&str>) -> Reply<U> {
    let mut error = Vec::new();
    if let Some(message) = message {
        error.push(("message".to_string(), JValue::Str(message.into())));
    }
    error.push(("type".to_string(), JValue::Str("relay_error".into())));
    if let Some(code) = code.filter(|c| !c.is_empty()) {
        error.push(("code".into(), JValue::Str(code.into())));
    }
    Reply::Json {
        status,
        body: stringify(&JValue::obj([("error", JValue::Obj(error))])),
    }
}

fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "null".into())
}

fn error_reply<U>(status: u16, message: &str, code: Option<&str>) -> Reply<U> {
    error_body(status, Some(message), code)
}

/// JS `\s` (WhiteSpace and LineTerminator).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `/^Bearer\s+(\S+)$/i.exec(authorization)`: the token, or `None`.
pub fn bearer(authorization: &str) -> Option<&str> {
    let head = authorization.get(..6)?;
    if !head.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = authorization.get(6..)?;
    let token = rest.trim_start_matches(js_space);
    if token.len() == rest.len() || token.is_empty() || token.chars().any(js_space) {
        return None;
    }
    Some(token)
}

/// The relay: `createEngineRelay`'s `open`, `handle`, `active`, `disable` and `url`.
pub struct Relay<M: Machines> {
    machines: Arc<M>,
    target: TargetFn,
    url: Mutex<Option<String>>,
    table: Arc<Table>,
    body_cap: usize,
    max_concurrent: usize,
    request_timeout: Duration,
}

impl<M: Machines> Relay<M> {
    pub fn new(o: Options<M>) -> Relay<M> {
        Relay {
            machines: o.machines,
            target: o.target,
            url: Mutex::new(o.url),
            table: Arc::new(Table {
                leases: Mutex::new(HashMap::new()),
                log: o.log,
            }),
            body_cap: o.body_cap,
            max_concurrent: o.max_concurrent,
            request_timeout: o.request_timeout,
        }
    }

    fn current(&self) -> Option<Target> {
        let target = Arc::clone(&self.target);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| target()))
            .ok()
            .flatten()
    }

    /// The relay's URL, or `None` (no relay, or a listener that could not bind).
    pub fn url(&self) -> Option<String> {
        self.url.lock().ok().and_then(|u| u.clone())
    }

    /// A relay that cannot bind is off: Machine Code tasks then refuse with `machineRelayOff`.
    pub fn disable(&self) {
        if let Ok(mut u) = self.url.lock() {
            *u = None;
        }
    }

    /// Open leases.
    pub fn active(&self) -> usize {
        self.table.leases().len()
    }

    /// Open the relay for one Executor run. `model` is the model the task names (`None` or empty:
    /// none); `expect` the target key the task was pinned to (`None`: not checked; `Some(None)`: this
    /// machine). Refuses when there is no relay, the role moved, the task names another model, or
    /// the machine is not ready.
    pub async fn open(
        &self,
        task_id: &str,
        model: Option<&str>,
        expect: Option<Option<&str>>,
    ) -> Result<Opened, Refusal> {
        let Some(t) = self.current() else {
            return Err(refusal(
                "model_mismatch",
                "The Code role moved back to this machine while the task was starting, and one task runs on one model. Start the task again.".into(),
            ));
        };
        let Some(base_url) = self.url() else {
            return Err(refusal(
                "machineRelayOff",
                format!(
                    "The Code role runs on {}, but the sandbox has no route to it: web needs CODE_ENGINE_RELAY_PORT (see the code-sandbox override). The task was not started.",
                    t.machine_name
                ),
            ));
        };
        let key = target_key(Some(&t));
        if let Some(expect) = expect {
            if expect.map(str::to_string) != key {
                return Err(refusal(
                    "model_mismatch",
                    "The Code role moved to another machine or model during this task, and one task runs on one model. Start the task again.".into(),
                ));
            }
        }
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            if model != t.model {
                return Err(refusal(
                    "model_mismatch",
                    format!(
                        "This task names {model}, but the Code role runs {} on {}. One task runs on one model.",
                        t.model, t.machine_name
                    ),
                ));
            }
        }
        let ready = self.machines.ready(&t).await;
        if !ready.ok {
            let code = ready
                .code
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "machineUnreachable".into());
            // `Error(undefined).message` is "".
            return Err(refusal(&code, ready.error.unwrap_or_default()));
        }
        let context_tokens = self.machines.context_tokens(&t).await;
        let Some(token) = mint() else {
            return Err(refusal(
                "machineRelayOff",
                "The relay could not make a token for this task. The task was not started.".into(),
            ));
        };
        let hashed = hash(&token);
        let lease = Arc::new(Lease {
            task_id: task_id.to_string(),
            target: t.clone(),
            key,
            active: Mutex::new(0),
            cancel: CancellationToken::new(),
        });
        self.table.leases().insert(hashed.clone(), lease);
        let mut opened = event("relay.opened", task_id);
        opened.push(("machine".into(), JValue::Str(t.machine_id.clone())));
        self.table.record(JValue::Obj(opened));
        Ok(Opened {
            base_url,
            token,
            model: t.model.clone(),
            context_tokens,
            target: t,
            table: Arc::clone(&self.table),
            key: hashed,
            task_id: task_id.to_string(),
            closed: AtomicBool::new(false),
        })
    }

    fn lease_of(&self, authorization: Option<&str>) -> Option<Arc<Lease>> {
        let token = bearer(authorization?)?;
        self.table.get(&hash(token))
    }

    fn refused(&self, lease: &Lease, code: Option<&str>) {
        let mut e = event("relay.refused", &lease.task_id);
        if let Some(code) = code {
            e.push(("code".into(), JValue::Str(code.into())));
        }
        self.table.record(JValue::Obj(e));
    }

    /// The relay's request handler.
    pub async fn handle<B: BodySource>(&self, req: Request<B>) -> Reply<M::Upstream> {
        let path = req.url.split('?').next().unwrap_or("");
        let Some(lease) = self.lease_of(req.authorization.as_deref()) else {
            return error_reply(401, "No running Code task holds this token.", None);
        };
        if req.method == "GET" && (path == "/v1/models" || path == "/models") {
            let model = JValue::obj([
                ("id", JValue::Str(lease.target.model.clone())),
                ("object", JValue::Str("model".into())),
                ("owned_by", JValue::Str("noevia".into())),
            ]);
            return Reply::Json {
                status: 200,
                body: stringify(&JValue::obj([
                    ("object", JValue::Str("list".into())),
                    ("data", JValue::Arr(vec![model])),
                ])),
            };
        }
        if req.method != "POST" || (path != "/v1/chat/completions" && path != "/chat/completions") {
            return error_reply(404, "Only chat completions are relayed.", None);
        }
        let mut slot = {
            let Ok(mut n) = lease.active.lock() else {
                return error_reply(500, "The relay failed.", None);
            };
            if *n >= self.max_concurrent {
                return error_reply(429, "Too many model requests from this task at once.", None);
            }
            *n += 1;
            Slot {
                lease: Arc::clone(&lease),
                turn: None,
            }
        };
        let cancel = lease.cancel.clone();
        // From here a closed lease cuts the request, whatever phase it is in.
        macro_rules! or_cut {
            ($f:expr) => {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Reply::Cut,
                    v = $f => v,
                }
            };
        }
        let raw = match or_cut!(req.body.read(self.body_cap)) {
            Ok(raw) => raw,
            Err(BodyFail::TooLarge) => return error_reply(413, "The request is too large.", None),
            Err(BodyFail::Failed) => return error_reply(400, "The request is not JSON.", None),
            Err(BodyFail::TimedOut) => return Reply::Cut,
        };
        let Ok(payload) = wtf16::parse(&String::from_utf8_lossy(&raw)) else {
            return error_reply(400, "The request is not JSON.", None);
        };
        let JValue::Obj(mut items) = payload else {
            return error_reply(400, "The request is not a JSON object.", None);
        };
        let t = match self.current() {
            Some(t) if target_key(Some(&t)) == lease.key => t,
            _ => {
                self.refused(&lease, Some("model_mismatch"));
                return error_reply(
                    409,
                    "The Code role moved to another machine or model during this task; one task runs on one model.",
                    Some("model_mismatch"),
                );
            }
        };
        let ready = or_cut!(self.machines.ready(&t));
        if !ready.ok {
            self.refused(&lease, ready.code.as_deref());
            return error_body(502, ready.error.as_deref(), ready.code.as_deref());
        }
        // #1880: each forwarded request counts as a turn on the machine until its answer has been relayed.
        slot.turn = self.machines.begin_turn(&t.machine_id);
        // One task, one model: whatever the harness asked for, the machine's model is what is sent.
        wtf16::set(&mut items, "model", JValue::Str(t.model.clone()));
        let accept = match req.accept.as_deref() {
            Some(a) if !a.is_empty() => a,
            _ => "application/json",
        };
        let forward = Forward {
            url: format!("{}/v1/chat/completions", t.base_url),
            accept: js_json::js_slice(accept, 200),
            body: stringify(&JValue::Obj(items)),
        };
        let deadline = Instant::now() + self.request_timeout;
        let fetched = or_cut!(tokio::time::timeout_at(
            deadline,
            self.machines.fetch(forward)
        ));
        let upstream = match fetched {
            Ok(Ok(up)) => up,
            failed => {
                self.table
                    .record(JValue::Obj(event("relay.unreachable", &lease.task_id)));
                let text = match &failed {
                    Ok(Err(e)) => self.machines.connect_error(&t, Some(e)),
                    _ => self.machines.connect_error(&t, None),
                };
                return error_reply(502, &text, Some("machineUnreachable"));
            }
        };
        Reply::Stream(Relayed {
            status: upstream.status(),
            content_type: upstream
                .content_type()
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "application/json".into()),
            upstream: Some(upstream),
            cancel,
            deadline,
            _slot: slot,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
mod tests {
    use super::*;

    struct NoUpstream;

    impl Upstream for NoUpstream {
        fn status(&self) -> u16 {
            200
        }
        fn content_type(&self) -> Option<String> {
            None
        }
        async fn next(&mut self) -> Option<Result<Vec<u8>, ()>> {
            None
        }
    }

    /// A machine that is always ready and never reached (only the model list is asked).
    struct QaMachine;

    impl Machines for QaMachine {
        type Upstream = NoUpstream;
        type Error = ();
        async fn ready(&self, _target: &Target) -> Ready {
            Ready::ok()
        }
        async fn fetch(&self, _request: Forward) -> Result<NoUpstream, ()> {
            Err(())
        }
        fn connect_error(&self, _target: &Target, _error: Option<&()>) -> String {
            String::new()
        }
    }

    fn models(token: &str) -> Request<Vec<u8>> {
        Request {
            method: "GET".into(),
            url: "/v1/models".into(),
            authorization: Some(format!("Bearer {token}")),
            accept: None,
            body: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_poisoned_lease_table_still_opens_authenticates_and_revokes() {
        let t = Target {
            machine_id: "qa-m".into(),
            machine_name: "QA machine".into(),
            base_url: "http://qa-machine.invalid:8080".into(),
            model: "synthetic-coder".into(),
        };
        let relay = Relay::new(Options::new(
            Arc::new(QaMachine),
            Arc::new(move || Some(t.clone())),
            Some("http://qa-relay.invalid:8095/v1".into()),
        ));
        let table = Arc::clone(&relay.table);
        let _ = std::thread::spawn(move || {
            let _held = table.leases.lock().unwrap();
            panic!("QA: poison the lease table");
        })
        .join();
        assert!(relay.table.leases.is_poisoned());
        let lease = relay.open("qa-poison", None, None).await.unwrap();
        assert_eq!(relay.active(), 1);
        assert!(matches!(
            relay.handle(models(&lease.token)).await,
            Reply::Json { status: 200, .. }
        ));
        lease.close();
        assert_eq!(relay.active(), 0);
        assert!(matches!(
            relay.handle(models(&lease.token)).await,
            Reply::Json { status: 401, .. }
        ));
    }

    #[test]
    fn bearer_is_node_s_pattern() {
        assert_eq!(bearer("Bearer ncr_x"), Some("ncr_x"));
        assert_eq!(bearer("bearer \t ncr_x"), Some("ncr_x"));
        assert_eq!(bearer("BEARER\u{a0}ncr_x"), Some("ncr_x"));
        for no in [
            "Bearer",
            "Bearer ",
            "Bearerncr_x",
            "Bearer ncr x",
            "Bearer ncr_x ",
            "Basic ncr_x",
            "",
            "Bear",
            " Bearer ncr_x",
        ] {
            assert_eq!(bearer(no), None, "{no:?}");
        }
    }

    #[test]
    fn the_target_key_is_machine_engine_and_model() {
        let t = Target {
            machine_id: "qa-m".into(),
            machine_name: "QA machine".into(),
            base_url: "http://qa.invalid:8080".into(),
            model: "qa-code".into(),
        };
        assert_eq!(
            target_key(Some(&t)).as_deref(),
            Some("qa-m|http://qa.invalid:8080|qa-code")
        );
        assert_eq!(target_key(None), None);
    }

    #[test]
    fn a_token_is_ncr_and_32_base64url_characters() {
        let a = mint().unwrap_or_default();
        let b = mint().unwrap_or_default();
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(a.len(), TOKEN_PREFIX.len() + 32);
        assert!(a[4..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }

    #[test]
    fn json_errors_are_node_s_bodies() {
        let body = |r: Reply<std::convert::Infallible>| match r {
            Reply::Json { status, body } => (status, body),
            _ => (0, String::new()),
        };
        assert_eq!(
            body(json_error(401, "No running Code task holds this token.", None)),
            (
                401,
                r#"{"error":{"message":"No running Code task holds this token.","type":"relay_error"}}"#
                    .to_string()
            )
        );
        assert_eq!(
            body(json_error(409, "x", Some("model_mismatch"))),
            (
                409,
                r#"{"error":{"message":"x","type":"relay_error","code":"model_mismatch"}}"#
                    .to_string()
            )
        );
        // `...(code ? { code } : {})`: an empty code is no code.
        assert_eq!(
            body(json_error(502, "x", Some(""))).1,
            r#"{"error":{"message":"x","type":"relay_error"}}"#
        );
    }
}
