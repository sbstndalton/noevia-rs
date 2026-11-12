//! The control client: the authenticated session with the proxy and the grants issued under it.
//! A port of the oracle's `createControlClient` (see the crate header).
//!
//! `epoch` is the only network authority. A failure drops it and only an authenticated Ready for a
//! different session restores it: the proxy refuses that Ready until the old session is dead
//! (expired, retired or lost with its process), so an uncertain revoke cannot survive recovery.
//! Only a clock fault is final. No lock is held across an await; every flight (Ready, Renew,
//! cleanup, Forget) runs on its own task so a caller that goes away never leaves one half done.
use crate::files::KeyKeeper;
use crate::transport::{BoxFuture, Transport, TransportRequest};
use crate::wire::{
    check_scope, closed_keys, field, hex, is_hex, number, report_value, response_mac_header,
    safe_int, safe_int_value, str_of, unhex, valid_task, Report, Scope, MAX_REQUEST, MAX_RESPONSE,
};
use crate::{err, ControlError, Reason, CAPABILITY};
use egress::control::{derive_epoch_keys, ControlKey, FileKey, RequestMetadata, Route};
use egress::{b64url_encode, mint, Act, GrantKey, SignedGrant};
use futures_util::future::{FutureExt, Shared};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;
use tokio::task::JoinHandle;

const SESSION_MS: f64 = 15000.0;
const RENEW_MS: f64 = 5000.0;
const RPC_MS: f64 = 10000.0;
const MAX_HANDLES: usize = 256;
const MAX_RECORDS: usize = 4096;
const MAX_STAMPS: usize = 10_000;
const MAX_ATTEMPTS: u32 = 16;
pub const DAY_MS: u64 = 86_400_000;

/// Time and randomness, injected so tests and the oracle replay are deterministic.
pub trait Clock: Send + Sync + 'static {
    fn wall_ms(&self) -> u64;
    fn mono_ms(&self) -> f64;
    /// The protocol stream: challenges, request ids, grant nonces and key nonces.
    fn fill(&self, buf: &mut [u8]);
    fn sleep(&self, ms: u64) -> BoxFuture<()>;
}

pub struct SystemClock {
    start: Instant,
}
impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}
impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}
impl Clock for SystemClock {
    fn wall_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
    fn mono_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
    fn fill(&self, buf: &mut [u8]) {
        use rand_core::{OsRng, RngCore};
        OsRng.fill_bytes(buf);
    }
    fn sleep(&self, ms: u64) -> BoxFuture<()> {
        Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
    }
}

pub struct ClientConfig {
    /// `host:port` of the proxy's control listener.
    pub endpoint: String,
    /// The idle TTL a grant carries (`CODE_EGRESS_TOKEN_TTL_MS`).
    pub ttl_ms: u64,
    /// The absolute grant lifetime (`CODE_EGRESS_GRANT_LIFETIME_MS`), capped at 24 h.
    pub lifetime_ms: u64,
    pub auto_renew: bool,
    pub retry_min_ms: f64,
    pub retry_max_ms: f64,
}

impl ClientConfig {
    pub fn new(endpoint: impl Into<String>, ttl_ms: u64, lifetime_ms: u64) -> Self {
        Self {
            endpoint: endpoint.into(),
            ttl_ms,
            lifetime_ms,
            auto_renew: true,
            retry_min_ms: 1000.0,
            retry_max_ms: 15000.0,
        }
    }
}

/// A diagnostics event (`egress.*`); never carries a secret.
pub type Log = Arc<dyn Fn(&'static str, &[(&'static str, String)]) + Send + Sync>;

type Flight<T> = Shared<BoxFuture<Result<T, ControlError>>>;

#[derive(Clone)]
struct Epoch {
    boot: String,
    session: String,
    sequence: u64,
    until: f64,
}

struct RecordState {
    cleanup: Option<Flight<Revoked>>,
    report: Option<Report>,
    ackable: Option<String>,
    acknowledged: bool,
    forget: Option<Flight<bool>>,
}

struct Record {
    id: String,
    task: String,
    nonce: String,
    iat: u64,
    token: String,
    key: GrantKey,
    boot: String,
    session: String,
    state: Mutex<RecordState>,
}

/// What a caller gets from a grant. The token is the only secret and goes to the task.
#[derive(Clone)]
pub struct Handle {
    pub id: String,
    pub task_id: String,
    pub domains: Vec<String>,
    pub token: String,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("task_id", &self.task_id)
            .finish_non_exhaustive()
    }
}

/// The result of a revoke: `confirmed` is true only when the native barrier and final report agree.
#[derive(Clone, Debug)]
pub struct Revoked {
    pub confirmed: bool,
    pub report: Report,
}

struct Stamps {
    by_task: HashMap<String, (u64, u64)>,
    order: BTreeMap<u64, String>,
    seq: u64,
}

struct State {
    file: [u8; 32],
    fingerprint: String,
    conflict_since: Option<f64>,
    /// The one immediate re-key for a refused Initial with no session to wait for has been used.
    unnamed_rekeyed: bool,
    superseded_noted: bool,
    records: HashMap<String, Arc<Record>>,
    record_order: VecDeque<String>,
    active: HashSet<String>,
    stamps: Stamps,
    epoch: Option<Epoch>,
    last_epoch: Option<(String, String)>,
    ready_flight: Option<Flight<()>>,
    renew_flight: Option<Flight<()>>,
    timer: Option<JoinHandle<()>>,
    retry_timer: Option<JoinHandle<()>>,
    failed: bool,
    closed: bool,
    clock_fault: bool,
    last_mono: f64,
    next_attempt: f64,
    retry_delay: f64,
    down_since: Option<f64>,
}

