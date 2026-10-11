//! Default-off native ChatGPT status GET, with explicitly retained startup/maintenance.
use crate::{
    chatgpt_worker::{JobInput, OwnedPrefilter, OwnedStatusWorker, Reply},
    code_net_resolver::{Resolver, Snapshot},
    config::Config,
    reply,
    serve::{Arrival, Conn},
};
use axum::{
    body::Body,
    http::{header, Request, Response, StatusCode},
};
use server_auth::{AuthConfig, Authenticator};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const STARTING: u8 = 0;
const READY: u8 = 1;
const QUARANTINED: u8 = 2;
const STOPPING: u8 = 3;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Busy,
    Conflict,
    Configuration,
    Cleanup,
}

struct Fixed {
    path: Option<PathBuf>,
    auth: AuthConfig,
    previous: Option<crate::config::PreviousStatusKey>,
    product: Option<String>,
    spec: crate::code_net_resolver::Spec,
    switch: server_store::RustAuth,
}
impl Fixed {
    fn from_config(config: &Config) -> Result<Self, Fault> {
        if config
            .data_dir
            .as_ref()
            .is_some_and(|p| !p.is_absolute() || p.as_os_str().len() > 4096)
        {
            return Err(Fault::Configuration);
        }
        let value = Self {
            path: config.data_dir.clone(),
            auth: config
                .auth
                .as_ref()
                .map_err(|_| Fault::Configuration)?
                .clone(),
            previous: config.status_previous.clone(),
            product: config.status_product_env.clone(),
            spec: config.status_resolver.clone().ok_or(Fault::Configuration)?,
            switch: config.rust_auth.ok_or(Fault::Configuration)?,
        };
        Ok(value)
    }
    fn same(&self, other: &Self) -> bool {
        self.path == other.path
            && self.auth == other.auth
            && self.product == other.product
            && self.spec == other.spec
            && self.previous.as_ref().map(|s| s.text()) == other.previous.as_ref().map(|s| s.text())
    }
}
type Initialization = JoinHandle<Result<Arc<OwnedStatusWorker>, Fault>>;
pub struct StatusOwner {
    fixed: Fixed,
    resolver: Resolver,
    stop: AtomicBool,
    state: AtomicU8,
    init: Mutex<Option<Initialization>>,
    maintenance: Mutex<Option<JoinHandle<Result<(), Fault>>>>,
    worker: Mutex<Option<Arc<OwnedStatusWorker>>>,
    startup_cleanup: Mutex<Option<server_store::bounded::Error>>,
}
static GLOBAL: Mutex<Option<Arc<StatusOwner>>> = Mutex::new(None);
impl StatusOwner {
    /// Refuse stopped/conflicting canonical owners before resolver acquisition.
    pub fn preflight(config: &Config) -> Result<(), Fault> {
        let fixed = Fixed::from_config(config)?;
        let global = GLOBAL.try_lock().map_err(|_| Fault::Busy)?;
        if let Some(owner) = global.as_ref() {
            if !owner.fixed.same(&fixed) {
                return Err(Fault::Conflict);
            }
            if owner.stop.load(Ordering::Acquire)
                || owner.state.load(Ordering::Acquire) == QUARANTINED
            {
                return Err(Fault::Cleanup);
            }
        }
        Ok(())
    }
    /// No filesystem/SQL/key loading here: construction publishes retained thread slots.
    pub fn new(config: &Config, resolver: Resolver) -> Result<Arc<Self>, Fault> {
        let fixed = Fixed::from_config(config)?;
        let mut global = GLOBAL.try_lock().map_err(|_| Fault::Busy)?;
        if let Some(owner) = global.as_ref() {
            if !owner.fixed.same(&fixed) {
                return Err(Fault::Conflict);
            }
            if owner.stop.load(Ordering::Acquire)
                || owner.state.load(Ordering::Acquire) == QUARANTINED
            {
                return Err(Fault::Cleanup);
            }
            return Ok(owner.clone());
        }
        let owner = Arc::new(Self {
            fixed,
            resolver,
            stop: AtomicBool::new(false),
            state: AtomicU8::new(STARTING),
            init: Mutex::new(None),
            maintenance: Mutex::new(None),
            worker: Mutex::new(None),
            startup_cleanup: Mutex::new(None),
        });
        *global = Some(owner.clone());
        let mut init_slot = owner.init.try_lock().map_err(|_| Fault::Busy)?;
        let mut maintenance_slot = owner.maintenance.try_lock().map_err(|_| Fault::Busy)?;
        let (init_tx, init_rx) = std::sync::mpsc::sync_channel(1);
        let worker_owner = owner.clone();
        let init = thread::Builder::new()
            .name("chatgpt-owned-initialization".into())
            .spawn(move || {
                if init_rx.recv().is_err() {
                    return Err(Fault::Cleanup);
                }
                initialize(&worker_owner)
            })
            .map_err(|_| {
                owner.state.store(QUARANTINED, Ordering::Release);
                Fault::Cleanup
            })?;
        *init_slot = Some(init);
        let (maintenance_tx, maintenance_rx) = std::sync::mpsc::sync_channel(1);
        let maintenance_owner = owner.clone();
        let maintenance = thread::Builder::new()
            .name("chatgpt-owned-maintenance".into())
            .spawn(move || {
                if maintenance_rx.recv().is_err() {
                    return Err(Fault::Cleanup);
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    maintain(&maintenance_owner)
                }))
                .unwrap_or(Err(Fault::Cleanup));
                if result.is_err() {
                    maintenance_owner
                        .state
                        .store(QUARANTINED, Ordering::Release);
                }
                result
            })
            .map_err(|_| {
                owner.stop.store(true, Ordering::Release);
                owner.state.store(QUARANTINED, Ordering::Release);
                Fault::Cleanup
            })?;
        *maintenance_slot = Some(maintenance);
        // Both handles are published before either thread starts. Release the
        // publication locks first so maintenance cannot mistake startup's own
        // lock for a conflicting lifecycle owner.
        drop(init_slot);
        drop(maintenance_slot);
        if init_tx.try_send(()).is_err() || maintenance_tx.try_send(()).is_err() {
            owner.stop.store(true, Ordering::Release);
            owner.state.store(QUARANTINED, Ordering::Release);
            return Err(Fault::Cleanup);
        }
        Ok(owner)
    }
    pub fn ready(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self.state.load(Ordering::Acquire) == READY
            && resolver_ready(&self.resolver)
    }
    /// Try-only observation of the retained worker's leases, for shutdown controls.
    pub fn active_jobs(&self) -> Option<usize> {
        self.worker().ok()?.active().ok()
    }
    fn worker(&self) -> Result<Arc<OwnedStatusWorker>, Fault> {
        if self.stop.load(Ordering::Acquire) || self.state.load(Ordering::Acquire) != READY {
            return Err(Fault::Busy);
        }
        self.worker
            .try_lock()
            .map_err(|_| Fault::Busy)?
            .as_ref()
            .cloned()
            .ok_or(Fault::Busy)
    }
    pub fn request_shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        self.state.store(STOPPING, Ordering::Release);
        self.resolver.request_shutdown();
    }
    /// Called only by the synchronous main shutdown owner, outside Tokio.
    fn shutdown_blocking(&self) -> bool {
        self.request_shutdown();
        let mut healthy = true;
        loop {
            let handle = match self.maintenance.try_lock() {
                Ok(mut s) => s.take(),
                Err(_) => {
                    thread::park_timeout(Duration::from_millis(10));
                    continue;
                }
            };
            if let Some(h) = handle {
                if !matches!(h.join(), Ok(Ok(()))) {
                    healthy = false;
                }
            }
            break;
        }
        // If maintenance failed before consuming init, the main owner joins it.
        loop {
            let handle = match self.init.try_lock() {
                Ok(mut s) => s.take(),
                Err(_) => {
                    thread::park_timeout(Duration::from_millis(10));
                    continue;
                }
            };
            if let Some(h) = handle {
                match h.join() {
                    Ok(Ok(w)) => {
                        let mut slot = match self.worker.lock() {
                            Ok(s) => s,
                            Err(e) => e.into_inner(),
                        };
                        *slot = Some(w);
                    }
                    _ => healthy = false,
                }
            }
            break;
        }
        let worker = match self.worker.lock() {
            Ok(s) => s.clone(),
            Err(e) => {
                healthy = false;
                e.into_inner().clone()
            }
        };
        if let Some(w) = worker {
            loop {
                match w.shutdown() {
                    Ok(()) => break,
                    Err(_) => {
                        healthy = false;
                        thread::park_timeout(Duration::from_millis(10));
                    }
                }
            }
        }
        // Startup validation close errors retain their actual Connection too.
        loop {
            let mut slot = match self.startup_cleanup.lock() {
                Ok(s) => s,
                Err(e) => {
                    healthy = false;
                    e.into_inner()
                }
            };
            let Some(error) = slot.take() else {
                break;
            };
            match error.retry_close() {
                Ok(()) => break,
                Err(error) => {
                    *slot = Some(error);
                    healthy = false;
                }
            }
            drop(slot);
            thread::park_timeout(Duration::from_millis(10));
        }
        loop {
            match self.resolver.shutdown() {
                Ok(()) => break,
                Err(crate::code_net_resolver::Fault::Cleanup) if !self.resolver.leased() => {
                    healthy = false;
                    break;
                }
                Err(_) => {
                    healthy = false;
                    thread::park_timeout(Duration::from_millis(10));
                }
            }
        }
        healthy
    }
}
fn initialize(owner: &StatusOwner) -> Result<Arc<OwnedStatusWorker>, Fault> {
    let mut wait = Duration::from_secs(5);
    loop {
        if owner.stop.load(Ordering::Acquire) {
            return Err(Fault::Cleanup);
        }
        if let Some(path) = owner.fixed.path.as_ref() {
            if startup_schema(owner, path)? {
                if let Ok(worker) = OwnedStatusWorker::new(
                    path.clone(),
                    Authenticator::new(owner.fixed.auth.clone()),
                    owner.fixed.switch,
                    Arc::new(OwnedPrefilter::shared(owner.resolver.clone())),
                    owner.fixed.previous.as_ref().map(|p| p.text()),
                    owner.fixed.product.as_deref(),
                ) {
                    return Ok(Arc::new(worker));
                }
            }
        }
        let next = Instant::now() + wait;
        while Instant::now() < next {
            if owner.stop.load(Ordering::Acquire) {
                return Err(Fault::Cleanup);
            }
            thread::park_timeout(Duration::from_millis(10));
        }
        wait = (wait * 2).min(Duration::from_secs(300));
    }
}
/// Bounded read-only startup schema check, no upkeep/auth/OAuth application access.
/// Failed closes are published to an already owned destination before returning.
fn startup_schema(owner: &StatusOwner, path: &std::path::Path) -> Result<bool, Fault> {
    use server_store::bounded::{BoundedStore, Budget, Fault as StoreFault, QualifiedRoot};
    let budget = Budget::new(Instant::now() + Duration::from_secs(2), 1_000_000)
        .map_err(|_| Fault::Cleanup)?;
    let root = match QualifiedRoot::open(path, &budget) {
        Ok(r) => r,
        Err(_) => return Ok(false),
    };
    let mut destination = owner.startup_cleanup.try_lock().map_err(|_| Fault::Busy)?;
    let mut reader = match BoundedStore::open(&root, &budget) {
        Ok(r) => r,
        Err(e) => {
            if e.has_pending_close() {
                *destination = Some(e);
                return Err(Fault::Cleanup);
            }
            return Ok(false);
        }
    };
    let checked = reader.read(|_| Ok(()));
    if let Err(e) = reader.close() {
        *destination = Some(e);
        return Err(Fault::Cleanup);
    }
    match checked {
        Ok(()) => Ok(true),
        Err(e) if e.fault == StoreFault::SchemaTooNew => Err(Fault::Configuration),
        Err(_) => Ok(false),
    }
}
fn maintain(owner: &StatusOwner) -> Result<(), Fault> {
    let worker = loop {
        let mut slot = owner.init.try_lock().map_err(|_| Fault::Busy)?;
        if slot.as_ref().is_some_and(JoinHandle::is_finished) {
            let mut destination = owner.worker.try_lock().map_err(|_| Fault::Busy)?;
            let handle = slot.take().ok_or(Fault::Cleanup)?;
            drop(slot);
            let worker = handle.join().map_err(|_| Fault::Cleanup)??;
            *destination = Some(worker.clone());
            break worker;
        }
        drop(slot);
        thread::park_timeout(Duration::from_millis(10));
    };
    if owner.stop.load(Ordering::Acquire) {
        return worker.shutdown().map_err(|_| Fault::Cleanup);
    }
    owner.state.store(READY, Ordering::Release);
    loop {
        if owner.stop.load(Ordering::Acquire) {
            return worker.shutdown().map_err(|_| Fault::Cleanup);
        }
        match worker.reap_owned() {
            Ok(_) | Err(crate::chatgpt_worker::ReapFault::Busy) => {}
            Err(crate::chatgpt_worker::ReapFault::Cleanup) => return Err(Fault::Cleanup),
        }
        crate::health::reap_owned_probes();
        thread::park_timeout(Duration::from_millis(10));
    }
}
pub fn resolver_ready(resolver: &Resolver) -> bool {
    resolver.snapshot().is_ok_and(|s| match s.as_ref() {
        Snapshot::Disabled => true,
        Snapshot::Ready { expires, .. } => expires.is_none_or(|t| Instant::now() < t),
        _ => false,
    })
}
/// Exact uppercase GET and WHATWG pathname; query is irrelevant to classification.
pub fn candidate(req: &Request<Body>) -> bool {
    if req.method().as_str() != "GET" {
        return false;
    }
    // Root-approved unsupported GET-path narrowing: never normalize/copy/proxy
    // an overcap path which might normalize to this native route. The same
    // admitted worker envelope emits 400 only after shared CodeNet permission.
    if req.uri().path().len() > 8192 {
        return true;
    }
    // Borrow/check path before the bounded URI/WHATWG copy; oversized query is
    // staged as ingress failure by JobInput, without copying it to authenticate.
    req.uri()
        .path()
        .parse::<axum::http::Uri>()
        .ok()
        .and_then(|u| crate::identity::whatwg_pathname_of(&u))
        .as_deref()
        == Some("/api/providers/chatgpt")
}
fn unavailable() -> Response<Body> {
    reply::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Sign-in is unavailable right now. Try again shortly.",
        true,
    )
}
fn request_wall_now(app: &crate::App) -> i64 {
    #[cfg(all(test, feature = "rust-chatgpt-status"))]
    if let Some(now) = app.status_test_now {
        return now;
    }
    #[cfg(not(all(test, feature = "rust-chatgpt-status")))]
    let _ = app;
    crate::identity::now_ms()
}
pub async fn serve(app: &crate::App, conn: Conn, req: Request<Body>) -> Response<Body> {
    if conn.local.is_none() {
        return reply::code_net_refused();
    }
    let resolver = match app.owned_resolver.as_ref().and_then(|r| r.as_ref().ok()) {
        Some(r) => r,
        None => return unavailable(),
    };
    match resolver
        .snapshot()
        .map(|s| s.refuses(conn.local.map(|a| a.ip()), Instant::now()))
    {
        Ok(Some(true)) => return reply::code_net_refused(),
        Err(_) => return unavailable(),
        _ => {}
    }
    let Some(Arrival(arrival)) = req.extensions().get::<Arrival>().copied() else {
        return unavailable();
    };
    let now = Instant::now();
    if arrival > now || now.saturating_duration_since(arrival) >= Duration::from_secs(2) {
        return unavailable();
    }
    let worker = match app
        .chatgpt_status
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .and_then(|s| s.worker().ok())
    {
        Some(w) => w,
        None => return unavailable(),
    };
    let target = req
        .uri()
        .path_and_query()
        .map_or(req.uri().path(), |s| s.as_str());
    let input = match JobInput::from_headers(
        arrival,
        request_wall_now(app),
        req.method().as_str(),
        target,
        conn.local,
        req.headers()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_bytes())),
    ) {
        Ok(Some(input)) => input,
        Err(_) => JobInput::rejected(arrival, request_wall_now(app), conn.local),
        Ok(None) => return unavailable(),
    };
    let ticket = match worker.submit(input) {
        Ok(t) => t,
        Err(_) => return unavailable(),
    };
    response(ticket.wait().await)
}
fn response(value: Reply) -> Response<Body> {
    if value.status == 403 && value.body.is_empty() {
        return reply::code_net_refused();
    }
    let status = StatusCode::from_u16(value.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    let mut response = reply::json_text(status, value.body, true);
    if value.www_authenticate {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer realm=\"cowork\""),
        );
    }
    response
}
/// Retain the process-global owner through real joins/reaps after HTTP drain.
pub fn shutdown_process_owners() -> bool {
    let owner = loop {
        match GLOBAL.try_lock() {
            Ok(s) => break s.clone(),
            Err(_) => thread::park_timeout(Duration::from_millis(10)),
        }
    };
    owner.is_none_or(|o| o.shutdown_blocking())
}

