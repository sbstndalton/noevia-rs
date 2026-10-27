//! Integrated buffered session, scoped take-away, ownership and report controller.
//! Live transport, idle/policy forwarding integration and permission-safe key-file
//! adapter remain held; no listener or production mode is enabled here.
use super::leases::{Book, Retirement, Scope};
use super::{
    http::{self, Authenticated},
    replay::{self, Admission, Replay, Ticket},
    session::{Boot, Proof, Session},
    wire::ReadyMode,
};
use egress::control::{FileKey, Route, CAPABILITY, MAX_SAFE_INTEGER};
use egress::{Act, Admission as GrantAdmission, Ledger};
use hyper::{Request, Response};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Notify};
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OPERATIONS: usize = 256;
const MAX_TASK_OPERATIONS: usize = 64;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Key,
    State,
    Clock,
    Authentication,
    Unavailable,
    Capacity,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control engine {self:?}")
    }
}
impl std::error::Error for Error {}
/// Trusted configuration adapter, not a request-selected key. Must reread current
/// permission-checked source and fail closed; no cached-key fallback on read error.
pub trait KeyProvider: Send + Sync {
    fn current(&self) -> Result<FileKey, Error>;
}
struct Operation {
    scope: Scope,
    task: String,
    cancel: watch::Sender<bool>,
}
struct State {
    boot: Boot,
    fingerprint: Option<String>,
    session: Option<Session>,
    session_id: Option<String>,
    initialized: bool,
    transition: Option<u64>,
    generation: u64,
    replay: Replay,
    ledger: Ledger,
    leases: Book,
    current_leases: HashMap<String, Scope>,
    epoch_retired: bool,
    operations: HashMap<u64, Operation>,
    next_operation: u64,
}
struct Inner {
    source: Arc<dyn KeyProvider>,
    state: Mutex<State>,
    drained: Notify,
    drain_timeout: Duration,
}
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}
/// Includes pending DNS/connect/forward work once the native transport adopts it.
/// Drop means that work is actually finished/reaped, not just cancellation signaled.
pub struct OperationGuard {
    inner: Weak<Inner>,
    id: u64,
    session_id: String,
    token: Vec<u8>,
    task: String,
    nonce: String,
    cancel: watch::Receiver<bool>,
}
impl std::fmt::Debug for OperationGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlOperation(<redacted>)")
    }
}
impl OperationGuard {
    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow() || self.cancel.has_changed().is_err()
    }
    /// Decision tally only; never supplies network authority or touches TTL.
    /// Native adapter must pass host-only identities and controlled refusal text.
    pub fn record_decision(
        &self,
        host: &str,
        allowed: bool,
        reason: Option<&str>,
    ) -> Result<(), Error> {
        let inner = self.inner.upgrade().ok_or(Error::Unavailable)?;
        let mut st = inner.state.lock().map_err(|_| Error::State)?;
        let op = st.operations.get(&self.id).ok_or(Error::Unavailable)?;
        let scope = op.scope.clone();
        st.leases.event(&scope, host, allowed, reason);
        Ok(())
    }
    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        let _ = self.cancel.changed().await;
    }
    /// Call immediately after async work and before any new upstream effect. Target
    /// policy/idle TTL must also be checked by the native data admission path.
    pub fn recheck(&self) -> Result<(), Error> {
        let inner = self.inner.upgrade().ok_or(Error::Unavailable)?;
        let mut st = inner.state.lock().map_err(|_| Error::State)?;
        let _key = current(&inner, &mut st)?;
        refresh(&mut st);
        if st.transition.is_some()
            || self.is_cancelled()
            || st.session_id.as_deref() != Some(&self.session_id)
            || !st.operations.contains_key(&self.id)
        {
            return Err(Error::Unavailable);
        }
        let claims = st
            .session
            .as_mut()
            .ok_or(Error::Unavailable)?
            .verify_token(&self.token, wall()?)
            .map_err(|_| Error::Authentication)?;
        if claims.act != Act::Grant || claims.task != self.task || claims.nonce != self.nonce {
            return Err(Error::Authentication);
        }
        if st
            .ledger
            .admit(&claims, wall()?)
            .map_err(|_| Error::Authentication)?
            != GrantAdmission::Current
        {
            st.ledger.retire(&claims.task, &claims.nonce, claims.exp);
            return Err(Error::Authentication);
        }
        Ok(())
    }
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            if let Ok(mut st) = inner.state.lock() {
                st.operations.remove(&self.id);
            }
            inner.drained.notify_waiters();
        }
    }
}
struct Transition {
    inner: Weak<Inner>,
    generation: u64,
}
impl Drop for Transition {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            if let Ok(mut st) = inner.state.lock() {
                if st.transition == Some(self.generation) {
                    st.transition = None;
                }
            }
        }
    }
}
impl Engine {
    pub fn new(source: Arc<dyn KeyProvider>) -> Result<Self, Error> {
        Self::with_timeout(source, DRAIN_TIMEOUT)
    }
    fn with_timeout(source: Arc<dyn KeyProvider>, drain_timeout: Duration) -> Result<Self, Error> {
        let file = source.current()?;
        Ok(Self {
            inner: Arc::new(Inner {
                source,
                drain_timeout,
                drained: Notify::new(),
                state: Mutex::new(State {
                    boot: Boot::generate().map_err(|_| Error::Unavailable)?,
                    fingerprint: Some(file.fingerprint()),
                    session: None,
                    session_id: None,
                    initialized: false,
                    transition: None,
                    generation: 0,
                    replay: Replay::new(),
                    ledger: Ledger::new(),
                    leases: Book::default(),
                    current_leases: HashMap::new(),
                    epoch_retired: true,
                    operations: HashMap::new(),
                    next_operation: 0,
                }),
            }),
        })
    }
    /// Called by the existing native proxy maintenance loop once integrated; this
    /// function creates no background worker. Admission also checks expiry itself.
    pub fn maintenance(&self) -> Result<(), Error> {
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let _key = current(&self.inner, &mut st)?;
        refresh(&mut st);
        Ok(())
    }
    pub fn operation(
        &self,
        token: &[u8],
        task: &str,
        nonce: &str,
    ) -> Result<OperationGuard, Error> {
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let _file = current(&self.inner, &mut st)?;
        refresh(&mut st);
        if st.transition.is_some() || !st.initialized {
            return Err(Error::Unavailable);
        }
        let claims = st
            .session
            .as_mut()
            .ok_or(Error::Unavailable)?
            .verify_token(token, wall()?)
            .map_err(|_| Error::Authentication)?;
        if claims.act != Act::Grant || claims.task != task || claims.nonce != nonce {
            return Err(Error::Authentication);
        }
        if st.operations.len() >= MAX_OPERATIONS
            || st.operations.values().filter(|o| o.task == task).count() >= MAX_TASK_OPERATIONS
        {
            return Err(Error::Capacity);
        }
        let scope = Scope {
            session: st.session_id.clone().ok_or(Error::Unavailable)?,
            task: task.into(),
            nonce: nonce.into(),
        };
        if st.leases.known(&scope) && !st.leases.active(&scope) {
            return Err(Error::Authentication);
        }
        let original = std::str::from_utf8(token).map_err(|_| Error::Authentication)?;
        let owner = st
            .session
            .as_ref()
            .ok_or(Error::Unavailable)?
            .ownership_verifier();
        if let Err(err) = st.leases.remember(&scope, &claims, original, owner) {
            if err == super::leases::Error::Capacity {
                retire(&mut st);
                return Err(Error::Capacity);
            }
            return Err(Error::Authentication);
        }
        let id = st.next_operation.checked_add(1).ok_or(Error::Capacity)?;
        let admitted = match st.ledger.admit(&claims, wall()?) {
            Ok(v) => v,
            Err(_) => {
                let _ = st.leases.retire(&scope, Retirement::Interrupted);
                cancel_scope(&mut st, &scope);
                finalize_scope(&mut st, &scope);
                return Err(Error::Authentication);
            }
        };
        if admitted == GrantAdmission::New {
            if let Some(old) = st.current_leases.insert(task.into(), scope.clone()) {
                if old != scope {
                    let _ = st.leases.retire(&old, Retirement::Interrupted);
                    cancel_scope(&mut st, &old);
                    finalize_scope(&mut st, &old);
                }
            }
        }
        let session_id = st.session_id.clone().ok_or(Error::Unavailable)?;
        let (send, cancel) = watch::channel(false);
        st.next_operation = id;
        st.operations.insert(
            id,
            Operation {
                scope: scope.clone(),
                task: task.into(),
                cancel: send,
            },
        );
        Ok(OperationGuard {
            inner: Arc::downgrade(&self.inner),
            id,
            session_id,
            token: token.to_vec(),
            task: task.into(),
            nonce: nonce.into(),
            cancel,
        })
    }
    /// Explicit native GrantStore idle/absolute retirement hook; actual store
    /// wiring is still held. Never updates another lease's task watermark.
    pub fn retire_grant(&self, session: &str, task: &str, nonce: &str) -> Result<(), Error> {
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let scope = Scope {
            session: session.into(),
            task: task.into(),
            nonce: nonce.into(),
        };
        st.leases
            .retire(&scope, Retirement::Interrupted)
            .map_err(|_| Error::Unavailable)?;
        if st.session_id.as_deref() == Some(session) {
            let exp = st.leases.exp(&scope).map_err(|_| Error::Unavailable)?;
            st.ledger.retire(task, nonce, exp);
        }
        cancel_scope(&mut st, &scope);
        finalize_scope(&mut st, &scope);
        Ok(())
    }
    pub async fn execute(&self, req: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, Error> {
        let file = load(&self.inner)?;
        let auth = http::authenticate(&req, &file, wall()?).map_err(|_| Error::Authentication)?;
        let admission = {
            let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
            let current = current(&self.inner, &mut st)?;
            refresh(&mut st);
            if auth.fingerprint() != current.fingerprint() {
                return Err(Error::Key);
            }
            auth.reserve(&mut st.replay, wall()?)
        };
        let admission = match admission {
            Ok(a) => a,
            Err(replay::Error::Conflict) => return reply(&auth, 409, "conflict"),
            Err(replay::Error::Capacity) => return reply(&auth, 503, "unavailable"),
            Err(replay::Error::Stale) => return reply(&auth, 408, "stale"),
            Err(_) => return reply(&auth, 503, "uncertain"),
        };
        match admission {
            Admission::Cached(c) => auth
                .response(c.status, c.body.to_vec())
                .map_err(|_| Error::State),
            Admission::Pending => reply(&auth, 202, "pending"),
            Admission::Unavailable => reply(&auth, 503, "uncertain"),
            Admission::Execute(ticket) => {
                let outcome = match auth.body().route() {
                    Route::Ready => self.ready(&auth).await,
                    Route::Renew => self.renew(&auth),
                    Route::Revoke => self.revoke(&auth).await,
                    Route::Activity => self.activity(&auth),
                    Route::Forget => self.forget(&auth),
                };
                let (status, body) = match outcome {
                    Ok(v) => v,
                    Err(_) => (503, terminal(&auth, "uncertain")),
                };
                self.finish(&auth, ticket, status, body)
            }
        }
    }
    fn finish(
        &self,
        auth: &Authenticated,
        ticket: Ticket,
        mut status: u16,
        mut body: Value,
    ) -> Result<Response<Vec<u8>>, Error> {
        let bytes = {
            let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
            let scope = auth.body().lease().map(|l| Scope {
                session: l.session_id.into(),
                task: l.task_id.into(),
                nonce: l.nonce.into(),
            });
            let has_summary = body
                .pointer("/snapshot/summary")
                .is_some_and(|v| !v.is_null());
            // An earlier control may have captured a report, then lost the lock
            // to acknowledged Forget. Never cache/send that stale body afterward.
            if has_summary && scope.as_ref().is_some_and(|s| st.leases.forgotten(s)) {
                status = 503;
                body = terminal(auth, "unavailable");
            }
            let bytes = serde_json::to_vec(&body).map_err(|_| Error::State)?;
            let result = if has_summary && status == 200 {
                if let Some(s) = scope {
                    st.replay
                        .complete_scoped(ticket, status, bytes.clone(), s.report_tag())
                } else {
                    st.replay.complete(ticket, status, bytes.clone())
                }
            } else {
                st.replay.complete(ticket, status, bytes.clone())
            };
            if result.is_err() {
                return reply(auth, 503, "uncertain");
            }
            bytes
        };
        auth.response(status, bytes).map_err(|_| Error::State)
    }
    fn renew(&self, auth: &Authenticated) -> Result<(u16, Value), Error> {
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let file = current(&self.inner, &mut st)?;
        refresh(&mut st);
        if !fresh(auth)? {
            return Ok((408, terminal(auth, "stale")));
        }
        let (id, sequence) = auth.body().renewal().ok_or(Error::State)?;
        if st.transition.is_some()
            || auth.fingerprint() != file.fingerprint()
            || auth.boot_id() != Some(st.boot.id().as_str())
            || st.session_id.as_deref() != Some(id)
        {
            return Ok((409, terminal(auth, "conflict")));
        }
        let Some(session) = st.session.as_mut() else {
            return Ok((409, terminal(auth, "conflict")));
        };
        let Ok(prior) = session.proof() else {
            return Ok((409, terminal(auth, "conflict")));
        };
        // Identical-ID retries were handled by Replay. A new ID cannot reuse sequence.
        if prior.sequence.checked_add(1) != Some(sequence) {
            return Ok((409, terminal(auth, "conflict")));
        }
        let proof = session.renew(sequence).map_err(|_| Error::Unavailable)?;
        Ok((200, live_response(auth, "renewed", proof)))
    }
    async fn ready(&self, auth: &Authenticated) -> Result<(u16, Value), Error> {
        let generation = {
            let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
            let file = current(&self.inner, &mut st)?;
            refresh(&mut st);
            if !fresh(auth)? {
                return Ok((408, terminal(auth, "stale")));
            }
            if st.transition.is_some() || !ready_precondition(&mut st, auth, &file) {
                return Ok((409, terminal(auth, "conflict")));
            }
            retire(&mut st);
            st.generation = st.generation.checked_add(1).ok_or(Error::Capacity)?;
            let g = st.generation;
            st.transition = Some(g);
            g
        };
        let transition = Transition {
            inner: Arc::downgrade(&self.inner),
            generation,
        };
        if !matches!(
            tokio::time::timeout(self.inner.drain_timeout, self.wait_drained()).await,
            Ok(Ok(()))
        ) {
            drop(transition);
            return Ok((503, terminal(auth, "drain-incomplete")));
        }
        // No state mutex is held across drain. Reload actual key and all preconditions.
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let file = current(&self.inner, &mut st)?;
        if st.transition != Some(generation)
            || st.generation != generation
            || !st.operations.is_empty()
            || !fresh(auth)?
            || !ready_precondition(&mut st, auth, &file)
        {
            return Ok((409, terminal(auth, "conflict")));
        }
        let mut session = st
            .boot
            .start_session(&file)
            .map_err(|_| Error::Unavailable)?;
        let proof = session.proof().map_err(|_| Error::Unavailable)?;
        st.session_id = Some(proof.session_id.clone());
        st.session = Some(session);
        st.initialized = true;
        st.ledger = Ledger::new();
        st.current_leases.clear();
        st.epoch_retired = false;
        st.transition = None;
        let result = live_response(auth, "ready", proof);
        drop(st);
        drop(transition);
        Ok((200, result))
    }
    fn activity(&self, auth: &Authenticated) -> Result<(u16, Value), Error> {
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let file = current(&self.inner, &mut st)?;
        refresh(&mut st);
        if !fresh(auth)? || auth.fingerprint() != file.fingerprint() {
            return Ok((409, terminal(auth, "conflict")));
        }
        let scope = owned(&mut st, auth)?;
        finalize_scope(&mut st, &scope);
        let snap = st.leases.snapshot(&scope).map_err(|_| Error::Unavailable)?;
        Ok((200, snapshot_response(auth, "activity", &scope, snap)))
    }
    fn forget(&self, auth: &Authenticated) -> Result<(u16, Value), Error> {
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let file = current(&self.inner, &mut st)?;
        refresh(&mut st);
        if !fresh(auth)?
            || auth.fingerprint() != file.fingerprint()
            || auth.boot_id() != Some(st.boot.id().as_str())
        {
            return Ok((409, terminal(auth, "conflict")));
        }
        let l = auth.body().lease().ok_or(Error::Authentication)?;
        let scope = Scope {
            session: l.session_id.into(),
            task: l.task_id.into(),
            nonce: l.nonce.into(),
        };
        let id = auth.body().snapshot_id().ok_or(Error::Authentication)?;
        if st.leases.forget(&scope, id).is_err() {
            return Ok((409, terminal(auth, "conflict")));
        }
        st.replay.redact_reports(scope.report_tag());
        Ok((
            200,
            json!({"version":1,"requestId":auth.request_id(),"state":"forgotten","bootId":auth.boot_id(),"sessionId":scope.session,"taskId":scope.task,"leaseNonce":scope.nonce,"snapshotId":id}),
        ))
    }
    async fn revoke(&self, auth: &Authenticated) -> Result<(u16, Value), Error> {
        let scope = {
            let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
            let file = current(&self.inner, &mut st)?;
            refresh(&mut st);
            if !fresh(auth)? || auth.fingerprint() != file.fingerprint() {
                return Ok((409, terminal(auth, "conflict")));
            }
            let scope = owned(&mut st, auth)?;
            let (iat, token) = auth.body().revocation().ok_or(Error::Authentication)?;
            st.leases
                .proof(
                    &scope,
                    auth.body().signed_grant().ok_or(Error::Authentication)?,
                    Some(iat),
                )
                .map_err(|_| Error::Authentication)?;
            let revoke = st
                .leases
                .verify_revoke(&scope, token, iat, wall()?)
                .map_err(|_| Error::Authentication)?;
            if st.session_id.as_deref() == Some(scope.session.as_str()) {
                if st
                    .current_leases
                    .get(&scope.task)
                    .is_some_and(|v| v != &scope)
                {
                    // Reject any task-wide watermark change, but still fence
                    // this proven target nonce: even an unused newer token must
                    // not gain authority after its owner asked for take-away.
                    let exp = st.leases.exp(&scope).map_err(|_| Error::Unavailable)?;
                    st.ledger.retire(&scope.task, &scope.nonce, exp);
                    st.leases
                        .retire(&scope, Retirement::Revoked)
                        .map_err(|_| Error::Unavailable)?;
                    cancel_scope(&mut st, &scope);
                    finalize_scope(&mut st, &scope);
                    let drained = !st.operations.values().any(|o| o.scope == scope);
                    return Ok((409, barrier_response(auth, &scope, drained)));
                }
                if st.ledger.admit(&revoke, wall()?).is_err() {
                    retire(&mut st);
                    return Ok((503, terminal(auth, "unavailable")));
                }
                let exp = st.leases.exp(&scope).map_err(|_| Error::Unavailable)?;
                st.ledger.retire(&scope.task, &scope.nonce, exp);
            }
            st.leases
                .retire(&scope, Retirement::Revoked)
                .map_err(|_| Error::Unavailable)?;
            cancel_scope(&mut st, &scope);
            scope
        };
        if !matches!(
            tokio::time::timeout(self.inner.drain_timeout, self.wait_scope_drained(&scope)).await,
            Ok(Ok(()))
        ) {
            return Ok((503, barrier_response(auth, &scope, false)));
        }
        let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
        let file = current(&self.inner, &mut st)?;
        refresh(&mut st);
        if !fresh(auth)?
            || auth.fingerprint() != file.fingerprint()
            || auth.boot_id() != Some(st.boot.id().as_str())
        {
            return Ok((409, terminal(auth, "conflict")));
        }
        finalize_scope(&mut st, &scope);
        let snap = match st.leases.snapshot(&scope) {
            Ok(v) => v,
            Err(_) => return Ok((503, barrier_response(auth, &scope, true))),
        };
        if !snap.final_report {
            return Err(Error::Unavailable);
        }
        Ok((
            200,
            snapshot_response(auth, "revoke-confirmed", &scope, snap),
        ))
    }
    async fn wait_scope_drained(&self, scope: &Scope) -> Result<(), Error> {
        loop {
            let n = self.inner.drained.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            let empty = {
                let st = self.inner.state.lock().map_err(|_| Error::State)?;
                !st.operations.values().any(|o| o.scope == *scope)
            };
            if empty {
                return Ok(());
            }
            n.await;
        }
    }
    async fn wait_drained(&self) -> Result<(), Error> {
        loop {
            let notified = self.inner.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let empty = {
                self.inner
                    .state
                    .lock()
                    .map_err(|_| Error::State)?
                    .operations
                    .is_empty()
            };
            if empty {
                return Ok(());
            }
            notified.await;
        }
    }
}
fn owned(st: &mut State, auth: &Authenticated) -> Result<Scope, Error> {
    if auth.boot_id() != Some(st.boot.id().as_str()) {
        return Err(Error::Authentication);
    }
    let l = auth.body().lease().ok_or(Error::Authentication)?;
    let scope = Scope {
        session: l.session_id.into(),
        task: l.task_id.into(),
        nonce: l.nonce.into(),
    };
    let token = auth.body().signed_grant().ok_or(Error::Authentication)?;
    if st.leases.known(&scope) {
        st.leases
            .proof(&scope, token, None)
            .map_err(|_| Error::Authentication)?;
        return Ok(scope);
    }
    if st.transition.is_some() || !st.initialized || st.session_id.as_deref() != Some(l.session_id)
    {
        return Err(Error::Unavailable);
    }
    let session = st.session.as_mut().ok_or(Error::Unavailable)?;
    let claims = session
        .verify_token(token.as_bytes(), wall()?)
        .map_err(|_| Error::Authentication)?;
    let owner = session.ownership_verifier();
    if let Err(e) = st.leases.remember(&scope, &claims, token, owner) {
        if e == super::leases::Error::Capacity {
            retire(st);
        }
        return Err(Error::Unavailable);
    }
    if let Some(other) = st.current_leases.get(&scope.task).cloned() {
        if other != scope {
            let current_iat = st
                .leases
                .issued_at(&other)
                .map_err(|_| Error::Unavailable)?;
            if (claims.iat, claims.nonce.as_str()) < (current_iat, other.nonce.as_str()) {
                let _ = st.leases.retire(&scope, Retirement::Interrupted);
                finalize_scope(st, &scope);
            }
        }
    }
    Ok(scope)
}
fn barrier_response(auth: &Authenticated, s: &Scope, drained: bool) -> Value {
    json!({"version":1,"requestId":auth.request_id(),"state":"barrier-installed","barrierInstalled":true,"bootId":auth.boot_id(),"drained":drained,"sessionId":s.session,"taskId":s.task,"leaseNonce":s.nonce})
}
fn snapshot_response(
    auth: &Authenticated,
    state: &str,
    s: &Scope,
    p: super::leases::Snapshot,
) -> Value {
    json!({"version":1,"requestId":auth.request_id(),"state":state,"bootId":auth.boot_id(),"sessionId":s.session,"taskId":s.task,"leaseNonce":s.nonce,"barrierInstalled":p.barrier_installed,"drained":p.final_report,"snapshot":{"id":p.id,"state":p.state,"final":p.final_report,"summary":p.summary,"partial":p.partial,"overflow":p.overflow}})
}
fn ready_precondition(st: &mut State, auth: &Authenticated, file: &FileKey) -> bool {
    if auth.fingerprint() != file.fingerprint() {
        return false;
    }
    let Some(r) = auth.body().ready() else {
        return false;
    };
    if st.session.as_mut().is_some_and(|s| s.proof().is_ok()) {
        return false;
    }
    match r.mode {
        ReadyMode::Initial => {
            !st.initialized && auth.boot_id().is_none() && r.prior_session.is_none()
        }
        ReadyMode::Replace => {
            st.initialized
                && auth.boot_id() == Some(st.boot.id().as_str())
                && r.prior_session == st.session_id.as_deref()
        }
    }
}
fn refresh(st: &mut State) {
    if st.session.as_mut().is_some_and(|s| s.proof().is_err()) {
        retire(st);
    }
}
fn retire(st: &mut State) {
    if let Some(s) = st.session.as_mut() {
        s.retire();
    }
    for o in st.operations.values() {
        o.cancel.send_replace(true);
    }
    if !st.epoch_retired {
        st.epoch_retired = true;
        if let Some(id) = st.session_id.clone() {
            st.leases.retire_session(&id);
        }
        let scopes: Vec<_> = st.current_leases.values().cloned().collect();
        for scope in scopes {
            finalize_scope(st, &scope);
        }
    }
}
fn cancel_scope(st: &mut State, s: &Scope) {
    for o in st.operations.values().filter(|o| o.scope == *s) {
        o.cancel.send_replace(true);
    }
}
fn finalize_scope(st: &mut State, s: &Scope) {
    if !st.operations.values().any(|o| o.scope == *s) {
        let _ = st.leases.finalize(s);
    }
}
fn sync_key(st: &mut State, fingerprint: Option<String>) -> Result<(), Error> {
    if st.fingerprint != fingerprint {
        retire(st);
        st.fingerprint = fingerprint;
        st.initialized = false;
        st.generation = st.generation.checked_add(1).ok_or(Error::Capacity)?;
    }
    Ok(())
}
// Trusted adapter executes under the admission lock: it must be bounded and
// non-reentrant. Reading outside the lock could roll back a newer observed key.
fn current(inner: &Arc<Inner>, st: &mut State) -> Result<FileKey, Error> {
    match inner.source.current() {
        Ok(key) => {
            sync_key(st, Some(key.fingerprint()))?;
            Ok(key)
        }
        Err(_) => {
            sync_key(st, None)?;
            Err(Error::Key)
        }
    }
}
fn load(inner: &Arc<Inner>) -> Result<FileKey, Error> {
    // Even an initial failed observation must be serialized. A saved failure
    // outside this lock could retire a newer recovered session after it starts.
    let mut st = inner.state.lock().map_err(|_| Error::State)?;
    current(inner, &mut st)
}
fn wall() -> Result<u64, Error> {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Clock)?
        .as_millis();
    let n = u64::try_from(n).map_err(|_| Error::Clock)?;
    if n == 0 || n > MAX_SAFE_INTEGER {
        return Err(Error::Clock);
    }
    Ok(n)
}
fn fresh(auth: &Authenticated) -> Result<bool, Error> {
    let now = wall()?;
    Ok(auth.deadline_ms() > now && auth.deadline_ms() - now <= replay::MAX_FRESHNESS_MS)
}
fn terminal(auth: &Authenticated, state: &str) -> Value {
    json!({"version":1,"requestId":auth.request_id(),"state":state})
}
fn live_response(auth: &Authenticated, state: &str, p: Proof) -> Value {
    let mut v = json!({"version":1,"requestId":auth.request_id(),"state":state,"capability":CAPABILITY,"bootId":p.boot_id,"sessionId":p.session_id,"keyFingerprint":p.key_fingerprint,"sequence":p.sequence,"leaseMs":p.remaining.as_millis()});
    if let Some(r) = auth.body().ready() {
        if let Some(o) = v.as_object_mut() {
            o.insert("challenge".into(), json!(r.challenge));
        }
    }
    v
}
fn reply(auth: &Authenticated, status: u16, state: &str) -> Result<Response<Vec<u8>>, Error> {
    let bytes = serde_json::to_vec(&terminal(auth, state)).map_err(|_| Error::State)?;
    auth.response(status, bytes).map_err(|_| Error::State)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egress::control::{derive_control_key, derive_epoch_keys, RequestMetadata};
    use serde::Deserialize;
    pub(super) struct Keys(Mutex<Option<FileKey>>);
    impl KeyProvider for Keys {
        fn current(&self) -> Result<FileKey, Error> {
            self.0
                .lock()
                .map_err(|_| Error::Key)?
                .clone()
                .ok_or(Error::Key)
        }
    }
    pub(super) fn setup() -> Result<(Engine, Arc<Keys>, FileKey), Error> {
        let f = FileKey::from_bytes([7; 32]);
        let k = Arc::new(Keys(Mutex::new(Some(f.clone()))));
        let e = Engine::with_timeout(k.clone(), Duration::from_millis(500))?;
        Ok((e, k, f))
    }
    pub(super) fn request(
        file: &FileKey,
        route: Route,
        id: u64,
        boot: Option<&str>,
        body: Value,
    ) -> Result<Request<Vec<u8>>, Box<dyn std::error::Error>> {
        let body = serde_json::to_vec(&body)?;
        let id = format!("{id:032x}");
        let fp = file.fingerprint();
        let deadline = wall()? + 30_000;
        let m = RequestMetadata {
            request_id: &id,
            deadline_ms: deadline,
            boot_id: boot,
            key_fingerprint: &fp,
        };
        let tag = derive_control_key(file)?.sign_request(route, m, &body)?;
        let mut b = Request::builder()
            .method("POST")
            .uri(route.path())
            .header("host", "egress-proxy:3001")
            .header("content-type", "application/json")
            .header("content-length", body.len())
            .header(http::ID_HEADER, id)
            .header(http::DEADLINE_HEADER, deadline)
            .header(http::FINGERPRINT_HEADER, fp)
            .header(http::MAC_HEADER, tag);
        if let Some(boot) = boot {
            b = b.header(http::BOOT_HEADER, boot);
        }
        Ok(b.body(body)?)
    }
    pub(super) fn initial(
        file: &FileKey,
        id: u64,
    ) -> Result<Request<Vec<u8>>, Box<dyn std::error::Error>> {
        request(
            file,
            Route::Ready,
            id,
            None,
            json!({"version":1,"mode":"initial","challenge":format!("{id:032x}"),"expectedKeyFingerprint":file.fingerprint(),"priorSession":null}),
        )
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct Ready {
        pub(super) boot_id: String,
        pub(super) session_id: String,
    }
    pub(super) async fn start(
        e: &Engine,
        f: &FileKey,
    ) -> Result<Ready, Box<dyn std::error::Error>> {
        let r = e.execute(initial(f, 1)?).await?;
        assert_eq!(r.status(), 200);
        Ok(serde_json::from_slice(r.body())?)
    }
    pub(super) fn unhex(s: &str) -> Result<[u8; 16], Box<dyn std::error::Error>> {
        if s.len() != 32 {
            return Err("identity length".into());
        }
        let v = s
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                std::str::from_utf8(p)
                    .map_err(|e| e.to_string())
                    .and_then(|s| u8::from_str_radix(s, 16).map_err(|e| e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        v.try_into().map_err(|_| "identity bytes".into())
    }
    pub(super) fn grant(
        f: &FileKey,
        r: &Ready,
        nonce: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let k = derive_epoch_keys(f, &unhex(&r.boot_id)?, &unhex(&r.session_id)?)?;
        let now = wall()?;
        let g = egress::SignedGrant {
            act: Act::Grant,
            task: "task.1".into(),
            hosts: vec!["example.com".into()],
            iat: now,
            exp: now + 60_000,
            idle: 10_000,
            nonce: nonce.into(),
        };
        egress::mint(&k.grant, &g).ok_or("mint".into())
    }
    pub(super) const NONCE: &str = "AAECAwQFBgcICQoLDA0ODw";
    #[tokio::test]
    async fn ready_and_ordered_renew_use_exact_replay_without_new_lease(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let ready_req = initial(&f, 1)?;
        let duplicate = ready_req.clone();
        let first = e.execute(ready_req).await?;
        let r: Ready = serde_json::from_slice(first.body())?;
        let cached = e.execute(duplicate).await?;
        assert_eq!(cached.body(), first.body());
        assert_eq!(e.execute(initial(&f, 2)?).await?.status(), 409);
        let changed = request(
            &f,
            Route::Ready,
            1,
            None,
            json!({"version":1,"mode":"initial","challenge":format!("{:032x}",99),"expectedKeyFingerprint":f.fingerprint(),"priorSession":null}),
        )?;
        assert_eq!(e.execute(changed).await?.status(), 409);

        let req = request(
            &f,
            Route::Renew,
            3,
            Some(&r.boot_id),
            json!({"version":1,"sessionId":r.session_id,"sequence":1}),
        )?;
        let duplicate = req.clone();
        let first = e.execute(req).await?;
        assert_eq!(first.status(), 200);
        let cached = e.execute(duplicate).await?;
        assert_eq!(first.body(), cached.body());
        let changed_id = request(
            &f,
            Route::Renew,
            4,
            Some(&r.boot_id),
            json!({"version":1,"sessionId":r.session_id,"sequence":1}),
        )?;
        assert_eq!(e.execute(changed_id).await?.status(), 409);
        let token = grant(&f, &r, NONCE)?;
        let op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        op.recheck()?;
        Ok(())
    }
    #[tokio::test]
    async fn key_replacement_cancels_pending_work_and_waits_for_actual_drop(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, k, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let mut op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        let next = FileKey::from_bytes([8; 32]);
        *k.0.lock().map_err(|_| Error::State)? = Some(next.clone());
        let future = e.execute(initial(&next, 9)?);
        tokio::pin!(future);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut future)
            .await
            .is_err());
        op.cancelled().await;
        assert!(op.recheck().is_err());
        drop(op);
        let response = future.await?;
        assert_eq!(response.status(), 200);
        let new: Ready = serde_json::from_slice(response.body())?;
        assert_ne!(new.session_id, r.session_id);
        assert_eq!(new.boot_id, r.boot_id);
        assert!(e.operation(token.as_bytes(), "task.1", NONCE).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn key_change_during_drain_cannot_install_stale_ready_epoch(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, k, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        let next = FileKey::from_bytes([8; 32]);
        *k.0.lock().map_err(|_| Error::State)? = Some(next.clone());
        let future = e.execute(initial(&next, 9)?);
        tokio::pin!(future);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut future)
            .await
            .is_err());
        assert!(op.is_cancelled());
        let newest = FileKey::from_bytes([9; 32]);
        *k.0.lock().map_err(|_| Error::State)? = Some(newest.clone());
        drop(op);
        assert_eq!(future.await?.status(), 409);
        assert_eq!(
            e.inner
                .state
                .lock()
                .map_err(|_| Error::State)?
                .fingerprint
                .as_deref(),
            Some(newest.fingerprint().as_str())
        );
        assert_eq!(e.execute(initial(&newest, 10)?).await?.status(), 200);
        Ok(())
    }
    #[tokio::test]
    async fn drain_timeout_and_cancelled_ready_never_restore_authority(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, k, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let mut op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        let next = FileKey::from_bytes([8; 32]);
        *k.0.lock().map_err(|_| Error::State)? = Some(next.clone());
        let result = e.execute(initial(&next, 9)?).await?;
        assert_eq!(result.status(), 503);
        assert!(String::from_utf8_lossy(result.body()).contains("drain-incomplete"));
        assert!(op.is_cancelled());
        assert!(op.recheck().is_err());
        let clone = e.clone();
        let req = initial(&next, 10)?;
        let task = tokio::spawn(async move { clone.execute(req).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let active = {
                    e.inner
                        .state
                        .lock()
                        .map_err(|_| Error::State)?
                        .transition
                        .is_some()
                };
                if active {
                    return Ok::<(), Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        task.abort();
        let _ = task.await;
        assert!(e
            .inner
            .state
            .lock()
            .map_err(|_| Error::State)?
            .transition
            .is_none());
        op.cancelled().await;
        drop(op);
        assert_eq!(e.execute(initial(&next, 11)?).await?.status(), 200);
        Ok(())
    }
    #[tokio::test]
    async fn expiry_and_key_failure_cancel_without_automatic_regrant(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, k, f) = setup()?;
        let ready_request = initial(&f, 1)?;
        let duplicate = ready_request.clone();
        let first = e.execute(ready_request).await?;
        let r: Ready = serde_json::from_slice(first.body())?;
        let token = grant(&f, &r, NONCE)?;
        let op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        {
            let mut st = e.inner.state.lock().map_err(|_| Error::State)?;
            st.session.as_mut().ok_or(Error::State)?.expire_for_test();
        }
        e.maintenance()?;
        assert_eq!(e.execute(duplicate).await?.body(), first.body());
        assert!(op.is_cancelled());
        assert!(op.recheck().is_err());
        assert!(e.operation(token.as_bytes(), "task.1", NONCE).is_err());
        drop(op);
        let wrong = request(
            &f,
            Route::Ready,
            4,
            Some(&r.boot_id),
            json!({"version":1,"mode":"replace","challenge":format!("{:032x}",4),"expectedKeyFingerprint":f.fingerprint(),"priorSession":null}),
        )?;
        assert_eq!(e.execute(wrong).await?.status(), 409);
        let replacement = request(
            &f,
            Route::Ready,
            5,
            Some(&r.boot_id),
            json!({"version":1,"mode":"replace","challenge":format!("{:032x}",5),"expectedKeyFingerprint":f.fingerprint(),"priorSession":r.session_id}),
        )?;
        let new = e.execute(replacement).await?;
        assert_eq!(new.status(), 200);
        let new: Ready = serde_json::from_slice(new.body())?;
        let token = grant(&f, &new, NONCE)?;
        let op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        *k.0.lock().map_err(|_| Error::State)? = None;
        assert_eq!(e.maintenance(), Err(Error::Key));
        assert!(op.is_cancelled());
        *k.0.lock().map_err(|_| Error::State)? = Some(f.clone());
        assert!(e.operation(token.as_bytes(), "task.1", NONCE).is_err());
        drop(op);
        assert_eq!(e.execute(initial(&f, 6)?).await?.status(), 200);
        Ok(())
    }
    #[tokio::test]
    async fn orphaned_operation_observes_controller_shutdown_as_cancellation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let mut op = e.operation(token.as_bytes(), "task.1", NONCE)?;
        drop(e);
        assert!(op.is_cancelled());
        op.cancelled().await;
        assert!(op.recheck().is_err());
        Ok(())
    }
    #[tokio::test]
    async fn task_capacity_and_scope_refuse_before_operation_registration(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        assert!(e.operation(token.as_bytes(), "other-task", NONCE).is_err());
        let mut ops = Vec::new();
        for _ in 0..MAX_TASK_OPERATIONS {
            ops.push(e.operation(token.as_bytes(), "task.1", NONCE)?);
        }
        assert_eq!(
            e.operation(token.as_bytes(), "task.1", NONCE).err(),
            Some(Error::Capacity)
        );
        drop(ops);
        assert!(e
            .inner
            .state
            .lock()
            .map_err(|_| Error::State)?
            .operations
            .is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn initial_key_failure_is_serialized_before_recovered_ready(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, TryLockError,
        };
        struct Gated {
            key: Mutex<FileKey>,
            fail_next: AtomicBool,
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl KeyProvider for Gated {
            fn current(&self) -> Result<FileKey, Error> {
                if self.fail_next.swap(false, Ordering::SeqCst) {
                    self.entered.send(()).map_err(|_| Error::State)?;
                    self.release
                        .lock()
                        .map_err(|_| Error::State)?
                        .recv()
                        .map_err(|_| Error::State)?;
                    return Err(Error::Key);
                }
                Ok(self.key.lock().map_err(|_| Error::State)?.clone())
            }
        }
        let (entered_tx, entered) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let file = FileKey::from_bytes([7; 32]);
        let source = Arc::new(Gated {
            key: Mutex::new(file.clone()),
            fail_next: AtomicBool::new(false),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let e = Engine::new(source.clone())?;
        let ready = start(&e, &file).await?;
        let token = grant(&file, &ready, NONCE)?;
        let old = e.operation(token.as_bytes(), "task.1", NONCE)?;
        source.fail_next.store(true, Ordering::SeqCst);
        let clone = e.clone();
        let first = std::thread::spawn(move || load(&clone.inner));
        entered.recv_timeout(Duration::from_secs(1))?;
        // Gated observation proves serialization without a scheduling sleep.
        // Old implementation observed failure without owning this mutex.
        assert!(matches!(
            e.inner.state.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        let next = FileKey::from_bytes([8; 32]);
        *source.key.lock().map_err(|_| Error::State)? = next.clone();
        release.send(())?;
        let result = first.join().map_err(|_| "reader thread failed")?;
        assert_eq!(result.err(), Some(Error::Key));
        assert!(old.is_cancelled());
        drop(old);
        let response = e.execute(initial(&next, 9)?).await?;
        assert_eq!(response.status(), 200);
        let recovered: Ready = serde_json::from_slice(response.body())?;
        let new_token = grant(&next, &recovered, NONCE)?;
        let live = e.operation(new_token.as_bytes(), "task.1", NONCE)?;
        live.recheck()?;
        assert!(!live.is_cancelled());
        Ok(())
    }
    #[tokio::test]
    async fn unused_activity_proof_does_not_install_network_authority(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let req = request(
            &f,
            Route::Activity,
            3,
            Some(&r.boot_id),
            json!({"version":1,"sessionId":r.session_id,"taskId":"task.1","leaseNonce":NONCE,"signedGrant":token}),
        )?;
        let result = e.execute(req).await?;
        assert_eq!(result.status(), 200);
        assert!(String::from_utf8_lossy(result.body()).contains("activity"));
        assert!(e
            .inner
            .state
            .lock()
            .map_err(|_| Error::State)?
            .operations
            .is_empty());
        Ok(())
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::tests::{grant, request, setup, start, unhex, Ready, NONCE};
    use super::*;
    use egress::control::derive_epoch_keys;
    const NEXT_NONCE: &str = "BAECAwQFBgcICQoLDA0ODw";
    fn claims(
        file: &FileKey,
        r: &Ready,
        token: &str,
    ) -> Result<egress::SignedGrant, Box<dyn std::error::Error>> {
        let key = derive_epoch_keys(file, &unhex(&r.boot_id)?, &unhex(&r.session_id)?)?;
        Ok(egress::verify(&key.grant, token.as_bytes(), wall()?).map_err(|e| format!("{e:?}"))?)
    }
    fn minted(
        file: &FileKey,
        r: &Ready,
        g: &egress::SignedGrant,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let key = derive_epoch_keys(file, &unhex(&r.boot_id)?, &unhex(&r.session_id)?)?;
        egress::mint(&key.grant, g).ok_or("mint".into())
    }
    fn control(
        file: &FileKey,
        r: &Ready,
        token: &str,
        g: &egress::SignedGrant,
        id: u64,
        revoke: bool,
    ) -> Result<Request<Vec<u8>>, Box<dyn std::error::Error>> {
        let body = if revoke {
            let now = wall()?.max(g.iat + 1);
            let signed = egress::SignedGrant {
                act: Act::Revoke,
                task: g.task.clone(),
                hosts: vec![],
                iat: now,
                exp: now + 60_000,
                idle: 1,
                nonce: NEXT_NONCE.into(),
            };
            json!({"version":1,"sessionId":r.session_id,"taskId":g.task,"leaseNonce":g.nonce,"grantIssuedAt":g.iat,"signedGrant":token,"signedRevoke":minted(file,r,&signed)?})
        } else {
            json!({"version":1,"sessionId":r.session_id,"taskId":g.task,"leaseNonce":g.nonce,"signedGrant":token})
        };
        request(
            file,
            if revoke {
                Route::Revoke
            } else {
                Route::Activity
            },
            id,
            Some(&r.boot_id),
            body,
        )
    }
    fn snapshot_id(response: &Response<Vec<u8>>) -> Result<String, Box<dyn std::error::Error>> {
        let v: Value = serde_json::from_slice(response.body())?;
        v.get("snapshot")
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or("snapshot id".into())
    }
    async fn forget(
        e: &Engine,
        f: &FileKey,
        r: &Ready,
        g: &egress::SignedGrant,
        id: u64,
        snapshot: &str,
    ) -> Result<Response<Vec<u8>>, Box<dyn std::error::Error>> {
        Ok(e.execute(request(f,Route::Forget,id,Some(&r.boot_id),json!({"version":1,"sessionId":r.session_id,"taskId":g.task,"leaseNonce":g.nonce,"snapshotId":snapshot}))?).await?)
    }
    #[tokio::test]
    async fn scoped_revoke_waits_for_drop_keeps_other_task_live_and_forget_keeps_barrier(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let g = claims(&f, &r, &token)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        op.record_decision("example.com", true, None)?;
        let mut other = g.clone();
        other.task = "task.2".into();
        other.nonce = NEXT_NONCE.into();
        let ot = minted(&f, &r, &other)?;
        let other_op = e.operation(ot.as_bytes(), &other.task, &other.nonce)?;
        let future = e.execute(control(&f, &r, &token, &g, 20, true)?);
        tokio::pin!(future);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut future)
            .await
            .is_err());
        assert!(op.is_cancelled());
        other_op.recheck()?;
        drop(op);
        let final_report = future.await?;
        assert_eq!(final_report.status(), 200);
        assert!(String::from_utf8_lossy(final_report.body()).contains("revoke-confirmed"));
        assert!(final_report.body().len() <= 64 * 1024);
        let id = snapshot_id(&final_report)?;
        assert_eq!(forget(&e, &f, &r, &g, 21, &id).await?.status(), 200);
        assert_eq!(forget(&e, &f, &r, &g, 22, &id).await?.status(), 200);
        other_op.recheck()?;
        {
            e.inner.state.lock().map_err(|_| Error::State)?.ledger = Ledger::new();
        }
        assert!(e.operation(token.as_bytes(), &g.task, &g.nonce).is_err());

        Ok(())
    }
    #[tokio::test]
    async fn superseded_report_finalizes_without_old_revoke_harming_new_lease(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let old = claims(&f, &r, &token)?;
        let op = e.operation(token.as_bytes(), &old.task, &old.nonce)?;
        op.record_decision("old.example", false, Some("host does not resolve"))?;
        let mut new = old.clone();
        new.iat += 1;
        new.exp += 1;
        new.nonce = NEXT_NONCE.into();
        let nt = minted(&f, &r, &new)?;
        let current = e.operation(nt.as_bytes(), &new.task, &new.nonce)?;
        assert!(op.is_cancelled());
        drop(op);
        assert_eq!(
            e.execute(control(&f, &r, &token, &old, 20, true)?)
                .await?
                .status(),
            409
        );
        e.retire_grant(&r.session_id, &old.task, &old.nonce)?;
        current.recheck()?;
        let report = e.execute(control(&f, &r, &token, &old, 21, false)?).await?;
        assert_eq!(report.status(), 200);
        let v: Value = serde_json::from_slice(report.body())?;
        assert_eq!(v.pointer("/snapshot/final"), Some(&json!(true)));
        assert_eq!(v.pointer("/snapshot/state"), Some(&json!("interrupted")));
        let id = snapshot_id(&report)?;
        assert_eq!(forget(&e, &f, &r, &old, 22, &id).await?.status(), 200);
        current.recheck()?;
        Ok(())
    }
    #[tokio::test]
    async fn retained_expired_original_reads_history_but_unknown_expired_never_returns_empty(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let ordinary = grant(&f, &r, NONCE)?;
        let mut g = claims(&f, &r, &ordinary)?;
        g.exp = g.iat + 100;
        g.idle = 100;
        let token = minted(&f, &r, &g)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        op.record_decision("example.com", true, None)?;
        let mut unknown = g.clone();
        unknown.nonce = NEXT_NONCE.into();
        let ut = minted(&f, &r, &unknown)?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(claims(&f, &r, &token).is_err());
        e.retire_grant(&r.session_id, &g.task, &g.nonce)?;
        assert!(op.is_cancelled());
        drop(op);
        let result = e.execute(control(&f, &r, &token, &g, 20, false)?).await?;
        assert_eq!(result.status(), 200);
        let v: Value = serde_json::from_slice(result.body())?;
        assert_eq!(v.pointer("/snapshot/final"), Some(&json!(true)));
        assert_eq!(v.pointer("/snapshot/summary/allowed"), Some(&json!(1)));
        let missing = e
            .execute(control(&f, &r, &ut, &unknown, 21, false)?)
            .await?;
        assert_eq!(missing.status(), 503);
        assert!(!String::from_utf8_lossy(missing.body()).contains("summary"));
        Ok(())
    }
    #[tokio::test]
    async fn scoped_drain_timeout_reports_installed_barrier_and_active_snapshot_cannot_be_forgotten(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let g = claims(&f, &r, &token)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        let active = e.execute(control(&f, &r, &token, &g, 20, false)?).await?;
        let id = snapshot_id(&active)?;
        assert_eq!(forget(&e, &f, &r, &g, 21, &id).await?.status(), 409);
        let timeout = e.execute(control(&f, &r, &token, &g, 22, true)?).await?;
        assert_eq!(timeout.status(), 503);
        assert!(String::from_utf8_lossy(timeout.body()).contains("barrier-installed"));
        assert!(op.is_cancelled());
        assert!(e.operation(token.as_bytes(), &g.task, &g.nonce).is_err());
        drop(op);
        let final_report = e.execute(control(&f, &r, &token, &g, 23, false)?).await?;
        assert_eq!(final_report.status(), 200);
        assert_ne!(snapshot_id(&final_report)?, id);
        Ok(())
    }
    #[tokio::test]
    async fn historical_scope_after_session_replacement_cannot_touch_new_authority(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let old = start(&e, &f).await?;
        let token = grant(&f, &old, NONCE)?;
        let g = claims(&f, &old, &token)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        op.record_decision("old.example", true, None)?;
        {
            e.inner
                .state
                .lock()
                .map_err(|_| Error::State)?
                .session
                .as_mut()
                .ok_or(Error::State)?
                .expire_for_test();
        }
        e.maintenance()?;
        drop(op);
        let replacement = request(
            &f,
            Route::Ready,
            20,
            Some(&old.boot_id),
            json!({"version":1,"mode":"replace","challenge":format!("{:032x}",20),"expectedKeyFingerprint":f.fingerprint(),"priorSession":old.session_id}),
        )?;
        let response = e.execute(replacement).await?;
        assert_eq!(response.status(), 200);
        let new: Ready = serde_json::from_slice(response.body())?;
        let nt = grant(&f, &new, NEXT_NONCE)?;
        let current = e.operation(nt.as_bytes(), "task.1", NEXT_NONCE)?;
        assert!(e.operation(token.as_bytes(), "task.1", NONCE).is_err());
        let history = e.execute(control(&f, &old, &token, &g, 21, false)?).await?;
        assert_eq!(history.status(), 200);
        let value: Value = serde_json::from_slice(history.body())?;
        assert_eq!(
            value.pointer("/snapshot/state"),
            Some(&json!("interrupted"))
        );
        assert_eq!(value.pointer("/snapshot/final"), Some(&json!(true)));
        let revoked = e.execute(control(&f, &old, &token, &g, 22, true)?).await?;
        assert_eq!(revoked.status(), 200);
        current.recheck()?;
        Ok(())
    }
    #[tokio::test]
    async fn acknowledged_forget_redacts_old_and_late_report_replays_without_reexecution(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let g = claims(&f, &r, &token)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        op.record_decision("private-host.example", true, None)?;
        drop(op);
        let revoke = control(&f, &r, &token, &g, 20, true)?;
        let retry_revoke = revoke.clone();
        let final_report = e.execute(revoke).await?;
        assert_eq!(final_report.status(), 200);
        let snapshot = snapshot_id(&final_report)?;
        let activity = control(&f, &r, &token, &g, 21, false)?;
        let retry_activity = activity.clone();
        assert_eq!(e.execute(activity).await?.status(), 200);
        // Deterministically model a report capture interleaving before its finish.
        let late = control(&f, &r, &token, &g, 22, false)?;
        let auth = http::authenticate(&late, &f, wall()?)?;
        let ticket = {
            let mut st = e.inner.state.lock().map_err(|_| Error::State)?;
            match auth.reserve(&mut st.replay, wall()?)? {
                Admission::Execute(t) => t,
                _ => return Err("ticket".into()),
            }
        };
        let (_, captured) = e.activity(&auth)?;
        assert_eq!(forget(&e, &f, &r, &g, 23, &snapshot).await?.status(), 200);
        let late_response = e.finish(&auth, ticket, 200, captured)?;
        assert_eq!(late_response.status(), 503);
        assert!(!String::from_utf8_lossy(late_response.body()).contains("private-host"));
        for req in [retry_revoke, retry_activity, late] {
            let reply = e.execute(req).await?;
            assert_eq!(reply.status(), 503);
            assert!(!String::from_utf8_lossy(reply.body()).contains("private-host"));
        }
        let forgotten = e.execute(control(&f, &r, &token, &g, 24, false)?).await?;
        assert_eq!(forgotten.status(), 200);
        let v: Value = serde_json::from_slice(forgotten.body())?;
        assert_eq!(v.pointer("/snapshot/state"), Some(&json!("forgotten")));
        assert_eq!(v.pointer("/snapshot/summary"), Some(&Value::Null));
        assert!(e.operation(token.as_bytes(), &g.task, &g.nonce).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn large_final_snapshot_fits_signed_response_and_keeps_full_host_identities(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let g = claims(&f, &r, &token)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        for prefix in ['a', 'b', 'c', 'd'] {
            op.record_decision(&prefix.to_string().repeat(14_000), true, None)?;
        }
        drop(op);
        let req = control(&f, &r, &token, &g, 20, true)?;
        let request_mac = req
            .headers()
            .get(http::MAC_HEADER)
            .ok_or("request mac")?
            .to_str()?
            .to_owned();
        let result = e.execute(req).await?;
        assert_eq!(result.status(), 200);
        assert!(result.body().len() <= 64 * 1024);
        let response_mac = result
            .headers()
            .get(http::MAC_HEADER)
            .ok_or("response mac")?
            .to_str()?;
        egress::control::derive_control_key(&f)?.verify_response(
            &request_mac,
            200,
            result.body(),
            response_mac,
        )?;
        let v: Value = serde_json::from_slice(result.body())?;
        assert_eq!(v.pointer("/snapshot/state"), Some(&json!("partial")));
        assert_eq!(v.pointer("/snapshot/summary/allowed"), Some(&json!(3)));
        let rows = v
            .pointer("/snapshot/summary/hosts")
            .and_then(Value::as_array)
            .ok_or("hosts")?;
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|v| v
            .get("host")
            .and_then(Value::as_str)
            .is_some_and(|h| h.len() == 14_000)));
        Ok(())
    }
    #[tokio::test]
    async fn take_away_of_unused_different_lease_still_installs_only_its_nonce_barrier(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let original = claims(&f, &r, &token)?;
        let current = e.operation(token.as_bytes(), &original.task, &original.nonce)?;
        let mut unused = original.clone();
        unused.iat += 1;
        unused.exp += 1;
        unused.nonce = NEXT_NONCE.into();
        let ut = minted(&f, &r, &unused)?;
        let reply = e.execute(control(&f, &r, &ut, &unused, 20, true)?).await?;
        assert_eq!(reply.status(), 409);
        let value: Value = serde_json::from_slice(reply.body())?;
        assert_eq!(value.get("barrierInstalled"), Some(&json!(true)));
        assert!(e
            .operation(ut.as_bytes(), &unused.task, &unused.nonce)
            .is_err());
        current.recheck()?;
        Ok(())
    }
    #[tokio::test]
    async fn first_seen_older_proofs_finalize_without_changing_current_watermark(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let old = claims(&f, &r, &token)?;
        let mut newer = old.clone();
        newer.iat += 1;
        newer.exp += 1;
        newer.nonce = NEXT_NONCE.into();
        let nt = minted(&f, &r, &newer)?;
        let current = e.operation(nt.as_bytes(), &newer.task, &newer.nonce)?;
        // Valid signature but native-ledger rejected: still finalize its owned
        // bookkeeping record rather than leaving a stale final:false report.
        assert!(e
            .operation(token.as_bytes(), &old.task, &old.nonce)
            .is_err());
        let reply = e.execute(control(&f, &r, &token, &old, 20, false)?).await?;
        let v: Value = serde_json::from_slice(reply.body())?;
        assert_eq!(v.pointer("/snapshot/final"), Some(&json!(true)));
        assert_eq!(v.pointer("/snapshot/state"), Some(&json!("interrupted")));
        current.recheck()?;
        // A previously unseen older control proof has the same bounded result,
        // but must never enter grant admission to obtain it.
        let mut unknown = old.clone();
        unknown.nonce = "CAECAwQFBgcICQoLDA0ODw".into();
        let ut = minted(&f, &r, &unknown)?;
        let reply = e
            .execute(control(&f, &r, &ut, &unknown, 21, false)?)
            .await?;
        let v: Value = serde_json::from_slice(reply.body())?;
        assert_eq!(v.pointer("/snapshot/final"), Some(&json!(true)));
        current.recheck()?;
        Ok(())
    }
    #[tokio::test]
    async fn wrong_action_take_away_signature_cannot_mutate_a_live_lease(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (e, _, f) = setup()?;
        let r = start(&e, &f).await?;
        let token = grant(&f, &r, NONCE)?;
        let g = claims(&f, &r, &token)?;
        let op = e.operation(token.as_bytes(), &g.task, &g.nonce)?;
        let req = request(
            &f,
            Route::Revoke,
            20,
            Some(&r.boot_id),
            json!({"version":1,"sessionId":r.session_id,"taskId":g.task,"leaseNonce":g.nonce,"grantIssuedAt":g.iat,"signedGrant":token,"signedRevoke":token}),
        )?;
        assert_eq!(e.execute(req).await?.status(), 503);
        assert!(!op.is_cancelled());
        op.recheck()?;
        Ok(())
    }
}