impl State {
    fn stop_timers(&mut self) {
        if let Some(t) = self.timer.take() {
            t.abort();
        }
        if let Some(t) = self.retry_timer.take() {
            t.abort();
        }
    }
    fn base(&self) -> f64 {
        if self.last_mono.is_finite() {
            self.last_mono
        } else {
            0.0
        }
    }
}

struct Inner {
    cfg: ClientConfig,
    life: u64,
    idle: u64,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn Transport>,
    keeper: Option<Mutex<KeyKeeper>>,
    log: Log,
    incarnation: String,
    st: Mutex<State>,
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Client(<redacted>)")
    }
}

struct RpcReply {
    status: u16,
    value: Value,
    first: f64,
}

struct Sent {
    reply: RpcReply,
    challenge: String,
}

/// Clears a flight slot when its task ends, however it ends.
struct ClearFlight {
    inner: Weak<Inner>,
    which: Which,
}
#[derive(Clone, Copy)]
enum Which {
    Ready,
    Renew,
}
impl Drop for ClearFlight {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut s = inner.lock();
            match self.which {
                Which::Ready => s.ready_flight = None,
                Which::Renew => s.renew_flight = None,
            }
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn note(&self, event: &'static str, fields: &[(&'static str, String)]) {
        let log = Arc::clone(&self.log);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log(event, fields)));
    }
    fn fill_hex<const N: usize>(&self) -> String {
        let mut buf = [0u8; N];
        self.clock.fill(&mut buf);
        hex(&buf)
    }
    fn mono_in(&self, s: &mut State) -> Result<f64, ControlError> {
        let value = self.clock.mono_ms();
        if !value.is_finite() || value < s.last_mono {
            s.failed = true;
            s.clock_fault = true;
            s.epoch = None;
            s.stop_timers();
            return err(Reason::Clock);
        }
        s.last_mono = value;
        Ok(value)
    }
    fn mono(&self) -> Result<f64, ControlError> {
        let mut s = self.lock();
        self.mono_in(&mut s)
    }
    fn wall(&self) -> Result<u64, ControlError> {
        safe_int(self.clock.wall_ms())
    }
    fn stamp(&self, task: &str) -> Result<u64, ControlError> {
        let wall = self.wall()?;
        let mut s = self.lock();
        let stamps = &mut s.stamps;
        let prior = stamps.by_task.get(task).map_or(0, |p| p.0);
        let t = wall.max(prior + 1);
        if t - wall > 60_000 {
            return err(Reason::Clock);
        }
        if let Some((_, old_seq)) = stamps.by_task.remove(task) {
            stamps.order.remove(&old_seq);
        }
        stamps.seq += 1;
        let seq = stamps.seq;
        stamps.by_task.insert(task.to_string(), (t, seq));
        stamps.order.insert(seq, task.to_string());
        if stamps.by_task.len() > MAX_STAMPS {
            if let Some((&oldest, _)) = stamps.order.iter().next() {
                if let Some(name) = stamps.order.remove(&oldest) {
                    stamps.by_task.remove(&name);
                }
            }
        }
        Ok(t)
    }
}

fn json_str(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

impl Client {
    /// `keeper` is the key keeper (restore/rekey); `None` for a client that cannot republish.
    pub fn new(
        config: ClientConfig,
        file_key: [u8; 32],
        keeper: Option<KeyKeeper>,
        persisted_epoch: Option<(String, String)>,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn Transport>,
        log: Log,
    ) -> Result<Self, ControlError> {
        crate::wire::endpoint_address(&config.endpoint)?;
        if config.ttl_ms == 0
            || config.lifetime_ms == 0
            || config.ttl_ms > crate::wire::MAX_SAFE_INTEGER as u64
            || config.lifetime_ms > crate::wire::MAX_SAFE_INTEGER as u64
        {
            return err(Reason::Integer);
        }
        let life = config.lifetime_ms.min(DAY_MS);
        let idle = config.ttl_ms.min(life);
        let mut incarnation = [0u8; 8];
        {
            use rand_core::{OsRng, RngCore};
            OsRng.fill_bytes(&mut incarnation);
        }
        let fingerprint = FileKey::from_bytes(file_key).fingerprint();
        let retry_delay = config.retry_min_ms;
        let inner = Inner {
            cfg: config,
            life,
            idle,
            clock,
            transport,
            keeper: keeper.map(Mutex::new),
            log,
            incarnation: hex(&incarnation),
            st: Mutex::new(State {
                file: file_key,
                fingerprint,
                conflict_since: None,
                unnamed_rekeyed: false,
                superseded_noted: false,
                records: HashMap::new(),
                record_order: VecDeque::new(),
                active: HashSet::new(),
                stamps: Stamps {
                    by_task: HashMap::new(),
                    order: BTreeMap::new(),
                    seq: 0,
                },
                epoch: None,
                last_epoch: persisted_epoch,
                ready_flight: None,
                renew_flight: None,
                timer: None,
                retry_timer: None,
                failed: false,
                closed: false,
                clock_fault: false,
                last_mono: f64::NEG_INFINITY,
                next_attempt: f64::NEG_INFINITY,
                retry_delay,
                down_since: None,
            }),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.inner.cfg.endpoint
    }

    /// Boot without waiting: an outage is logged, retried and refused per grant, never fatal.
    pub fn start(&self) {
        let me = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let _ = me.ready().await;
        });
    }

    pub async fn ready(&self) -> Result<(), ControlError> {
        self.inner.ready().await
    }

    pub async fn renew(&self) -> Result<(), ControlError> {
        self.inner.renew().await
    }

    pub fn available(&self) -> bool {
        let mut s = self.inner.lock();
        let Ok(now) = self.inner.mono_in(&mut s) else {
            return false;
        };
        !s.closed && !s.failed && s.epoch.as_ref().is_some_and(|e| now < e.until)
    }

    pub async fn grant(&self, task_id: &str, domains: &[String]) -> Result<Handle, ControlError> {
        self.inner.grant(task_id, domains).await
    }

    pub async fn activity(
        &self,
        handle: &str,
        budget_ms: Option<f64>,
    ) -> Result<Report, ControlError> {
        let record = self.inner.record_for(handle)?;
        self.inner
            .activity(&record, budget_ms.unwrap_or(RPC_MS))
            .await
    }

    pub async fn revoke(
        &self,
        handle: &str,
        budget_ms: Option<f64>,
    ) -> Result<Revoked, ControlError> {
        let record = self.inner.record_for(handle)?;
        let flight = self
            .inner
            .revoke_flight(&record, budget_ms.unwrap_or(RPC_MS));
        flight.await
    }

    pub async fn forget(&self, handle: &str, snapshot_id: &str) -> Result<bool, ControlError> {
        let record = self.inner.record_for(handle)?;
        let flight = self.inner.forget_flight(&record, snapshot_id)?;
        flight.await
    }

    pub async fn close(&self) -> Vec<Revoked> {
        self.inner.close().await
    }
}

