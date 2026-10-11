//! Private owned worker substrate. No HTTP interception or real resolver lifecycle is wired.
use chatgpt_status::{ExternalContext, ExternalFailure, ExternalStatus};
use server_auth::{Authenticator, Creds, Refusal};
use server_store::{
    bounded::{
        BoundedStore, BoundedUpkeepWriter, Budget, Error as StoreFailure, Fault, QualifiedRoot,
    },
    RustAuth,
};
use sha2::{Digest, Sha256};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub www_authenticate: bool,
}
fn unavailable() -> Reply {
    Reply {
        status: 503,
        body: "{\"error\":\"Sign-in is unavailable right now. Try again shortly.\"}".into(),
        www_authenticate: false,
    }
}
fn bad_input() -> Reply {
    Reply {
        status: 400,
        body: "{\"error\":\"invalid URL\"}".into(),
        www_authenticate: false,
    }
}
fn status_failure() -> Reply {
    Reply {
        status: 502,
        body: "{\"error\":\"ChatGPT sign-in failed.\"}".into(),
        www_authenticate: false,
    }
}
fn refused(r: Refusal) -> Reply {
    match r{Refusal::Unauthorized=>Reply{status:401,body:"{\"error\":\"unauthorized\"}".into(),www_authenticate:true},Refusal::Csrf=>Reply{status:403,body:"{\"error\":\"invalid CSRF token\"}".into(),www_authenticate:false},Refusal::BrowserOnly=>Reply{status:403,body:"{\"error\":\"This needs a signed-in browser session.\",\"code\":\"browser_session_required\"}".into(),www_authenticate:false}}
}
/// Raw header validation happens before Latin1 conversion/join. No body is consumed.
pub struct JobInput {
    arrival: Instant,
    now_ms: i64,
    creds: Creds,
    local: Option<SocketAddr>,
    digest: [u8; 32],
}
impl JobInput {
    pub fn from_headers<'a>(
        arrival: Instant,
        now_ms: i64,
        method: &str,
        target: &str,
        local: Option<SocketAddr>,
        headers: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Result<Option<Self>, Reply> {
        if target.len() > 8192 || method.len() > 32 {
            return Err(bad_input());
        }
        if method != "GET" {
            return Ok(None);
        }
        let uri = target.parse::<axum::http::Uri>().map_err(|_| bad_input())?;
        if crate::identity::whatwg_pathname_of(&uri).as_deref() != Some("/api/providers/chatgpt") {
            return Ok(None);
        }
        let mut relevant = Vec::new();
        let mut count = 0usize;
        let mut bytes = 0usize;
        for (name, value) in headers {
            count += 1;
            if count > 64 || name.len() > 128 {
                return Err(bad_input());
            }
            if ["cookie", "authorization", "origin", "x-csrf-token"]
                .iter()
                .any(|n| name.eq_ignore_ascii_case(n))
            {
                bytes = bytes
                    .checked_add(value.len())
                    .filter(|n| *n <= 8192)
                    .ok_or_else(bad_input)?;
                relevant.push((name, value));
            }
        }
        let creds = Creds::from_headers(method, relevant);
        let mut digest = Sha256::new();
        digest.update(b"noevia/chatgpt/preauth/v1");
        for value in [
            creds.cookie.as_deref().unwrap_or(""),
            creds.authorization.as_deref().unwrap_or(""),
            method,
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        Ok(Some(Self {
            arrival,
            now_ms,
            creds,
            local,
            digest: digest.finalize().into(),
        }))
    }
}
#[derive(Clone)]
enum PrefilterState {
    Ready(Arc<Vec<Vec<u16>>>),
    Pending,
    Broken,
}
/// Injected owned snapshot only. No lookup or detached task exists in this module.
pub struct OwnedPrefilter {
    enabled: bool,
    state: Mutex<PrefilterState>,
}
impl OwnedPrefilter {
    pub fn ready(enabled: bool, addresses: Vec<String>) -> Result<Self, Reply> {
        Ok(Self {
            enabled,
            state: Mutex::new(Self::addresses(addresses)?),
        })
    }
    pub fn pending() -> Self {
        Self {
            enabled: true,
            state: Mutex::new(PrefilterState::Pending),
        }
    }
    fn addresses(addresses: Vec<String>) -> Result<PrefilterState, Reply> {
        if addresses.len() > 256
            || addresses
                .iter()
                .any(|s| s.len() > 64 || s.parse::<std::net::IpAddr>().is_err())
        {
            return Err(unavailable());
        }
        Ok(PrefilterState::Ready(Arc::new(
            addresses
                .iter()
                .map(|s| s.encode_utf16().collect())
                .collect(),
        )))
    }
    pub fn publish(&self, addresses: Vec<String>) -> Result<(), Reply> {
        let next = Self::addresses(addresses)?;
        *self.state.try_lock().map_err(|_| unavailable())? = next;
        Ok(())
    }
    pub fn mark_broken(&self) -> Result<(), Reply> {
        *self.state.try_lock().map_err(|_| unavailable())? = PrefilterState::Broken;
        Ok(())
    }
    fn check(&self, local: Option<SocketAddr>) -> Result<Option<bool>, Reply> {
        if !self.enabled {
            return Ok(Some(false));
        }
        let Some(local) = local else {
            return Ok(Some(true));
        };
        let snapshot = self.state.try_lock().map_err(|_| unavailable())?.clone();
        match snapshot {
            PrefilterState::Pending => Ok(None),
            PrefilterState::Broken => Ok(Some(true)),
            PrefilterState::Ready(a) => {
                let refs: Vec<&[u16]> = a.iter().map(Vec::as_slice).collect();
                let local: Vec<u16> = local.ip().to_string().encode_utf16().collect();
                Ok(Some(
                    code_net_guard::refuses(&refs, Some(&local)).unwrap_or(true),
                ))
            }
        }
    }
}
#[derive(Clone)]
pub struct Cancellation {
    budget: Budget,
    status: ExternalContext,
}
impl Cancellation {
    pub fn cancel(&self) {
        self.budget.cancellation().cancel();
        self.status.cancel();
    }
}
enum Cleanup {
    Store(StoreFailure),
    Status(ExternalFailure),
    #[cfg(test)]
    Test(Arc<std::sync::atomic::AtomicBool>),
}
impl Cleanup {
    fn retry(self) -> Result<(), Self> {
        match self {
            Self::Store(e) => e.retry_close().map_err(Self::Store),
            Self::Status(e) => e.retry_close().map_err(Self::Status),
            #[cfg(test)]
            Self::Test(done) => {
                if done.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(Self::Test(done))
                }
            }
        }
    }
}
struct Done {
    cleanup: Vec<Cleanup>,
}
struct Entry {
    id: u64,
    service: u64,
    digest: [u8; 32],
    owner: Mutex<Option<String>>,
    signal: Cancellation,
    handle: Mutex<Option<JoinHandle<Done>>>,
    joined: Mutex<Option<Done>>,
}
static GLOBAL: Mutex<Vec<Arc<Entry>>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(1);
fn next() -> Result<u64, Reply> {
    NEXT.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
        .map_err(|_| unavailable())
}
#[must_use]
pub struct OwnedStatusWorker {
    inner: Arc<Inner>,
}
#[cfg(test)]
type TestHook = Arc<dyn Fn(&str) + Send + Sync>;
struct Inner {
    id: u64,
    path: PathBuf,
    auth: Authenticator,
    switch: RustAuth,
    status: ExternalStatus,
    prefilter: Arc<OwnedPrefilter>,
    closed: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    hook: Option<TestHook>,
    #[cfg(test)]
    retained_cleanup: Option<Arc<std::sync::atomic::AtomicBool>>,
}
pub struct JobTicket {
    rx: Option<tokio::sync::oneshot::Receiver<Reply>>,
    signal: Cancellation,
    deadline: Instant,
    received: bool,
}
impl JobTicket {
    pub fn cancellation(&self) -> Cancellation {
        self.signal.clone()
    }
    pub async fn wait(mut self) -> Reply {
        let Some(rx) = self.rx.take() else {
            return unavailable();
        };
        let result =
            tokio::time::timeout(self.deadline.saturating_duration_since(Instant::now()), rx).await;
        self.received = true;
        match result {
            Ok(Ok(reply)) => reply,
            _ => {
                self.signal.cancel();
                unavailable()
            }
        }
    }
}
impl Drop for JobTicket {
    fn drop(&mut self) {
        if !self.received {
            self.signal.cancel();
        }
    }
}
impl OwnedStatusWorker {
    pub fn new(
        path: PathBuf,
        auth: Authenticator,
        switch: RustAuth,
        prefilter: Arc<OwnedPrefilter>,
        previous: Option<&str>,
        product_env: Option<&str>,
    ) -> Result<Self, Reply> {
        let path = std::fs::canonicalize(path).map_err(|_| unavailable())?;
        let status =
            ExternalStatus::open(&path, previous, product_env).map_err(|_| status_failure())?;
        Ok(Self {
            inner: Arc::new(Inner {
                id: next()?,
                path,
                auth,
                switch,
                status,
                prefilter,
                closed: std::sync::atomic::AtomicBool::new(false),
                #[cfg(test)]
                hook: None,
                #[cfg(test)]
                retained_cleanup: None,
            }),
        })
    }
    pub fn submit(&self, input: JobInput) -> Result<JobTicket, Reply> {
        let now = Instant::now();
        if input.arrival > now
            || now.saturating_duration_since(input.arrival) >= Duration::from_secs(2)
            || self.inner.closed.load(Ordering::Acquire)
        {
            return Err(unavailable());
        }
        let deadline = input.arrival + Duration::from_secs(2);
        let budget = Budget::new(deadline, 1_000_000).map_err(|_| unavailable())?;
        let status = ExternalContext::new(deadline);
        let signal = Cancellation { budget, status };
        let mut active = GLOBAL.try_lock().map_err(|_| unavailable())?;
        if active.len() >= 8 || active.iter().any(|e| e.digest == input.digest) {
            return Err(unavailable());
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        let entry = Arc::new(Entry {
            id: next()?,
            service: self.inner.id,
            digest: input.digest,
            owner: Mutex::new(None),
            signal: signal.clone(),
            handle: Mutex::new(None),
            joined: Mutex::new(None),
        });
        let (tx, rx) = tokio::sync::oneshot::channel();
        let inner = self.inner.clone();
        let worker = entry.clone();
        let mut handle = entry.handle.try_lock().map_err(|_| unavailable())?;
        let (started, start) = std::sync::mpsc::sync_channel(1);
        let task = thread::Builder::new()
            .name("chatgpt-owned-job".into())
            .spawn(move || {
                if start.recv().is_err() {
                    let _ = tx.send(unavailable());
                    return Done {
                        cleanup: Vec::new(),
                    };
                }
                let (reply, done) = pipeline(&inner, &worker, input);
                let _ = tx.send(reply);
                done
            })
            .map_err(|_| unavailable())?;
        *handle = Some(task);
        active.push(entry.clone());
        drop(handle);
        drop(active);
        let _ = started.send(());
        Ok(JobTicket {
            rx: Some(rx),
            signal,
            deadline,
            received: false,
        })
    }
    /// Try-only registry access. Kernel close/join may stall; ownership/permits remain retained.
    pub fn reap(&self) -> Result<usize, Reply> {
        let entries: Vec<_> = GLOBAL
            .try_lock()
            .map_err(|_| unavailable())?
            .iter()
            .filter(|e| e.service == self.inner.id)
            .cloned()
            .collect();
        let mut released = 0;
        for entry in entries {
            let mut slot = entry.joined.try_lock().map_err(|_| unavailable())?;
            let h = {
                let mut slot = entry.handle.try_lock().map_err(|_| unavailable())?;
                if slot.as_ref().is_some_and(JoinHandle::is_finished) {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(h) = h {
                match h.join() {
                    Ok(done) => *slot = Some(done),
                    Err(_) => return Err(unavailable()),
                }
            }
            if let Some(done) = slot.as_mut() {
                let pending = std::mem::take(&mut done.cleanup);
                for e in pending {
                    if let Err(e) = e.retry() {
                        done.cleanup.push(e);
                    }
                }
                if done.cleanup.is_empty() {
                    let mut active = GLOBAL.try_lock().map_err(|_| unavailable())?;
                    let n = active.len();
                    active.retain(|e| e.id != entry.id);
                    released += n - active.len();
                }
            }
        }
        Ok(released)
    }
    pub fn active(&self) -> Result<usize, Reply> {
        Ok(GLOBAL
            .try_lock()
            .map_err(|_| unavailable())?
            .iter()
            .filter(|e| e.service == self.inner.id)
            .count())
    }
    pub fn shutdown(&self) -> Result<(), Reply> {
        self.inner.closed.store(true, Ordering::Release);
        let entries: Vec<_> = GLOBAL
            .try_lock()
            .map_err(|_| unavailable())?
            .iter()
            .filter(|e| e.service == self.inner.id)
            .cloned()
            .collect();
        for e in &entries {
            e.signal.cancel();
        }
        for e in entries {
            let mut joined = e.joined.try_lock().map_err(|_| unavailable())?;
            let h = e.handle.try_lock().map_err(|_| unavailable())?.take();
            if let Some(h) = h {
                let done = h.join().map_err(|_| unavailable())?;
                *joined = Some(done);
            }
        }
        self.reap()?;
        if self.active()? != 0 {
            return Err(unavailable());
        }
        Ok(())
    }
}
fn promote(entry: &Entry, owner: &str) -> Result<(), Reply> {
    if owner.is_empty() || owner.len() > 1024 {
        return Err(unavailable());
    }
    let active = GLOBAL.try_lock().map_err(|_| unavailable())?;
    for e in active.iter() {
        if e.id != entry.id
            && e.owner.try_lock().map_err(|_| unavailable())?.as_deref() == Some(owner)
        {
            return Err(unavailable());
        }
    }
    *entry.owner.try_lock().map_err(|_| unavailable())? = Some(owner.to_owned());
    Ok(())
}
fn hook(inner: &Inner, phase: &str) {
    #[cfg(test)]
    if let Some(h) = &inner.hook {
        h(phase)
    };
    #[cfg(not(test))]
    let _ = (inner, phase);
}
fn pipeline(inner: &Inner, entry: &Entry, input: JobInput) -> (Reply, Done) {
    let mut cleanup = Vec::new();
    let result = (|| -> Result<Reply, Reply> {
        hook(inner, "start");
        loop {
            entry.signal.budget.check().map_err(|_| unavailable())?;
            match inner.prefilter.check(input.local)? {
                Some(true) => {
                    return Ok(Reply {
                        status: 403,
                        body: String::new(),
                        www_authenticate: false,
                    })
                }
                Some(false) => break,
                None => thread::park_timeout(Duration::from_millis(5)),
            }
        }
        let root =
            QualifiedRoot::open(&inner.path, &entry.signal.budget).map_err(|_| unavailable())?;
        match BoundedUpkeepWriter::open(&root, inner.switch, &entry.signal.budget) {
            Ok(mut w) => {
                let result = w.write(|tx| inner.auth.upkeep_in(tx, &input.creds, input.now_ms));
                if let Err(e) = w.close() {
                    cleanup.push(Cleanup::Store(e));
                    return Err(unavailable());
                }
                if let Err(e) = result {
                    if !matches!(e.fault, Fault::Unavailable | Fault::Busy) {
                        return Err(unavailable());
                    }
                }
            }
            Err(e) => {
                if e.has_pending_close() {
                    cleanup.push(Cleanup::Store(e));
                    return Err(unavailable());
                }
                if !matches!(e.fault, Fault::Unavailable | Fault::Busy) {
                    return Err(unavailable());
                }
            }
        }
        hook(inner, "after_upkeep");
        entry.signal.budget.check().map_err(|_| unavailable())?;
        let mut r = match BoundedStore::open(&root, &entry.signal.budget) {
            Ok(r) => r,
            Err(e) => {
                if e.has_pending_close() {
                    cleanup.push(Cleanup::Store(e));
                }
                return Err(unavailable());
            }
        };
        let identity = r.read(|r| {
            inner
                .auth
                .gate_in(r, &input.creds, "/api/providers/chatgpt", input.now_ms)
        });
        if let Err(e) = r.close() {
            cleanup.push(Cleanup::Store(e));
            return Err(unavailable());
        }
        let identity = identity.map_err(|_| unavailable())?.map_err(refused)?;
        promote(entry, &identity.user.id)?;
        hook(inner, "after_owner");
        entry.signal.budget.check().map_err(|_| unavailable())?;
        entry.signal.status.check().map_err(|_| unavailable())?;
        let mut status = match inner.status.job(entry.signal.status.clone()) {
            Ok(job) => job,
            Err(e) => {
                if e.has_pending_close() {
                    cleanup.push(Cleanup::Status(e));
                }
                return Err(if entry.signal.status.check().is_err() {
                    unavailable()
                } else {
                    status_failure()
                });
            }
        };
        let answer = status.read(&identity.user.id);
        if let Err(e) = status.close() {
            cleanup.push(Cleanup::Status(e));
            return Err(status_failure());
        }
        entry.signal.budget.check().map_err(|_| unavailable())?;
        match answer {
            Ok((status, body)) => Ok(Reply {
                status,
                body,
                www_authenticate: false,
            }),
            Err(_) => Err(if entry.signal.status.check().is_err() {
                unavailable()
            } else {
                status_failure()
            }),
        }
    })();
    hook(inner, "before_exit");
    #[cfg(test)]
    if let Some(done) = &inner.retained_cleanup {
        cleanup.push(Cleanup::Test(done.clone()));
    }
    (result.unwrap_or_else(|e| e), Done { cleanup })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod owned_tests {
    use super::*;
    use server_auth::AuthConfig;
    use server_store::rusqlite::Connection;
    const NOW: i64 = 1_800_000_000_000;
    fn setup(prefilter: Arc<OwnedPrefilter>) -> (tempfile::TempDir, Connection, OwnedStatusWorker) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("secrets.key"), [7; 32]).unwrap();
        let c = Connection::open(server_store::db_path(d.path())).unwrap();
        c.execute_batch(include_str!(
            "../../../crates/server-store/tests/fixtures/node-schema.sql"
        ))
        .unwrap();
        c.execute_batch(&format!("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL); INSERT INTO settings VALUES('feature:chatgptOAuth','true'); INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','A','a','A','admin','x','w',1,1); INSERT INTO sessions VALUES('{}','u','c',1,{}, {},'ua','ip');",server_auth::js::digest("live"),NOW-1000,NOW+99999)).unwrap();
        let auth = Authenticator::new(AuthConfig {
            public_origin: String::new(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: true,
            native_client_auth_env: Some(true),
            origin_from_settings: false,
        });
        let w = OwnedStatusWorker::new(
            d.path().to_owned(),
            auth,
            RustAuth::from_env_value(Some("1")).unwrap(),
            prefilter,
            None,
            None,
        )
        .unwrap();
        (d, c, w)
    }
    fn input(cookie: &str) -> JobInput {
        JobInput::from_headers(
            Instant::now(),
            NOW,
            "GET",
            "/api/providers/chatgpt",
            Some("127.0.0.1:8021".parse().unwrap()),
            [("cookie", cookie.as_bytes())],
        )
        .unwrap()
        .unwrap()
    }
    #[tokio::test]
    async fn global_pre_auth_drop_and_confirmed_join() {
        let (_d, _c, w) = setup(Arc::new(OwnedPrefilter::pending()));
        let mut tickets = Vec::new();
        for i in 0..8 {
            tickets.push(w.submit(input(&format!("cowork_session=p{i}"))).unwrap());
        }
        assert!(w.submit(input("cowork_session=p0")).is_err());
        drop(tickets);
        assert_eq!(w.active().unwrap(), 8);
        let ninth = w.submit(input("cowork_session=ninth"));
        let admitted = ninth.is_ok();
        drop(ninth);
        w.shutdown().unwrap();
        assert!(!admitted, "ROUTING_ASSERT/worker/global");
        assert_eq!(w.active().unwrap(), 0);
    }
    #[tokio::test]
    async fn owner_cancel_before_status_and_old_signal() {
        use std::sync::atomic::AtomicBool;
        let (_d, _c, mut w) = setup(Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let gate2 = gate.clone();
        let first = Arc::new(AtomicBool::new(false));
        Arc::get_mut(&mut w.inner).unwrap().hook = Some(Arc::new(move |phase| {
            if phase == "after_owner" && !first.swap(true, Ordering::AcqRel) {
                ready_tx.send(()).unwrap();
                let (lock, cv) = &*gate2;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cv.wait(released).unwrap();
                }
            }
        }));
        let ticket = w.submit(input("cowork_session=live")).unwrap();
        let old = ticket.cancellation();
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = w
            .submit(input("cowork_session=bad; cowork_session=live"))
            .unwrap()
            .wait()
            .await;
        assert_eq!(second.status, 503, "ROUTING_ASSERT/worker/owner");
        drop(ticket);
        assert_eq!(w.active().unwrap(), 2);
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        w.shutdown().unwrap();
        let (_d, _c, new) = setup(Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        old.cancel();
        let reply = new
            .submit(input("cowork_session=live"))
            .unwrap()
            .wait()
            .await;
        new.shutdown().unwrap();
        assert_eq!(reply.status, 200);
    }
    #[tokio::test]
    async fn quarantine_keeps_all_permits_until_cleanup() {
        use std::sync::atomic::AtomicBool;
        let (_d, _c, mut w) = setup(Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        let done = Arc::new(AtomicBool::new(false));
        Arc::get_mut(&mut w.inner).unwrap().retained_cleanup = Some(done.clone());
        for i in 0..8 {
            let reply = w
                .submit(input(&format!("cowork_session=invalid{i}")))
                .unwrap()
                .wait()
                .await;
            assert_eq!(reply.status, 401);
        }
        let refused = w.shutdown().is_err();
        let held = w.active().unwrap();
        done.store(true, Ordering::Release);
        w.reap().unwrap();
        assert!(refused && held == 8, "ROUTING_ASSERT/worker/quarantine");
        assert_eq!(w.active().unwrap(), 0);
    }
    #[tokio::test]
    async fn joined_slot_contention_retains_handle() {
        let (_d, _c, w) = setup(Arc::new(OwnedPrefilter::ready(false, vec![]).unwrap()));
        let reply = w
            .submit(input("cowork_session=invalid"))
            .unwrap()
            .wait()
            .await;
        assert_eq!(reply.status, 401);
        let entry = GLOBAL
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.service == w.inner.id)
            .unwrap()
            .clone();
        let guard = entry.joined.lock().unwrap();
        assert!(w.shutdown().is_err());
        assert!(
            entry.handle.lock().unwrap().is_some(),
            "ROUTING_ASSERT/worker/join_contention"
        );
        drop(guard);
        w.shutdown().unwrap();
        assert_eq!(w.active().unwrap(), 0);
    }
    #[tokio::test]
    async fn pending_prefilter_deadline_and_broken() {
        let (_d, _c, w) = setup(Arc::new(OwnedPrefilter::pending()));
        let mut request = input("cowork_session=live");
        request.arrival = Instant::now() - Duration::from_millis(1990);
        let reply = w.submit(request).unwrap().wait().await;
        assert_eq!(reply.status, 503);
        w.shutdown().unwrap();
        let p = Arc::new(OwnedPrefilter::pending());
        p.mark_broken().unwrap();
        let (_d, _c, w) = setup(p);
        let reply = w.submit(input("cowork_session=live")).unwrap().wait().await;
        assert_eq!(reply.status, 403);
        assert!(reply.body.is_empty());
        w.shutdown().unwrap();
    }
}
