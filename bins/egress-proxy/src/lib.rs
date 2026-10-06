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
    check_addresses, check_target, status_text, Allowed, Expired, Grant, GrantId, Refusal,
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
use tokio::sync::{watch, Semaphore};

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
    /// Request head buffer (hyper's minimum, 8192, is enforced).
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub header_timeout: Duration,
    pub resolve_timeout: Duration,
    pub connect_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            max_header_bytes: 16 * 1024,
            max_headers: 100,
            header_timeout: Duration::from_secs(30),
            resolve_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

struct State {
    store: TokenStore,
    /// Fires (true) when a grant is dropped, closing every connection it owns.
    cancels: HashMap<GrantId, watch::Sender<bool>>,
}

pub struct Proxy {
    state: Mutex<State>,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    clock: Arc<dyn Clock>,
    log: LogSink,
    limits: Limits,
    allowed_ports: Vec<u16>,
}

pub struct ProxyBuilder {
    grants: Vec<Grant>,
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
            resolver: Arc::new(SystemResolver),
            connector: Arc::new(DirectConnector),
            clock: Arc::new(MonotonicClock::default()),
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
            state: Mutex::new(State { store, cancels }),
            resolver: self.resolver,
            connector: self.connector,
            clock: self.clock,
            log: self.log,
            limits: self.limits,
            allowed_ports: ALLOWED_PORTS.to_vec(),
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

const HOP_BY_HOP: [&str; 8] = [
    "proxy-authorization",
    "proxy-connection",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

/// Removes `Proxy-Authorization` and every hop-by-hop header, including any the `Connection`
/// header names (RFC 9110 §7.6.1).
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for n in named {
        headers.remove(n);
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
            self.record(json!({ "event": "egress.expired", "taskId": e.task_id }));
        }
    }

    /// Drops grants idle past their TTL and closes their connections; returns how many.
    pub fn sweep(&self) -> usize {
        let now = self.clock.now_ms();
        let mut st = self.lock();
        let expired = st.store.sweep(now);
        let n = expired.len();
        self.drop_expired(&mut st, expired);
        n
    }

    /// Revokes a task's grant and closes its connections.
    pub fn revoke(&self, task_id: &str) -> usize {
        let mut st = self.lock();
        let ids = st.store.revoke(task_id);
        for id in &ids {
            if let Some(tx) = st.cancels.remove(id) {
                let _ = tx.send(true);
            }
        }
        ids.len()
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
            let mut st = self.lock();
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
                let _permit = permit;
                proxy.serve_connection(stream).await;
            });
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: TcpStream) {
        let proxy = self.clone();
        let service = hyper::service::service_fn(move |req| {
            let proxy = proxy.clone();
            async move { Ok::<_, Infallible>(proxy.handle(req).await) }
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

    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        if req.method() == Method::CONNECT {
            self.handle_connect(req).await
        } else {
            self.handle_forward(req).await
        }
    }

    /// HTTPS: a CONNECT tunnel to the checked address. Refusals are a bodiless response that
    /// closes the connection, as curl, npm and pip expect.
    async fn handle_connect(self: Arc<Self>, mut req: Request<Incoming>) -> Response<Body> {
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
        tokio::spawn(async move {
            if let Ok(upgraded) = on_upgrade.await {
                proxy
                    .tunnel(TokioIo::new(upgraded), upstream, grant, cancel)
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
        mut cancel: watch::Receiver<bool>,
    ) where
        C: AsyncRead + AsyncWrite + Unpin,
    {
        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut ur, mut uw) = upstream.into_split();
        let up = self.pump(&mut cr, &mut uw, grant);
        let down = self.pump(&mut ur, &mut cw, grant);
        tokio::select! {
            _ = async { tokio::try_join!(up, down) } => {}
            _ = cancel.wait_for(|dropped| *dropped) => {}
        }
    }

    /// Copies one direction, refreshing the grant's idle clock on every chunk.
    async fn pump<R, W>(&self, r: &mut R, w: &mut W, grant: GrantId) -> io::Result<()>
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
            w.write_all(buf.get(..n).unwrap_or_default()).await?;
        }
    }

    /// Plain HTTP: an absolute `http://` URI, forwarded to the checked address with the proxy's
    /// own credentials and every hop-by-hop header removed.
    async fn handle_forward(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        let uri = req.uri().clone();
        if let Some(scheme) = uri.scheme_str() {
            // `https://` in absolute form would be forwarded as plaintext to port 80 by the JS
            // reference; here it is refused like any other protocol (clients use CONNECT).
            if scheme != "http" {
                let r = Refusal {
                    status: 400,
                    reason: format!("unsupported protocol \"{scheme}:\""),
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
            tokio::select! {
                _ = conn => {}
                _ = cancel.wait_for(|dropped| *dropped) => {}
            }
        });
        match sender.send_request(upstream_req).await {
            Ok(up) => {
                self.touch(allowed.grant);
                let (mut parts, body) = up.into_parts();
                strip_hop_by_hop(&mut parts.headers);
                Response::from_parts(parts, body.boxed())
            }
            Err(_) => upstream_failed(),
        }
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