impl Inner {
    fn record_for(&self, handle: &str) -> Result<Arc<Record>, ControlError> {
        self.lock()
            .records
            .get(handle)
            .cloned()
            .ok_or(ControlError(Reason::Handle))
    }

    // ---- timers ----

    fn arm_retry_in(self: &Arc<Self>, s: &mut State) {
        if let Some(t) = s.retry_timer.take() {
            t.abort();
        }
        if !self.cfg.auto_renew || s.closed || s.clock_fault || !s.failed {
            return;
        }
        let delay = self
            .cfg
            .retry_min_ms
            .max(s.next_attempt - s.base())
            .max(0.0);
        let weak = Arc::downgrade(self);
        let clock = Arc::clone(&self.clock);
        s.retry_timer = Some(tokio::spawn(async move {
            clock.sleep(delay.ceil() as u64).await;
            let Some(me) = weak.upgrade() else { return };
            me.lock().retry_timer = None;
            if me.ready().await.is_err() {
                let mut s = me.lock();
                if s.retry_timer.is_none() {
                    me.arm_retry_in(&mut s);
                }
            }
        }));
    }

    fn schedule_in(self: &Arc<Self>, s: &mut State) {
        if let Some(t) = s.timer.take() {
            t.abort();
        }
        if self.cfg.auto_renew && !s.closed && !s.failed {
            let weak = Arc::downgrade(self);
            let clock = Arc::clone(&self.clock);
            s.timer = Some(tokio::spawn(async move {
                clock.sleep(RENEW_MS as u64).await;
                if let Some(me) = weak.upgrade() {
                    let _ = me.renew().await;
                }
            }));
        }
    }

    fn fail_in(self: &Arc<Self>, s: &mut State, reason: &'static str, hold_until: f64) {
        if !s.failed {
            s.down_since = Some(s.base());
            self.note(
                "egress.control_unavailable",
                &[("reason", reason.to_string())],
            );
        }
        s.failed = true;
        if let Some(e) = s.epoch.take() {
            s.last_epoch = Some((e.boot, e.session));
        }
        if let Some(t) = s.timer.take() {
            t.abort();
        }
        s.next_attempt = (s.base() + s.retry_delay).max(hold_until);
        s.retry_delay = (s.retry_delay * 2.0).min(self.cfg.retry_max_ms);
        self.arm_retry_in(s);
    }

    fn fail(self: &Arc<Self>, reason: &'static str, hold_until: f64) {
        let mut s = self.lock();
        self.fail_in(&mut s, reason, hold_until);
    }

    // ---- one authenticated exchange ----