#[cfg(all(test, feature = "rust-chatgpt-status"))]
#[allow(clippy::unwrap_used)]
mod m6_pending_controls {
    use super::*;
    use server_store::rusqlite::Connection;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn m6_shared_pending_original_deadline_and_cancel() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("secrets.key"), [7; 32]).unwrap();
        let database = Connection::open(server_store::db_path(directory.path())).unwrap();
        database
            .execute_batch(include_str!(
                "../../../crates/server-store/tests/fixtures/node-schema.sql"
            ))
            .unwrap();
        database.execute_batch("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL); INSERT INTO settings VALUES('feature:chatgptOAuth','true');").unwrap();
        let now = crate::identity::now_ms();
        database.execute_batch(&format!("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','A','a','A','admin','x','w',1,1); INSERT INTO sessions VALUES('{}','u','c',1,{}, {},'ua','ip');", server_auth::js::digest("live"), now-1000, now+99999)).unwrap();
        let config = Config::from_lookup(|k| match k {
            "NOEVIA_RUST_AUTH" | "NOEVIA_RUST_CHATGPT_STATUS" => Some("1".into()),
            "UI_DATA_DIR" => Some(directory.path().to_str().unwrap().into()),
            "NOEVIA_LEGACY_UPSTREAM" => Some("http://127.0.0.1:1".into()),
            "COWORK_CODE_NET_ADDR" => Some("blocked,127.0.0.2".into()),
            _ => None,
        })
        .unwrap();
        let resolver = crate::code_net_resolver::closed_fixture(
            config.status_resolver.clone().unwrap(),
            "/tmp/m6-resolver-fixture-v1".into(),
        )
        .unwrap();
        let owner = StatusOwner::new(&config, resolver.clone()).unwrap();
        let mut legacy = config.clone();
        legacy.rust_chatgpt_status = false;
        legacy.code_net = crate::code_net::CodeNetSpec::default();
        let mut app = crate::App::new(legacy);
        let raw = Arc::get_mut(&mut app).unwrap();
        raw.config = config;
        raw.owned_resolver = Some(Ok(resolver.clone()));
        raw.chatgpt_status = Some(Ok(owner.clone()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.worker().is_err() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(owner.worker().is_ok(), "M6_PENDING_ASSERT/owned-worker");
        assert!(!owner.ready(), "M6_PENDING_ASSERT/readiness");
        let local = Conn {
            peer: "127.0.0.1:1".parse().unwrap(),
            local: Some("127.0.0.1:1".parse().unwrap()),
        };
        let mut request = Request::builder()
            .uri("/api/providers/chatgpt")
            .header("cookie", "cowork_session=live")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(Arrival(Instant::now() - Duration::from_millis(1500)));
        let started = Instant::now();
        let response = serve(&app, local, request).await;
        let elapsed = started.elapsed();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "M6_PENDING_ASSERT/fail-closed"
        );
        assert!(
            elapsed >= Duration::from_millis(400) && elapsed < Duration::from_millis(1500),
            "M6_PENDING_ASSERT/original-deadline"
        );
        assert_eq!(
            database
                .query_row("SELECT last_seen_at FROM sessions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            now - 1000,
            "M6_PENDING_ASSERT/no-sql"
        );
        let mut denied = Request::builder()
            .uri("/api/providers/chatgpt")
            .header("cookie", "x".repeat(8193))
            .body(Body::empty())
            .unwrap();
        denied.extensions_mut().insert(Arrival(Instant::now()));
        let denied = serve(
            &app,
            Conn {
                local: Some("127.0.0.2:1".parse().unwrap()),
                ..local
            },
            denied,
        )
        .await;
        assert_eq!(
            denied.status(),
            StatusCode::FORBIDDEN,
            "M6_PENDING_ASSERT/known-deny-first"
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while owner.active_jobs() != Some(0) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            owner.active_jobs(),
            Some(0),
            "M6_PENDING_ASSERT/actual-reap"
        );
        let mut request = Request::builder()
            .uri("/api/providers/chatgpt")
            .header("cookie", "cowork_session=live")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(Arrival(Instant::now()));
        let cancelled_app = app.clone();
        let cancelled = tokio::spawn(async move { serve(&cancelled_app, local, request).await });
        let deadline = Instant::now() + Duration::from_secs(1);
        while owner.active_jobs() != Some(1) && Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        let admitted = owner.active_jobs() == Some(1);
        cancelled.abort();
        let _ = cancelled.await;
        let deadline = Instant::now() + Duration::from_secs(1);
        while owner.active_jobs() != Some(0) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let reaped = owner.active_jobs() == Some(0);
        owner.request_shutdown();
        let joined = std::thread::spawn(shutdown_process_owners);
        while !joined.is_finished() {
            tokio::task::yield_now().await;
        }
        let shutdown = joined.join().unwrap();
        assert!(admitted && reaped, "M6_PENDING_ASSERT/cancel-and-reap");
        assert!(
            shutdown && !resolver.leased(),
            "M6_PENDING_ASSERT/controller-joined-reaped"
        );
    }
}
