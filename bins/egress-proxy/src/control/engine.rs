//! Integrated buffered Ready/renew controller and operation drain registry.
//! Not wired to listener/forwarding. Actual transport, idle/policy integration,
//! scoped revoke/activity/forget and permission-checked key-file adapter remain held.
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
    task: String,
    nonce: String,
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
        let id = st.next_operation.checked_add(1).ok_or(Error::Capacity)?;
        let admitted = st
            .ledger
            .admit(&claims, wall()?)
            .map_err(|_| Error::Authentication)?;
        if admitted == GrantAdmission::New {
            for o in st
                .operations
                .values()
                .filter(|o| o.task == task && o.nonce != nonce)
            {
                o.cancel.send_replace(true);
            }
        }
        let session_id = st.session_id.clone().ok_or(Error::Unavailable)?;
        let (send, cancel) = watch::channel(false);
        st.next_operation = id;
        st.operations.insert(
            id,
            Operation {
                task: task.into(),
                nonce: nonce.into(),
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
                    _ => Ok((503, terminal(&auth, "unavailable"))),
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
        status: u16,
        body: Value,
    ) -> Result<Response<Vec<u8>>, Error> {
        let bytes = serde_json::to_vec(&body).map_err(|_| Error::State)?;
        let result = {
            let mut st = self.inner.state.lock().map_err(|_| Error::State)?;
            st.replay.complete(ticket, status, bytes.clone())
        };
        if result.is_err() {
            return reply(auth, 503, "uncertain");
        }
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
        st.transition = None;
        let result = live_response(auth, "ready", proof);
        drop(st);
        drop(transition);
        Ok((200, result))
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
    match inner.source.current() {
        Ok(k) => Ok(k),
        Err(_) => {
            let mut st = inner.state.lock().map_err(|_| Error::State)?;
            sync_key(&mut st, None)?;
            Err(Error::Key)
        }
    }
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
    struct Keys(Mutex<Option<FileKey>>);
    impl KeyProvider for Keys {
        fn current(&self) -> Result<FileKey, Error> {
            self.0
                .lock()
                .map_err(|_| Error::Key)?
                .clone()
                .ok_or(Error::Key)
        }
    }
    fn setup() -> Result<(Engine, Arc<Keys>, FileKey), Error> {
        let f = FileKey::from_bytes([7; 32]);
        let k = Arc::new(Keys(Mutex::new(Some(f.clone()))));
        let e = Engine::with_timeout(k.clone(), Duration::from_millis(500))?;
        Ok((e, k, f))
    }
    fn request(
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
    fn initial(file: &FileKey, id: u64) -> Result<Request<Vec<u8>>, Box<dyn std::error::Error>> {
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
    struct Ready {
        boot_id: String,
        session_id: String,
    }
    async fn start(e: &Engine, f: &FileKey) -> Result<Ready, Box<dyn std::error::Error>> {
        let r = e.execute(initial(f, 1)?).await?;
        assert_eq!(r.status(), 200);
        Ok(serde_json::from_slice(r.body())?)
    }
    fn unhex(s: &str) -> Result<[u8; 16], Box<dyn std::error::Error>> {
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
    fn grant(f: &FileKey, r: &Ready, nonce: &str) -> Result<String, Box<dyn std::error::Error>> {
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
    const NONCE: &str = "AAECAwQFBgcICQoLDA0ODw";
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
    async fn unimplemented_controls_never_report_success_or_install_network_authority(
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
        assert_eq!(result.status(), 503);
        assert!(String::from_utf8_lossy(result.body()).contains("unavailable"));
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
