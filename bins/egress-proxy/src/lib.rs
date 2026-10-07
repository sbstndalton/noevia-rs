//! Deny-by-default egress proxy for sandboxed coding tasks: the Rust port of the server half of
//! noevia's `apps/web/server/code-egress.cjs`, on top of the `egress` policy crate.
//!
//! * `CONNECT host:port` opens a tunnel; an absolute-URI `http://` request is forwarded.
//! * Every request needs `Proxy-Authorization: Basic base64(any:token)` for a live grant
//!   (407 otherwise), a target on 80/443 whose host is on the grant's list (403), and a host
//!   whose every address is public (403). The name is resolved **once**, and the connection is
//!   made to the first checked address — never to a second lookup (no DNS rebinding).
//! * `Proxy-Authorization` and the other hop-by-hop headers never travel upstream.
//! * Grants expire after their idle TTL; an expired grant takes its live tunnels with it.
//! * Refusals are logged as JSON lines with the task id; tokens, paths and headers never are.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use egress::{
    basic_token, check_addresses, check_target, looks_signed, status_text, verify, Act, Admission,
    Allowed, Expired, Grant, GrantError, GrantId, GrantKey, Ledger, Refusal, SignedGrant,
    TokenStore, ALLOWED_PORTS, DEFAULT_TOKEN_TTL_MS,
};
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

/// A connection slot. Held by the client connection and, after an upgrade or while a forward
/// is in flight, by the tunnel / upstream task too, so a slot frees only when all are gone.
type Slot = Arc<OwnedSemaphorePermit>;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Body = BoxBody<Bytes, hyper::Error>;

/// Resolves a host name to every address it has, in resolver order. Injected so tests can
/// play a rebinding resolver.
pub trait Resolver: Send + Sync + 'static {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>>;
}

/// Opens the upstream TCP connection to an already-checked address.
pub trait Connector: Send + Sync + 'static {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>>;
}

/// Millisecond clock for idle TTLs (the JS `now()`).
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

/// The system resolver (getaddrinfo via tokio).
pub struct SystemResolver;
impl Resolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host, 0u16)).await?;
            Ok(addrs.map(|a| a.ip()).collect())
        })
    }
}

/// Plain `connect(2)` to the given address.
pub struct DirectConnector;
impl Connector for DirectConnector {
    fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
        Box::pin(TcpStream::connect(addr))
    }
}

/// Monotonic milliseconds since the proxy started.
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Unix milliseconds. Signed grants carry absolute `iat`/`exp` minted by the web on the same
/// host, so the proxy's default clock is the wall clock.
#[derive(Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Where the grant HMAC key comes from. A file is re-read at most every `KEY_RELOAD`, so a key
/// the web writes after the proxy starts (or rewrites after a secrets rotation) is picked up.
pub enum KeySource {
    Static(GrantKey),
    File(std::path::PathBuf),
}

const KEY_RELOAD: Duration = Duration::from_secs(5);

struct KeyCache {
    key: Option<GrantKey>,
    read_at: Option<Instant>,
}

/// Loads a key file (64 hex digits). The error never contains the file's contents.
pub fn read_key_file(path: &std::path::Path) -> Result<GrantKey, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("grant key file {}: {}", path.display(), e.kind()))?;
    GrantKey::from_hex(&text).map_err(|e| format!("grant key file {}: {e}", path.display()))
}

/// Where structured log lines go.
pub type LogSink = Arc<dyn Fn(Value) + Send + Sync>;

/// One JSON object per line on stderr.
pub fn stderr_log() -> LogSink {
    Arc::new(|v: Value| eprintln!("{v}"))
}