    async fn rpc(
        self: &Arc<Self>,
        route: Route,
        body: String,
        boot: Option<&str>,
        budget_ms: f64,
    ) -> Result<RpcReply, ControlError> {
        if !budget_ms.is_finite() || budget_ms <= 0.0 {
            return err(Reason::Timeout);
        }
        let first = self.mono()?;
        let until = first + budget_ms.min(RPC_MS);
        let request_id = self.fill_hex::<16>();
        let bytes = body.into_bytes();
        if bytes.len() > MAX_REQUEST {
            return err(Reason::Request);
        }
        let (file, fingerprint) = {
            let s = self.lock();
            (s.file, s.fingerprint.clone())
        };
        let control = egress::control::derive_control_key(&FileKey::from_bytes(file))
            .map_err(|_| ControlError(Reason::Key))?;
        let deadline = self.wall()? + (until - first).ceil() as u64;
        let meta = RequestMetadata {
            request_id: &request_id,
            deadline_ms: deadline,
            boot_id: boot,
            key_fingerprint: &fingerprint,
        };
        let tag = control
            .sign_request(route, meta, &bytes)
            .map_err(|_| ControlError(Reason::Request))?;
        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("content-length".to_string(), bytes.len().to_string()),
            ("connection".to_string(), "close".to_string()),
            ("x-noevia-request-id".to_string(), request_id.clone()),
            ("x-noevia-deadline-ms".to_string(), deadline.to_string()),
            ("x-noevia-key-fingerprint".to_string(), fingerprint.clone()),
            ("x-noevia-control-mac".to_string(), tag.clone()),
        ];
        if let Some(b) = boot {
            headers.push(("x-noevia-boot-id".to_string(), b.to_string()));
        }
        for _ in 0..MAX_ATTEMPTS {
            if route == Route::Ready && self.lock().closed {
                return err(Reason::Closed);
            }
            let left = until - self.mono()?;
            if left <= 0.0 {
                return err(Reason::Timeout);
            }
            let attempt = async {
                let request = TransportRequest {
                    endpoint: self.cfg.endpoint.clone(),
                    route: route.path(),
                    headers: headers.clone(),
                    body: bytes.clone(),
                    timeout_ms: left.min(7500.0),
                };
                let reply = tokio::select! {
                    biased;
                    done = self.transport.post(request) => done,
                    () = self.clock.sleep(left.ceil() as u64) => err(Reason::Timeout),
                }?;
                if self.mono()? >= until || self.wall()? >= deadline {
                    return err(Reason::Stale);
                }
                if reply.body.len() > MAX_RESPONSE {
                    return err(Reason::Response);
                }
                let mac = response_mac_header(&reply.raw_headers, &reply.body)?;
                let expected = control
                    .sign_response(&tag, reply.status, &reply.body)
                    .map_err(|_| ControlError(Reason::Response))?;
                if !equal_tag(&mac, &expected) {
                    return err(Reason::Authentication);
                }
                let text =
                    std::str::from_utf8(&reply.body).map_err(|_| ControlError(Reason::Response))?;
                let value: Value =
                    serde_json::from_str(text).map_err(|_| ControlError(Reason::Response))?;
                if number(field(&value, "version")) != Some(1.0)
                    || str_of(&value, "requestId") != Some(request_id.as_str())
                    || !value.get("state").is_some_and(Value::is_string)
                {
                    return err(Reason::Identity);
                }
                Ok((reply.status, value))
            }
            .await;
            match attempt {
                // An exact in-flight retry, never a new mutation identity.
                Ok((202, _)) => {}
                Ok((status, value)) => {
                    return Ok(RpcReply {
                        status,
                        value,
                        first,
                    })
                }
                Err(e) if !matches!(e.0, Reason::Transport | Reason::Timeout) => return Err(e),
                Err(_) => {}
            }
            let rest = until - self.mono()?;
            if rest <= 0.0 {
                break;
            }
            self.clock.sleep(rest.min(250.0).ceil() as u64).await;
        }
        err(Reason::Uncertain)
    }

    fn proof(
        &self,
        reply: &RpcReply,
        state: &str,
        challenge: Option<&str>,
        sequence: u64,
    ) -> Result<Epoch, ControlError> {
        let v = &reply.value;
        closed_keys(
            v,
            &[
                "version",
                "requestId",
                "state",
                "capability",
                "bootId",
                "sessionId",
                "keyFingerprint",
                "sequence",
                "leaseMs",
            ],
            if challenge.is_some() {
                &["challenge"]
            } else {
                &[]
            },
        )?;
        let boot = str_of(v, "bootId")
            .filter(|b| is_hex(b, 16))
            .ok_or(ControlError(Reason::Identity))?;
        let session = str_of(v, "sessionId")
            .filter(|b| is_hex(b, 16))
            .ok_or(ControlError(Reason::Identity))?;
        let seq = safe_int_value(field(v, "sequence"), false)?;
        let lease = safe_int_value(field(v, "leaseMs"), true)?;
        let mono = self.mono()?;
        let mut s = self.lock();
        let challenge_ok = challenge.is_none_or(|c| str_of(v, "challenge") == Some(c));
        if reply.status != 200
            || str_of(v, "state") != Some(state)
            || str_of(v, "capability") != Some(CAPABILITY)
            || str_of(v, "keyFingerprint") != Some(s.fingerprint.as_str())
            || seq != sequence
            || lease as f64 > SESSION_MS
            || !challenge_ok
            || reply.first + lease as f64 <= mono
        {
            return err(Reason::Proof);
        }
        if let Some(e) = &s.epoch {
            if boot != e.boot || session != e.session {
                return err(Reason::Epoch);
            }
        }
        let _ = &mut s;
        Ok(Epoch {
            boot: boot.into(),
            session: session.into(),
            sequence: seq,
            until: reply.first + lease as f64,
        })
    }

    // ---- Ready ----

    async fn ready(self: &Arc<Self>) -> Result<(), ControlError> {
        let flight = {
            let mut s = self.lock();
            if s.closed || s.clock_fault {
                return err(Reason::Unavailable);
            }
            if let Some(until) = s.epoch.as_ref().map(|e| e.until) {
                let now = self.mono_in(&mut s)?;
                if now < until {
                    return Ok(());
                }
                self.fail_in(&mut s, "expired", f64::NEG_INFINITY);
            }
            if let Some(f) = &s.ready_flight {
                f.clone()
            } else {
                // Inside the backoff a caller is refused at once, not parked behind a proxy that just failed.
                if s.failed {
                    let now = self.mono_in(&mut s)?;
                    if now < s.next_attempt {
                        return err(Reason::Unavailable);
                    }
                }
                let me = Arc::clone(self);
                let task = tokio::spawn(async move {
                    let _clear = ClearFlight {
                        inner: Arc::downgrade(&me),
                        which: Which::Ready,
                    };
                    match me.establish().await {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            let conflict = e.0 == Reason::Conflict;
                            let (closed, fault) = {
                                let mut s = me.lock();
                                if !conflict {
                                    s.conflict_since = None; // only an unbroken run of signed refusals counts
                                }
                                (s.closed, s.clock_fault)
                            };
                            if !closed && !fault {
                                me.fail(
                                    if conflict { "conflict" } else { "ready" },
                                    f64::NEG_INFINITY,
                                );
                            }
                            Err(ControlError(if e.0 == Reason::Conflict {
                                Reason::Conflict
                            } else {
                                e.0
                            }))
                        }
                    }
                });
                let flight: Flight<()> = (Box::pin(async move {
                    task.await.unwrap_or(Err(ControlError(Reason::Ready)))
                })
                    as BoxFuture<Result<(), ControlError>>)
                    .shared();
                s.ready_flight = Some(flight.clone());
                flight
            }
        };
        flight.await
    }

    async fn ready_request(
        self: &Arc<Self>,
        mode: &str,
        prior: Option<&(String, String)>,
    ) -> Result<Sent, ControlError> {
        let challenge = self.fill_hex::<16>();
        let fingerprint = self.lock().fingerprint.clone();
        let body = format!(
            "{{\"version\":1,\"mode\":{},\"challenge\":{},\"expectedKeyFingerprint\":{},\"priorSession\":{}}}",
            json_str(mode),
            json_str(&challenge),
            json_str(&fingerprint),
            prior.map_or("null".to_string(), |p| json_str(&p.1))
        );
        let reply = self
            .rpc(Route::Ready, body, prior.map(|p| p.0.as_str()), RPC_MS)
            .await?;
        Ok(Sent { reply, challenge })
    }

    // Initial refused with a signed answer: the proxy is already initialized under this key, by a session
    // nobody here can name. The proxy accepts Initial again only after a key change (session expiry does
    // not reset it), so nothing else clears that until a new key does. The proxy reads the key file on
    // every request and a new key retires its session.
    //
    // `unnamed`: this client had no session of its own to offer (no Replace was tried). Then no wait can
    // help: Replace needs a session id this client does not have. This is the takeover of a key the
    // retired Node issuer published and already did its Ready under (an adopted key has no epoch), or a
    // start whose epoch was never saved. The first such refusal re-keys at once, without the wait and
    // without a conflict event. Only one immediate re-key per client: a refusal that persists after it
    // falls back to the wait below, so a proxy that never accepts can't make it rotate on every retry.
    //
    // Otherwise Replace was refused too. The wait outlasts any session this client could still renew,
    // so a live one is never cut short. Replace succeeds by itself once that session has expired.
    async fn conflicted(self: &Arc<Self>, unnamed: bool) -> Result<(), ControlError> {
        let at = self.mono()?;
        let immediate = {
            let mut s = self.lock();
            if unnamed && !s.unnamed_rekeyed {
                s.unnamed_rekeyed = true;
                true
            } else {
                if s.conflict_since.is_none() {
                    s.conflict_since = Some(at);
                    self.note("egress.control_conflict", &[]);
                }
                if at - s.conflict_since.unwrap_or(at) < SESSION_MS + RPC_MS {
                    return err(Reason::Conflict);
                }
                false
            }
        };
        // `rekey` writes only over this process's own key (never a superseded or foreign one), with the
        // published contract: owner and group from the configuration, mode 0440, one link, replaced by
        // rename in the same directory.
        let next = self
            .keeper
            .as_ref()
            .and_then(|k| k.lock().unwrap_or_else(|e| e.into_inner()).rekey());
        let Some(next) = next else {
            self.note("egress.control_rekey_refused", &[]);
            return err(Reason::Conflict);
        };
        let mut s = self.lock();
        s.file = next;
        s.fingerprint = FileKey::from_bytes(next).fingerprint();
        s.last_epoch = None;
        s.conflict_since = None;
        if immediate {
            self.note(
                "egress.control_rekeyed",
                &[("cause", "unnamed_session".to_string())],
            );
        } else {
            self.note("egress.control_rekeyed", &[]);
        }
        Ok(())
    }

    async fn establish(self: &Arc<Self>) -> Result<(), ControlError> {
        let failed_before = self.lock().failed;
        if failed_before {
            if let Some(k) = &self.keeper {
                let kept = k.lock().unwrap_or_else(|e| e.into_inner()).restore();
                if kept == "restored" {
                    self.note("egress.key_republished", &[]);
                }
                if kept == "superseded" {
                    let mut s = self.lock();
                    if !s.superseded_noted {
                        s.superseded_noted = true;
                        drop(s);
                        self.note("egress.key_superseded", &[]);
                    }
                }
            }
        }
        let mut prior = self.lock().last_epoch.clone();
        let unnamed = prior.is_none();
        let mut sent: Option<Sent> = None;
        // Replace answers a live proxy whose session this client held. A restarted or re-keyed proxy has
        // no such session and answers 409, signed; it accepts only Initial, which an initialized proxy refuses.
        if let Some(p) = &prior {
            let s = self.ready_request("replace", Some(p)).await?;
            if s.reply.status != 409 {
                sent = Some(s);
            }
        }
        if self.lock().closed {
            return err(Reason::Closed);
        }
        let sent = match sent {
            Some(s) => s,
            None => {
                let mut s = self.ready_request("initial", None).await?;
                if s.reply.status == 409 {
                    self.conflicted(unnamed).await?;
                    prior = None;
                    s = self.ready_request("initial", None).await?;
                }
                s
            }
        };
        self.lock().conflict_since = None;
        let next = self.proof(&sent.reply, "ready", Some(&sent.challenge), 0)?;
        {
            let s = self.lock();
            if s.closed || s.clock_fault || prior.as_ref().is_some_and(|p| next.session == p.1) {
                return err(Reason::Epoch);
            }
        }
        let identity = (next.boot.clone(), next.session.clone());
        {
            let mut s = self.lock();
            let recovered = s.failed;
            let down_ms = if recovered {
                s.down_since
                    .map_or(0.0, |d| (s.last_mono - d).max(0.0).round())
            } else {
                0.0
            };
            s.epoch = Some(next);
            s.last_epoch = None;
            s.failed = false;
            s.down_since = None;
            s.retry_delay = self.cfg.retry_min_ms;
            s.next_attempt = f64::NEG_INFINITY;
            if let Some(t) = s.retry_timer.take() {
                t.abort();
            }
            // Tokens of a replaced session verify nowhere now. Their owners can still ask for a report.
            let keep: Vec<String> = s
                .active
                .iter()
                .filter(|id| s.records.get(*id).is_some_and(|r| r.session == identity.1))
                .cloned()
                .collect();
            s.active = keep.into_iter().collect();
            let mut fields = vec![("recovered", recovered.to_string())];
            if recovered {
                fields.push(("downMs", format!("{down_ms}")));
            }
            self.note("egress.control_ready", &fields);
            self.schedule_in(&mut s);
        }
        if let Some(k) = &self.keeper {
            k.lock()
                .unwrap_or_else(|e| e.into_inner())
                .save_epoch(Some(identity));
        }
        Ok(())
    }

    // ---- Renew ----

    async fn renew(self: &Arc<Self>) -> Result<(), ControlError> {
        let flight = {
            let mut s = self.lock();
            if let Some(f) = &s.renew_flight {
                f.clone()
            } else {
                let me = Arc::clone(self);
                let task = tokio::spawn(async move {
                    let _clear = ClearFlight {
                        inner: Arc::downgrade(&me),
                        which: Which::Renew,
                    };
                    me.renew_once().await
                });
                let flight: Flight<()> = (Box::pin(async move {
                    task.await.unwrap_or(Err(ControlError(Reason::Renew)))
                })
                    as BoxFuture<Result<(), ControlError>>)
                    .shared();
                s.renew_flight = Some(flight.clone());
                flight
            }
        };
        flight.await
    }

    async fn renew_once(self: &Arc<Self>) -> Result<(), ControlError> {
        self.ready().await?;
        let prior = {
            let s = self.lock();
            match &s.epoch {
                Some(e)
                    if e.sequence
                        .checked_add(1)
                        .is_some_and(|n| n as f64 <= crate::wire::MAX_SAFE_INTEGER) =>
                {
                    e.clone()
                }
                _ => return err(Reason::Epoch),
            }
        };
        let outcome = async {
            let budget = (prior.until - self.mono()?).min(1000.0);
            let body = format!(
                "{{\"version\":1,\"sessionId\":{},\"sequence\":{}}}",
                json_str(&prior.session),
                prior.sequence + 1
            );
            let reply = self
                .rpc(Route::Renew, body, Some(&prior.boot), budget)
                .await?;
            let next = self.proof(&reply, "renewed", None, prior.sequence + 1)?;
            let mut s = self.lock();
            let same = s
                .epoch
                .as_ref()
                .is_some_and(|e| e.session == prior.session && e.sequence == prior.sequence);
            if s.closed || !same {
                return err(Reason::Closed);
            }
            s.epoch = Some(next);
            self.schedule_in(&mut s);
            Ok(())
        }
        .await;
        if let Err(e) = outcome {
            let same = self
                .lock()
                .epoch
                .as_ref()
                .is_some_and(|e| e.session == prior.session && e.sequence == prior.sequence);
            if same {
                self.fail("renew", f64::NEG_INFINITY);
            }
            return Err(e);
        }
        Ok(())
    }

    // ---- grants ----

    async fn grant(
        self: &Arc<Self>,
        task_id: &str,
        domains: &[String],
    ) -> Result<Handle, ControlError> {
        if !valid_task(task_id) {
            return err(Reason::Task);
        }
        let hosts = grant_hosts(domains).map_err(|_| ControlError(Reason::Hosts))?;
        if hosts.is_empty() {
            return err(Reason::Hosts);
        }
        self.ready().await?;
        let renewing = self.lock().renew_flight.clone();
        if let Some(f) = renewing {
            f.await?;
        }
        let (file, boot, session) = {
            let mut s = self.lock();
            let now = self.mono_in(&mut s)?;
            match &s.epoch {
                Some(e)
                    if !s.closed && !s.failed && now < e.until && s.active.len() < MAX_HANDLES =>
                {
                    (s.file, e.boot.clone(), e.session.clone())
                }
                _ => return err(Reason::Unavailable),
            }
        };
        let iat = self.stamp(task_id)?;
        let mut nonce_bytes = [0u8; 16];
        self.clock.fill(&mut nonce_bytes);
        let nonce = b64url_encode(&nonce_bytes);
        let exp = safe_int(iat + self.life)?;
        let boot_bytes = unhex::<16>(&boot).ok_or(ControlError(Reason::Identity))?;
        let session_bytes = unhex::<16>(&session).ok_or(ControlError(Reason::Identity))?;
        let keys = derive_epoch_keys(&FileKey::from_bytes(file), &boot_bytes, &session_bytes)
            .map_err(|_| ControlError(Reason::Key))?;
        let token = mint(
            &keys.grant,
            &SignedGrant {
                act: Act::Grant,
                task: task_id.into(),
                hosts: hosts.clone(),
                iat,
                exp,
                idle: self.idle,
                nonce: nonce.clone(),
            },
        )
        .ok_or(ControlError(Reason::Request))?;
        let id = {
            use rand_core::{OsRng, RngCore};
            let mut raw = [0u8; 12];
            OsRng.fill_bytes(&mut raw);
            format!("{}-{}", self.incarnation, hex(&raw))
        };
        let record = Arc::new(Record {
            id: id.clone(),
            task: task_id.into(),
            nonce,
            iat,
            token: token.clone(),
            key: keys.grant,
            boot,
            session,
            state: Mutex::new(RecordState {
                cleanup: None,
                report: None,
                ackable: None,
                acknowledged: false,
                forget: None,
            }),
        });
        let mut s = self.lock();
        if s.closed
            || s.failed
            || s.epoch.as_ref().is_none_or(|e| e.session != record.session)
            || s.active.len() >= MAX_HANDLES
        {
            return err(Reason::Unavailable);
        }
        s.active.insert(id.clone());
        s.records.insert(id.clone(), Arc::clone(&record));
        s.record_order.push_back(id.clone());
        while s.records.len() > MAX_RECORDS {
            let Some(pos) = s.record_order.iter().position(|r| !s.active.contains(r)) else {
                break;
            };
            if let Some(old) = s.record_order.remove(pos) {
                s.records.remove(&old);
            }
        }
        Ok(Handle {
            id,
            task_id: task_id.into(),
            domains: hosts,
            token,
        })
    }

    fn lease_body(r: &Record) -> String {
        format!(
            "{{\"version\":1,\"sessionId\":{},\"taskId\":{},\"leaseNonce\":{},\"signedGrant\":{}",
            json_str(&r.session),
            json_str(&r.task),
            json_str(&r.nonce),
            json_str(&r.token)
        )
    }

    fn scope_of(r: &Record) -> Scope<'_> {
        Scope {
            boot: &r.boot,
            session: &r.session,
            task: &r.task,
            nonce: &r.nonce,
        }
    }

    // Response arrival order is not native snapshot order. Once delivered, a frozen final identity
    // remains acknowledgeable across late active reads.
    fn retain_report(r: &Record, report: &Report) -> Result<(), ControlError> {
        let mut st = r.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ackable) = st.ackable.clone() {
            if !report.final_ || report.snapshot_id.is_none() || st.acknowledged {
                return Ok(());
            }
            let prior = st.report.as_ref().map(|p| p.state.clone());
            if prior.as_deref() == Some("forgotten") {
                return Ok(());
            }
            if prior.as_deref() == Some("evicted")
                && !["evicted", "forgotten"].contains(&report.state.as_str())
            {
                return Ok(());
            }
            // FIFO eviction drops the held frozen body and creates a new, unavailable final snapshot.
            let evicted = matches!(
                prior.as_deref(),
                Some("complete" | "partial" | "interrupted")
            ) && report.state == "evicted";
            if report.snapshot_id.as_deref() != Some(ackable.as_str()) && !evicted {
                return err(Reason::Snapshot);
            }
        }
        if report.final_ {
            if let Some(id) = &report.snapshot_id {
                st.ackable = Some(id.clone());
            }
        }
        st.report = Some(report.clone());
        Ok(())
    }

    async fn activity(
        self: &Arc<Self>,
        r: &Arc<Record>,
        budget_ms: f64,
    ) -> Result<Report, ControlError> {
        let body = format!("{}}}", Self::lease_body(r));
        let reply = self
            .rpc(Route::Activity, body, Some(&r.boot), budget_ms)
            .await?;
        if reply.status != 200 || str_of(&reply.value, "state") != Some("activity") {
            return err(Reason::Report);
        }
        let report = report_value(&reply.value, &Self::scope_of(r))?;
        Self::retain_report(r, &report)?;
        Ok(report)
    }

    fn revoke_flight(self: &Arc<Self>, r: &Arc<Record>, budget_ms: f64) -> Flight<Revoked> {
        let mut st = r.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = &st.cleanup {
            return f.clone();
        }
        let me = Arc::clone(self);
        let record = Arc::clone(r);
        let task = tokio::spawn(async move {
            match me.revoke_inner(&record, budget_ms).await {
                Ok(done) => Ok(done),
                Err(_) => {
                    // Only the session this lease belongs to can be left uncertain. Hold new authority until
                    // that lease has run out. A lease of an already replaced session must not take the new one down.
                    let mut s = me.lock();
                    let current = s.epoch.as_ref().map(|e| (e.session.clone(), e.until));
                    match current {
                        Some((session, until)) if session == record.session => {
                            me.fail_in(&mut s, "cleanup", until)
                        }
                        _ => {
                            s.active.remove(&record.id);
                        }
                    }
                    Ok(Revoked {
                        confirmed: false,
                        report: Report::unavailable("uncertain", false),
                    })
                }
            }
        });
        let flight: Flight<Revoked> = (Box::pin(async move {
            task.await.unwrap_or(Ok(Revoked {
                confirmed: false,
                report: Report::unavailable("uncertain", false),
            }))
        }) as BoxFuture<Result<Revoked, ControlError>>)
            .shared();
        st.cleanup = Some(flight.clone());
        flight
    }

    async fn revoke_inner(
        self: &Arc<Self>,
        r: &Arc<Record>,
        budget_ms: f64,
    ) -> Result<Revoked, ControlError> {
        if budget_ms <= 0.0 {
            return err(Reason::Timeout);
        }
        let first = self.mono()?;
        let until = first + budget_ms.min(RPC_MS);
        let iat = self.stamp(&r.task)?;
        let mut nonce = [0u8; 16];
        self.clock.fill(&mut nonce);
        let signed_revoke = mint(
            &r.key,
            &SignedGrant {
                act: Act::Revoke,
                task: r.task.clone(),
                hosts: Vec::new(),
                iat,
                exp: safe_int(iat + self.life)?,
                idle: 1,
                nonce: b64url_encode(&nonce),
            },
        )
        .ok_or(ControlError(Reason::Request))?;
        let body = format!(
            "{},\"grantIssuedAt\":{},\"signedRevoke\":{}}}",
            Self::lease_body(r),
            r.iat,
            json_str(&signed_revoke)
        );
        let left = until - self.mono()?;
        let reply = self.rpc(Route::Revoke, body, Some(&r.boot), left).await?;
        let v = &reply.value;
        let mut report: Option<Report> = None;
        let barrier = v.get("barrierInstalled").and_then(Value::as_bool);
        if reply.status == 200 && str_of(v, "state") == Some("revoke-confirmed") {
            report = Some(report_value(v, &Self::scope_of(r))?);
        } else {
            check_scope(v, &Self::scope_of(r))?;
            if str_of(v, "state") != Some("barrier-installed")
                || barrier != Some(true)
                || v.get("drained").and_then(Value::as_bool) != Some(true)
            {
                return err(Reason::Uncertain);
            }
        }
        if let Some(rep) = &report {
            if !rep.final_ || barrier != Some(true) {
                return err(Reason::Uncertain);
            }
        }
        let report = match report {
            Some(rep) => rep,
            None => {
                let left = until - self.mono()?;
                self.activity(r, left)
                    .await
                    .unwrap_or_else(|_| Report::unavailable("unavailable", true))
            }
        };
        if !report.final_ {
            return err(Reason::Uncertain);
        }
        Self::retain_report(r, &report)?;
        self.lock().active.remove(&r.id);
        Ok(Revoked {
            confirmed: true,
            report,
        })
    }

    fn forget_flight(
        self: &Arc<Self>,
        r: &Arc<Record>,
        snapshot_id: &str,
    ) -> Result<Flight<bool>, ControlError> {
        let mut st = r.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.ackable.as_deref() != Some(snapshot_id) || !is_hex(snapshot_id, 16) {
            return err(Reason::Snapshot);
        }
        if let Some(f) = &st.forget {
            return Ok(f.clone());
        }
        let me = Arc::clone(self);
        let record = Arc::clone(r);
        let snapshot = snapshot_id.to_string();
        let task = tokio::spawn(async move {
            let body = format!(
                "{{\"version\":1,\"sessionId\":{},\"taskId\":{},\"leaseNonce\":{},\"snapshotId\":{}}}",
                json_str(&record.session),
                json_str(&record.task),
                json_str(&record.nonce),
                json_str(&snapshot)
            );
            let outcome = async {
                let reply = me
                    .rpc(Route::Forget, body, Some(&record.boot), 5000.0)
                    .await?;
                check_scope(&reply.value, &Self::scope_of(&record))?;
                if reply.status != 200
                    || str_of(&reply.value, "state") != Some("forgotten")
                    || str_of(&reply.value, "snapshotId") != Some(snapshot.as_str())
                {
                    return err(Reason::Forget);
                }
                Ok(())
            }
            .await;
            if outcome.is_ok() {
                record
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .acknowledged = true;
            }
            Ok(outcome.is_ok())
        });
        let flight: Flight<bool> = (Box::pin(async move { task.await.unwrap_or(Ok(false)) })
            as BoxFuture<Result<bool, ControlError>>)
            .shared();
        st.forget = Some(flight.clone());
        Ok(flight)
    }

    async fn close(self: &Arc<Self>) -> Vec<Revoked> {
        let flight = {
            let mut s = self.lock();
            s.closed = true;
            s.stop_timers();
            s.ready_flight.clone()
        };
        if let Some(f) = flight {
            let _ = f.await;
        }
        let until = self.mono().unwrap_or(0.0) + RPC_MS;
        let pending: Vec<Arc<Record>> = {
            let s = self.lock();
            s.active
                .iter()
                .filter_map(|id| s.records.get(id).cloned())
                .collect()
        };
        let mut results = Vec::new();
        for chunk in pending.chunks(4) {
            let mut flights = Vec::new();
            for r in chunk {
                let budget = (until - self.mono().unwrap_or(until)).max(0.0);
                flights.push(self.revoke_flight(r, budget));
            }
            for f in flights {
                if let Ok(done) = f.await {
                    results.push(done);
                }
            }
        }
        results
    }
}

/// A task's domains as the contract carries them: lowercased, outer dots dropped, deduplicated.
pub fn grant_hosts(domains: &[String]) -> Result<Vec<String>, ControlError> {
    let mut hosts: Vec<String> = Vec::new();
    for d in domains {
        let h = d.to_lowercase();
        let h = h.trim_matches('.');
        if h.is_empty() {
            continue;
        }
        let ok = h.len() <= 253
            && h.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            });
        if !ok {
            return err(Reason::Hosts);
        }
        if !hosts.iter().any(|x| x == h) {
            hosts.push(h.to_string());
        }
    }
    if hosts.len() > 64 {
        return err(Reason::Hosts);
    }
    Ok(hosts)
}

fn equal_tag(actual: &str, expected: &str) -> bool {
    if actual.len() != 64
        || expected.len() != 64
        || !actual
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in actual.bytes().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[allow(dead_code)]
fn sha_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

#[allow(dead_code)]
fn control_key_of(file: [u8; 32]) -> Option<ControlKey> {
    egress::control::derive_control_key(&FileKey::from_bytes(file)).ok()
}