/// Limits and timeouts.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Concurrent client connections; extra ones are closed on accept.
    pub max_connections: usize,
    /// Concurrent CONNECT tunnels and in-flight forwarded requests per task; the next one is
    /// refused with 429. npm keeps at most 15 sockets per registry and pip a pool of 10, so 64
    /// is generous, yet four busy tasks still fit under `max_connections`.
    pub max_connections_per_task: usize,
    /// Request head buffer (hyper's minimum, 8192, is enforced).
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub header_timeout: Duration,
    pub resolve_timeout: Duration,
    pub connect_timeout: Duration,
    /// A CONNECT tunnel with no bytes in either direction for this long is closed.
    pub tunnel_idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            max_connections_per_task: 64,
            max_header_bytes: 16 * 1024,
            max_headers: 100,
            header_timeout: Duration::from_secs(30),
            resolve_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            tunnel_idle_timeout: Duration::from_secs(10 * 60),
        }
    }
}

struct State {
    store: TokenStore,
    /// Fires (true) when a grant is dropped, closing every connection it owns.
    cancels: HashMap<GrantId, watch::Sender<bool>>,
    /// Open tunnels / in-flight forwards per task id (keyed by task, so a re-grant does not
    /// reset the count).
    open_by_task: HashMap<String, usize>,
    /// Replay / supersession memory for signed grants.
    ledger: Ledger,
    /// Installed signed grants: their task, nonce and absolute expiry.
    signed: HashMap<GrantId, SignedMeta>,
}

struct SignedMeta {
    task_id: String,
    nonce: String,
    exp: u64,
}

/// One of a task's connection slots; frees itself on drop, exactly once.
struct TaskSlot {
    proxy: Arc<Proxy>,
    task_id: String,
}

impl Drop for TaskSlot {
    fn drop(&mut self) {
        let mut st = self.proxy.lock();
        if let Some(n) = st.open_by_task.get_mut(&self.task_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.open_by_task.remove(&self.task_id);
            }
        }
    }
}

pub struct Proxy {
    state: Mutex<State>,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    clock: Arc<dyn Clock>,
    log: LogSink,
    limits: Limits,
    allowed_ports: Vec<u16>,
    key_source: Option<KeySource>,
    key_cache: Mutex<KeyCache>,
}

pub struct ProxyBuilder {
    grants: Vec<Grant>,
    key_source: Option<KeySource>,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    clock: Arc<dyn Clock>,
    log: LogSink,
    limits: Limits,
}

impl ProxyBuilder {
    pub fn new(grants: Vec<Grant>) -> Self {
        Self {
            grants,
            key_source: None,
            resolver: Arc::new(SystemResolver),
            connector: Arc::new(DirectConnector),
            clock: Arc::new(SystemClock),
            log: stderr_log(),
            limits: Limits::default(),
        }
    }
    pub fn resolver(mut self, r: Arc<dyn Resolver>) -> Self {
        self.resolver = r;
        self
    }
    pub fn connector(mut self, c: Arc<dyn Connector>) -> Self {
        self.connector = c;
        self
    }
    pub fn clock(mut self, c: Arc<dyn Clock>) -> Self {
        self.clock = c;
        self
    }
    pub fn log(mut self, l: LogSink) -> Self {
        self.log = l;
        self
    }
    pub fn limits(mut self, l: Limits) -> Self {
        self.limits = l;
        self
    }
    /// Accept signed grants (`ngr1.` tokens) verified with this key.
    pub fn grant_key(mut self, k: KeySource) -> Self {
        self.key_source = Some(k);
        self
    }
    pub fn build(self) -> Arc<Proxy> {
        let now = self.clock.now_ms();
        let mut store = TokenStore::new();
        let mut cancels = HashMap::new();
        for g in self.grants {
            let (id, replaced) = store.insert(g, now);
            for r in replaced {
                cancels.remove(&r);
            }
            cancels.insert(id, watch::channel(false).0);
        }
        Arc::new(Proxy {
            state: Mutex::new(State {
                store,
                cancels,
                open_by_task: HashMap::new(),
                ledger: Ledger::new(),
                signed: HashMap::new(),
            }),
            resolver: self.resolver,
            connector: self.connector,
            clock: self.clock,
            log: self.log,
            limits: self.limits,
            allowed_ports: ALLOWED_PORTS.to_vec(),
            key_source: self.key_source,
            key_cache: Mutex::new(KeyCache {
                key: None,
                read_at: None,
            }),
        })
    }
}

/// Parses a grants file: one object or an array of objects
/// `{"token": str, "task": str, "domains": [str], "expiresIdleMs"?: positive int}`.
/// Unknown fields, short tokens, empty task ids and duplicate tokens/tasks are errors: a
/// typo in a security grant should stop the proxy, not silently widen or narrow it.
pub fn parse_grants(text: &str) -> Result<Vec<Grant>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("grants file: {e}"))?;
    let items = match v {
        Value::Array(a) => a,
        o @ Value::Object(_) => vec![o],
        _ => return Err("grants file: expected an object or an array of objects".into()),
    };
    let mut out: Vec<Grant> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Value::Object(o) = item else {
            return Err(format!("grant {i}: not an object"));
        };
        for k in o.keys() {
            if !matches!(k.as_str(), "token" | "task" | "domains" | "expiresIdleMs") {
                return Err(format!("grant {i}: unknown field {k:?}"));
            }
        }
        let token = o
            .get("token")
            .and_then(Value::as_str)
            .ok_or(format!("grant {i}: token must be a string"))?;
        if token.len() < 16 || token.bytes().any(|c| c.is_ascii_control() || c == b' ') {
            return Err(format!(
                "grant {i}: token must be at least 16 printable bytes without spaces"
            ));
        }
        let task = o
            .get("task")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or(format!("grant {i}: task must be a non-empty string"))?;
        let domains = o
            .get("domains")
            .and_then(Value::as_array)
            .ok_or(format!("grant {i}: domains must be an array"))?
            .iter()
            .map(|d| d.as_str().map(str::to_owned))
            .collect::<Option<Vec<String>>>()
            .ok_or(format!("grant {i}: domains must be strings"))?;
        let idle_ttl_ms = match o.get("expiresIdleMs") {
            None => DEFAULT_TOKEN_TTL_MS,
            Some(v) => v.as_u64().filter(|&n| n > 0).ok_or(format!(
                "grant {i}: expiresIdleMs must be a positive integer"
            ))?,
        };
        if out.iter().any(|g| g.token == token) {
            return Err(format!("grant {i}: duplicate token"));
        }
        if out.iter().any(|g| g.task_id == task) {
            return Err(format!("grant {i}: duplicate task {task:?}"));
        }
        out.push(Grant {
            token: token.to_owned(),
            task_id: task.to_owned(),
            domains,
            idle_ttl_ms,
        });
    }
    Ok(out)
}

const HOP_BY_HOP: [&str; 9] = [
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

/// Removes `Proxy-Authorization`, `Proxy-Authenticate` and every hop-by-hop header, including
/// any the `Connection` header names (RFC 9110 §7.6.1). Applied to requests and to upstream
/// responses alike, as `stripHopByHop` in code-egress.cjs. `Content-Length` and `Host` are
/// exempt from `Connection`: naming them must not unframe a body (smuggling) or drop the Host
/// the proxy sets.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for n in named {
        if n != hyper::header::CONTENT_LENGTH && n != hyper::header::HOST {
            headers.remove(n);
        }
    }
    for h in HOP_BY_HOP {
        headers.remove(h);
    }
}

fn full(text: impl Into<Bytes>) -> Body {
    Full::new(text.into())
        .map_err(|never| match never {})
        .boxed()
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

fn response(status: u16, body: Body) -> Response<Body> {
    let mut r = Response::new(body);
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
    r
}

impl Proxy {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned lock only means another task panicked mid-update; the store is still
        // consistent (every mutation is a single retain/push), so keep serving.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(&self, mut entry: Value) {
        if let Value::Object(o) = &mut entry {
            o.insert("at".into(), json!(self.clock.now_ms()));
        }
        (self.log)(entry);
    }

    fn drop_expired(&self, state: &mut State, expired: Vec<Expired>) {
        for e in expired {
            if let Some(tx) = state.cancels.remove(&e.id) {
                let _ = tx.send(true);
            }
            Self::retire_signed(state, e.id);
            self.record(json!({ "event": "egress.expired", "taskId": e.task_id }));
        }
    }

    /// A dropped signed grant's nonce is retired, so the same token cannot reinstall it.
    fn retire_signed(state: &mut State, id: GrantId) {
        if let Some(m) = state.signed.remove(&id) {
            state.ledger.retire(&m.task_id, &m.nonce, m.exp);
        }
    }

    /// Drops signed grants past their absolute `exp` and forgets ledger entries past theirs.
    fn expire_signed(&self, state: &mut State, now: u64) {
        let gone: Vec<(GrantId, String)> = state
            .signed
            .iter()
            .filter(|(_, m)| now >= m.exp)
            .map(|(id, m)| (*id, m.task_id.clone()))
            .collect();
        for (id, task) in gone {
            for rid in state.store.revoke(&task) {
                if let Some(tx) = state.cancels.remove(&rid) {
                    let _ = tx.send(true);
                }
            }
            Self::retire_signed(state, id);
            self.record(json!({ "event": "egress.expired", "taskId": task }));
        }
        state.ledger.prune(now);
    }

    /// Drops grants idle past their TTL or past their absolute expiry and closes their
    /// connections; returns how many.
    pub fn sweep(&self) -> usize {
        let now = self.clock.now_ms();
        let mut st = self.lock();
        let before = st.store.len();
        let expired = st.store.sweep(now);
        self.drop_expired(&mut st, expired);
        self.expire_signed(&mut st, now);
        before.saturating_sub(st.store.len())
    }

    /// Revokes a task's grant and closes its connections.
    pub fn revoke(&self, task_id: &str) -> usize {
        let mut st = self.lock();
        let ids = st.store.revoke(task_id);
        for id in &ids {
            if let Some(tx) = st.cancels.remove(id) {
                let _ = tx.send(true);
            }
            Self::retire_signed(&mut st, *id);
        }
        ids.len()
    }

    /// The grant key, (re)reading a key file at most every few seconds. None: no key (yet).
    fn grant_key(&self) -> Option<GrantKey> {
        let path = match self.key_source.as_ref()? {
            KeySource::Static(k) => return Some(k.clone()),
            KeySource::File(p) => p,
        };
        let mut cache = self.key_cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.read_at.is_none_or(|t| t.elapsed() >= KEY_RELOAD) {
            cache.read_at = Some(Instant::now());
            match read_key_file(path) {
                Ok(k) => cache.key = Some(k),
                // A previously loaded key survives a transient read failure.
                Err(e) => {
                    self.record(json!({ "event": "egress.grant_key_unavailable", "error": e }))
                }
            }
        }
        cache.key.clone()
    }

    fn verify_signed(&self, token: &[u8], now: u64) -> Result<SignedGrant, Refusal> {
        let Some(key) = self.grant_key() else {
            let (status, reason) = if self.key_source.is_some() {
                (503, "grant key is not available")
            } else {
                (407, "signed grants are not enabled")
            };
            return Err(Refusal {
                status,
                reason: reason.into(),
                task_id: None,
                host: None,
            });
        };
        verify(&key, token, now).map_err(|e| grant_refusal(e, None))
    }

    /// Installs (or confirms) the signed grant a request presents. Unsigned tokens pass
    /// through untouched to the static store.
    fn admit_signed(&self, proxy_authorization: Option<&[u8]>, now: u64) -> Result<(), Refusal> {
        let Some(token) = proxy_authorization.and_then(basic_token) else {
            return Ok(());
        };
        if !looks_signed(&token) {
            return Ok(());
        }
        let g = self.verify_signed(&token, now)?;
        if g.act != Act::Grant {
            return Err(grant_refusal(GrantError::NotAGrant, Some(&g.task)));
        }
        let Ok(token) = String::from_utf8(token) else {
            return Err(grant_refusal(GrantError::Malformed, None));
        };
        let mut st = self.lock();
        self.expire_signed(&mut st, now);
        match st.ledger.admit(&g, now) {
            Err(e) => Err(grant_refusal(e, Some(&g.task))),
            Ok(Admission::Current) => Ok(()),
            Ok(Admission::New) => {
                let (id, replaced) = st.store.insert(
                    Grant {
                        token,
                        task_id: g.task.clone(),
                        domains: g.hosts.clone(),
                        idle_ttl_ms: g.idle,
                    },
                    now,
                );
                for r in replaced {
                    if let Some(tx) = st.cancels.remove(&r) {
                        let _ = tx.send(true);
                    }
                    // The ledger already retired the superseded nonce.
                    st.signed.remove(&r);
                }
                st.cancels.insert(id, watch::channel(false).0);
                st.signed.insert(
                    id,
                    SignedMeta {
                        task_id: g.task.clone(),
                        nonce: g.nonce.clone(),
                        exp: g.exp,
                    },
                );
                self.record(
                    json!({ "event": "egress.granted", "taskId": g.task, "hosts": g.hosts.len() }),
                );
                Ok(())
            }
        }
    }

    /// `POST /v1/revoke` with `Authorization: Bearer <signed revoke>` (origin-form, sent by
    /// the web when a task ends): drops the task's grant and its connections. A grant token,
    /// an unsigned token or a bad MAC is refused (403); replaying a revoke is harmless.
    fn handle_revoke(&self, req: &Request<Incoming>) -> Response<Body> {
        let now = self.clock.now_ms();
        let token = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
            .map(<[u8]>::to_vec);
        let result = match token {
            None => Err(grant_refusal(GrantError::NotAGrant, None)),
            Some(t) => self.verify_signed(&t, now).and_then(|g| {
                if g.act != Act::Revoke {
                    return Err(grant_refusal(GrantError::NotAGrant, Some(&g.task)));
                }
                self.lock()
                    .ledger
                    .admit(&g, now)
                    .map_err(|e| grant_refusal(e, Some(&g.task)))?;
                Ok(g)
            }),
        };
        match result {
            Ok(g) => {
                let n = self.revoke(&g.task);
                self.record(json!({ "event": "egress.revoked", "taskId": g.task, "grants": n }));
                response(204, empty())
            }
            Err(mut r) => {
                if r.status == 407 {
                    r.status = 403;
                }
                self.refused(&r);
                text_refusal(&r)
            }
        }
    }

    fn touch(&self, id: GrantId) {
        let now = self.clock.now_ms();
        self.lock().store.touch(id, now);
    }

    /// The whole verdict: token, target, port, allowlist, one resolution, every answer
    /// public, grant still live. On allow, also hands back the grant's cancel signal.
    async fn decide(
        &self,
        proxy_authorization: Option<&[u8]>,
        target: Option<&str>,
        default_port: u16,
    ) -> Result<(Allowed, watch::Receiver<bool>), Refusal> {
        let pending = {
            let now = self.clock.now_ms();
            self.admit_signed(proxy_authorization, now)?;
            let mut st = self.lock();
            self.expire_signed(&mut st, now);
            let mut expired = Vec::new();
            let r = check_target(
                &mut st.store,
                proxy_authorization,
                target,
                default_port,
                &self.allowed_ports,
                now,
                &mut expired,
            );
            self.drop_expired(&mut st, expired);
            r?
        };
        let resolved = if pending.literal {
            Vec::new()
        } else {
            match tokio::time::timeout(
                self.limits.resolve_timeout,
                self.resolver.resolve(&pending.host),
            )
            .await
            {
                Ok(Ok(addrs)) => addrs,
                _ => Vec::new(),
            }
        };
        let st = self.lock();
        let allowed = check_addresses(&st.store, &pending, &resolved)?;
        match st.cancels.get(&allowed.grant) {
            Some(tx) => Ok((allowed, tx.subscribe())),
            None => Err(Refusal {
                status: 407,
                reason: "task token was revoked".into(),
                task_id: Some(allowed.task_id),
                host: Some(allowed.host),
            }),
        }
    }

    /// Takes one of the task's slots, or refuses with 429 when it is at its cap.
    fn take_task_slot(self: &Arc<Self>, allowed: &Allowed) -> Result<TaskSlot, Refusal> {
        let cap = self.limits.max_connections_per_task.max(1);
        let mut st = self.lock();
        let open = st.open_by_task.entry(allowed.task_id.clone()).or_insert(0);
        if *open >= cap {
            return Err(Refusal {
                status: 429,
                reason: format!("task has {cap} connections open"),
                task_id: Some(allowed.task_id.clone()),
                host: Some(allowed.host.clone()),
            });
        }
        *open += 1;
        Ok(TaskSlot {
            proxy: self.clone(),
            task_id: allowed.task_id.clone(),
        })
    }

    fn refused(&self, r: &Refusal) {
        self.record(json!({
            "event": "egress.refused",
            "status": r.status,
            "reason": r.reason,
            "taskId": r.task_id,
            "host": r.host,
        }));
    }

    fn allowed(&self, a: &Allowed, method: &str) {
        self.record(json!({
            "event": "egress.allowed",
            "taskId": a.task_id,
            "host": a.host,
            "port": a.port,
            "method": method,
        }));
    }

    async fn connect_upstream(&self, a: &Allowed) -> io::Result<TcpStream> {
        let addr = SocketAddr::new(a.address, a.port);
        match tokio::time::timeout(self.limits.connect_timeout, self.connector.connect(addr)).await
        {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
        }
    }

    /// Accepts connections until the listener fails.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        let slots = Arc::new(Semaphore::new(self.limits.max_connections.max(1)));
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    // EMFILE, ECONNABORTED, …: log, back off briefly, keep serving.
                    self.record(
                        json!({ "event": "egress.accept_failed", "error": e.kind().to_string() }),
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                self.record(json!({ "event": "egress.connection_limit" }));
                drop(stream);
                continue;
            };
            let proxy = self.clone();
            tokio::spawn(async move {
                proxy.serve_connection(stream, Arc::new(permit)).await;
            });
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: TcpStream, slot: Slot) {
        let proxy = self.clone();
        let conn_slot = slot.clone();
        let service = hyper::service::service_fn(move |req| {
            let proxy = proxy.clone();
            let slot = conn_slot.clone();
            async move { Ok::<_, Infallible>(proxy.handle(req, slot).await) }
        });
        let mut builder = hyper::server::conn::http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(self.limits.header_timeout)
            .max_buf_size(self.limits.max_header_bytes.max(8192))
            .max_headers(self.limits.max_headers);
        let _ = builder
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades()
            .await;
    }

    async fn handle(self: Arc<Self>, req: Request<Incoming>, slot: Slot) -> Response<Body> {
        if req.method() == Method::CONNECT {
            self.handle_connect(req, slot).await
        } else {
            self.handle_forward(req, slot).await
        }
    }

    /// HTTPS: a CONNECT tunnel to the checked address. Refusals are a bodiless response that
    /// closes the connection, as curl, npm and pip expect.
    async fn handle_connect(
        self: Arc<Self>,
        mut req: Request<Incoming>,
        slot: Slot,
    ) -> Response<Body> {
        let auth = req
            .headers()
            .get(hyper::header::PROXY_AUTHORIZATION)
            .map(|v| v.as_bytes().to_vec());
        let target = req.uri().authority().map(|a| a.as_str().to_owned());
        let (allowed, cancel) = match self.decide(auth.as_deref(), target.as_deref(), 443).await {
            Ok(v) => v,
            Err(r) => {
                self.refused(&r);
                return connect_refusal(r.status);
            }
        };
        let task_slot = match self.take_task_slot(&allowed) {
            Ok(t) => t,
            Err(r) => {
                self.refused(&r);
                return connect_refusal(r.status);
            }
        };
        self.allowed(&allowed, "CONNECT");
        let upstream = match self.connect_upstream(&allowed).await {
            Ok(s) => s,
            Err(e) => {
                self.record(
                    json!({ "event": "egress.upstream_failed", "taskId": allowed.task_id,
                    "host": allowed.host, "error": e.kind().to_string() }),
                );
                return connect_refusal(502);
            }
        };
        let on_upgrade = hyper::upgrade::on(&mut req);
        let proxy = self.clone();
        let grant = allowed.grant;
        let task_id = allowed.task_id.clone();
        tokio::spawn(async move {
            // The tunnel keeps the connection's slot: hyper's connection future ends at the
            // upgrade, and an upgraded tunnel must still count against max_connections.
            let _slot = slot;
            let _task_slot = task_slot;
            if let Ok(upgraded) = on_upgrade.await {
                proxy
                    .tunnel(TokioIo::new(upgraded), upstream, grant, &task_id, cancel)
                    .await;
            }
        });
        let mut r = response(200, empty());
        r.extensions_mut()
            .insert(hyper::ext::ReasonPhrase::from_static(
                b"Connection Established",
            ));
        r
    }

    async fn tunnel<C>(
        &self,
        client: C,
        upstream: TcpStream,
        grant: GrantId,
        task_id: &str,
        mut cancel: watch::Receiver<bool>,
    ) where
        C: AsyncRead + AsyncWrite + Unpin,
    {
        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut ur, mut uw) = upstream.into_split();
        let last = Mutex::new(tokio::time::Instant::now());
        let up = self.pump(&mut cr, &mut uw, grant, &last);
        let down = self.pump(&mut ur, &mut cw, grant, &last);
        let idle = self.limits.tunnel_idle_timeout;
        let watchdog = async {
            loop {
                let deadline = *last.lock().unwrap_or_else(|p| p.into_inner()) + idle;
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep_until(deadline).await;
            }
        };
        tokio::select! {
            _ = async { tokio::try_join!(up, down) } => {}
            _ = cancel.wait_for(|dropped| *dropped) => {}
            _ = watchdog => {
                self.record(json!({ "event": "egress.tunnel_idle", "taskId": task_id }));
            }
        }
    }

    /// Copies one direction, refreshing the grant's and the tunnel's idle clocks per chunk.
    async fn pump<R, W>(
        &self,
        r: &mut R,
        w: &mut W,
        grant: GrantId,
        last: &Mutex<tokio::time::Instant>,
    ) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = r.read(&mut buf).await?;
            if n == 0 {
                return w.shutdown().await;
            }
            self.touch(grant);
            *last.lock().unwrap_or_else(|p| p.into_inner()) = tokio::time::Instant::now();
            w.write_all(buf.get(..n).unwrap_or_default()).await?;
        }
    }

    /// Plain HTTP: an absolute `http://` URI, forwarded to the checked address with the proxy's
    /// own credentials and every hop-by-hop header removed.
    async fn handle_forward(self: Arc<Self>, req: Request<Incoming>, slot: Slot) -> Response<Body> {
        let uri = req.uri().clone();
        if uri.scheme().is_none() && req.method() == Method::POST && uri.path() == "/v1/revoke" {
            return self.handle_revoke(&req);
        }
        if let Some(scheme) = uri.scheme_str() {
            // `https://` in absolute form would go out as plaintext to port 80: the task
            // believes it has TLS and gets none. TLS goes through CONNECT only (#930).
            if scheme != "http" {
                let reason = if scheme == "https" {
                    "https:// must be requested with CONNECT".to_owned()
                } else {
                    format!("unsupported protocol \"{scheme}:\"")
                };
                let r = Refusal {
                    status: 400,
                    reason,
                    task_id: None,
                    host: None,
                };
                self.refused(&r);
                return text_refusal(&r);
            }
        }
        // The URL's host, without any userinfo, as the JS `url.host`.
        let target = uri
            .authority()
            .filter(|_| uri.scheme_str().is_some())
            .map(|a| a.as_str().rsplit('@').next().unwrap_or("").to_owned());
        let auth = req
            .headers()
            .get(hyper::header::PROXY_AUTHORIZATION)
            .map(|v| v.as_bytes().to_vec());
        let (allowed, mut cancel) = match self.decide(auth.as_deref(), target.as_deref(), 80).await
        {
            Ok(v) => v,
            Err(r) => {
                self.refused(&r);
                return text_refusal(&r);
            }
        };
        let task_slot = match self.take_task_slot(&allowed) {
            Ok(t) => t,
            Err(r) => {
                self.refused(&r);
                return text_refusal(&r);
            }
        };
        self.allowed(&allowed, req.method().as_str());

        let (mut parts, body) = req.into_parts();
        let had_te = parts.headers.contains_key(hyper::header::TRANSFER_ENCODING);
        strip_hop_by_hop(&mut parts.headers);
        if had_te {
            parts.headers.remove(hyper::header::CONTENT_LENGTH);
        }
        let host_value = target
            .as_deref()
            .and_then(|t| HeaderValue::from_str(t).ok());
        let path = uri
            .path_and_query()
            .map(|p| p.as_str().to_owned())
            .unwrap_or_else(|| "/".into());
        let (Some(host_value), Ok(path)) = (host_value, path.parse::<hyper::Uri>()) else {
            return upstream_failed();
        };
        parts.headers.insert(hyper::header::HOST, host_value);
        parts.uri = path;
        parts.version = hyper::Version::HTTP_11;
        let upstream_req = Request::from_parts(parts, body);

        let stream = match self.connect_upstream(&allowed).await {
            Ok(s) => s,
            Err(_) => return upstream_failed(),
        };
        let Ok((mut sender, conn)) =
            hyper::client::conn::http1::handshake::<_, Incoming>(TokioIo::new(stream)).await
        else {
            return upstream_failed();
        };
        tokio::spawn(async move {
            let _slot = slot;
            let _task_slot = task_slot;
            tokio::select! {
                _ = conn => {}
                _ = cancel.wait_for(|dropped| *dropped) => {}
            }
        });
        match sender.send_request(upstream_req).await {
            Ok(up) => {
                self.touch(allowed.grant);
                let (mut parts, body) = up.into_parts();
                // Same framing rule as the request side: a chunked body's Content-Length is a
                // lie (or a smuggling attempt), and hyper re-frames what it relays.
                let had_te = parts.headers.contains_key(hyper::header::TRANSFER_ENCODING);
                strip_hop_by_hop(&mut parts.headers);
                if had_te {
                    parts.headers.remove(hyper::header::CONTENT_LENGTH);
                }
                Response::from_parts(parts, body.boxed())
            }
            Err(_) => upstream_failed(),
        }
    }
}

fn grant_refusal(e: GrantError, task: Option<&str>) -> Refusal {
    Refusal {
        status: if e == GrantError::LedgerFull {
            503
        } else {
            407
        },
        reason: e.reason().into(),
        task_id: task.map(str::to_owned),
        host: None,
    }
}

fn connect_refusal(status: u16) -> Response<Body> {
    let mut r = response(status, empty());
    let h = r.headers_mut();
    if status == 407 {
        h.insert(
            hyper::header::PROXY_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"noevia task\""),
        );
    }
    h.insert(hyper::header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    h.insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    if let Ok(reason) = hyper::ext::ReasonPhrase::try_from(status_text(status).as_bytes()) {
        r.extensions_mut().insert(reason);
    }
    r
}

fn text_refusal(r: &Refusal) -> Response<Body> {
    let mut resp = response(r.status, full(format!("Refused: {}\n", r.reason)));
    let h = resp.headers_mut();
    h.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain"),
    );
    if r.status == 407 {
        h.insert(
            hyper::header::PROXY_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"noevia task\""),
        );
    }
    resp
}

fn upstream_failed() -> Response<Body> {
    let mut r = response(502, full("Upstream failed\n"));
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain"),
    );
    r
}
