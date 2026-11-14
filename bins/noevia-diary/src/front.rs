//! R9 shift 3, S2a: `noevia-diary --main`, the Diary container's main process (DIARY_MAIN=rust in
//! the image's entrypoint; unset keeps Python as the main process, unchanged).
//!
//! Topology. This process listens on the public port (0.0.0.0:8010) and on a loopback control port
//! (127.0.0.1:8011, the port the Python parent used to start this binary on). It starts the Python
//! app as its CHILD (uvicorn on 127.0.0.1:8012, the argv after `--`) with `DIARY_PARENT=rust` and
//! one JSON line on its stdin: the control token, the parent secret and the control port. The child
//! resolves its own configuration (config.yaml + environment, as before) and POSTs the start-up
//! config it used to send to this binary (`rust_forward.server_config`) to
//! `/internal/child/config` on the control port. Until then every public request answers 503.
//!
//! Requests. Every client-supplied `x-noevia-diary-*` header is dropped. A request naming a tenant
//! is verified HERE when a tenant key is set (the only verifier and the only nonce owner: the child
//! trusts the parent's verdict), with TenantAssertionMiddleware's refusals (401 `invalid tenant
//! assertion`, 413 `request body too large`). Then:
//! - a native read route (server.rs `route`, no body) that the child's DIARY_RUST_ROUTES names (the
//!   start-up config's `native_routes`) is served here exactly as when Python forwarded it, with the
//!   "recovered" fact taken from the child's last stamp for that tenant (below);
//! - everything else, and every request the native side declines, goes to the child with the
//!   parent secret and `x-noevia-diary-asserted`, its body buffered when it was verified (JSON) and
//!   streamed otherwise; the reply streams back unbuffered with no total deadline, and a client that
//!   goes away drops the child connection.
//!
//! S2a writes nothing natively: every write route is forwarded, so the child keeps its in-process
//! tenant writer lock and DIARY_RUST_WRITES has no effect (the child ignores it). The child stamps
//! every reply to a UUID tenant's request with `x-noevia-diary-recovered: 1|0` (does it hold a
//! recovered managed state for the tenant?); a stamp is trusted for RECOVERED_TTL, after which the
//! tenant's next read goes to the child again.
//!
//! Lifetime. No restarts: when the child exits, this process exits non-zero and the container
//! restarts, as it does today when Python dies. SIGTERM/SIGINT are passed to the child, which gets
//! STOP_GRACE to finish, then this process exits.

use crate::config::Config;
use crate::server::{self, App, DECLINE_HEADER, RECOVERED_HEADER};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// S3: "1" on a forwarded request when this process served a native write of the tenant since the
/// child last heard: the child marks its state for the tenant unrecovered (`_rust_write_uncertain`).
pub const WRITTEN_HEADER: &str = "x-noevia-diary-written";
/// The header carrying the parent secret on every request to the child.
pub const PARENT_HEADER: &str = "x-noevia-diary-parent";
/// "1" when this process verified the request's tenant assertion (the child's `tenant_asserted`).
pub const ASSERTED_HEADER: &str = server::ASSERTED_HEADER;
const INTERNAL_PREFIX: &str = "x-noevia-diary-";
/// How long a child's "recovered" stamp lets a tenant's reads stay native. Python checks it live on
/// every read; the child may evict an idle tenant's state (its 128-tenant LRU) without telling this
/// process, so a stamp is short-lived (named difference: for at most this long after an eviction a
/// read is served here without Python's recovery; pending journal, dirty or outbox work still
/// declines it).
pub const RECOVERED_TTL: Duration = Duration::from_secs(60);
/// How long the child gets after SIGTERM.
pub const STOP_GRACE: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Below uvicorn's keep-alive (5 s), so a pooled connection is never reused as the child closes it.
const POOL_IDLE: Duration = Duration::from_secs(2);
/// S3: how long a native write waits for the tenant's writer slot (Python's tenant_writer.DEADLINE_S).
pub const WRITE_WAIT: Duration = Duration::from_secs(30);
/// S3: how long a writer tenant's write body may take to arrive (Python's FORWARD_DEADLINE_S).
const BODY_DEADLINE: Duration = Duration::from_secs(30);
const TOO_LARGE: &[u8] = br#"{"detail":"request body too large"}"#;
const BODY_TIMEOUT: &[u8] = br#"{"detail":"The request body did not arrive in time."}"#;
pub const STARTING: &[u8] = br#"{"detail":"The Diary is starting. Try again in a moment."}"#;
pub const CHILD_DOWN: &[u8] = br#"{"detail":"The Diary did not answer. Try again in a moment."}"#;
const HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "upgrade",
    "trailer",
];

/// The command line of `--main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub host: String,
    pub port: u16,
    pub control_port: u16,
    pub child_port: u16,
    pub child: Vec<String>,
}

/// `--main [--host H] [--port P] [--control-port C] [--child-port K] -- <child argv...>`.
pub fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        host: "0.0.0.0".into(),
        port: 8010,
        control_port: 8011,
        child_port: 8012,
        child: Vec::new(),
    };
    let mut it = args.iter();
    if it.next().map(String::as_str) != Some("--main") {
        return Err("expected --main".into());
    }
    while let Some(a) = it.next() {
        let port = |v: Option<&String>| -> Result<u16, String> {
            v.and_then(|v| v.parse::<u16>().ok())
                .ok_or_else(|| format!("{a} needs a port number"))
        };
        match a.as_str() {
            "--host" => out.host = it.next().ok_or("--host needs a value")?.clone(),
            "--port" => out.port = port(it.next())?,
            "--control-port" => out.control_port = port(it.next())?,
            "--child-port" => out.child_port = port(it.next())?,
            "--" => {
                out.child = it.cloned().collect();
                break;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if out.child.is_empty() {
        return Err("no child command after --".into());
    }
    Ok(out)
}

/// The parent's state, shared by both listeners.
pub struct Front {
    pub control_token: String,
    pub secret: String,
    pub child_port: u16,
    app: OnceLock<Arc<App>>,
    native_routes: OnceLock<Vec<String>>,
    native_writers: OnceLock<Vec<String>>,
    recovered: Mutex<HashMap<String, Instant>>,
    /// S3: per tenant, (native writes served, the highest of them the child has been told of).
    written: Mutex<HashMap<String, (u64, u64)>>,
    write_wait: Duration,
    /// S4: this run's epoch for the embedding snapshot (handed to the child), and the owner.
    pub embedding_epoch: String,
    owner: OnceLock<Arc<crate::embedding_owner::Owner>>,
    http: Client<HttpConnector, Body>,
}

impl Front {
    pub fn new(control_token: String, secret: String, child_port: u16) -> Self {
        // The child gets the exact request target this process verified and routed (no URL
        // normalisation: a `..` segment or `%2e` reaches the child as the client sent it).
        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(CONNECT_TIMEOUT));
        connector.set_nodelay(true);
        let http = Client::builder(hyper_util::rt::TokioExecutor::new())
            .pool_idle_timeout(POOL_IDLE)
            .build(connector);
        Front {
            control_token,
            secret,
            child_port,
            app: OnceLock::new(),
            native_routes: OnceLock::new(),
            native_writers: OnceLock::new(),
            recovered: Mutex::new(HashMap::new()),
            written: Mutex::new(HashMap::new()),
            write_wait: WRITE_WAIT,
            embedding_epoch: random_hex(16).unwrap_or_default(),
            owner: OnceLock::new(),
            http,
        }
    }

    /// How long a native write waits for the writer slot (tests shorten it).
    pub fn with_write_wait(mut self, wait: Duration) -> Self {
        self.write_wait = wait;
        self
    }

    /// The App built from the child's start-up config (None until it arrived).
    pub fn app(&self) -> Option<&Arc<App>> {
        self.app.get()
    }

    /// Install the child's start-up config. It must carry this process's own control token and
    /// secret (the child echoes what it was given); a second config is refused.
    pub fn install(&self, cfg: Config) -> Result<(), &'static str> {
        if !bool::from(
            cfg.control_token
                .as_bytes()
                .ct_eq(self.control_token.as_bytes()),
        ) || !bool::from(cfg.forward_secret.as_bytes().ct_eq(self.secret.as_bytes()))
        {
            return Err("start-up config does not carry this process's secrets");
        }
        let routes = cfg.native_routes.clone();
        let writers = cfg.native_writers.clone();
        let embedding = cfg.embedding.clone();
        if embedding.is_some() && self.embedding_epoch.is_empty() {
            return Err("no embedding epoch for the child");
        }
        let mut app = App::new(cfg);
        app.write_wait = Some(self.write_wait);
        let app = Arc::new(app);
        self.app
            .set(app.clone())
            .map_err(|_| "start-up config already installed")?;
        if let Some(e) = embedding {
            match crate::embedding_owner::Owner::start(app, &e, self.embedding_epoch.clone()) {
                Ok(owner) => {
                    let _ = self.owner.set(owner);
                }
                Err(err) => {
                    eprintln!("noevia-diary: the embedding configuration cannot be owned here: {err}");
                    return Err("embedding owner not started");
                }
            }
        }
        let _ = self.native_writers.set(writers);
        self.native_routes
            .set(routes)
            .map_err(|_| "start-up config already installed")
    }

    /// S3: is this process the tenant's writer (the child's DIARY_RUST_WRITES: UUIDs or `*`)?
    fn is_writer(&self, uid: &str) -> bool {
        self.native_writers
            .get()
            .is_some_and(|w| w.iter().any(|w| w == "*" || w == uid))
    }

    fn is_recovered(&self, uid: &str) -> bool {
        let mut map = self.recovered.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(uid) {
            Some(at) if at.elapsed() < RECOVERED_TTL => true,
            Some(_) => {
                map.remove(uid);
                false
            }
            None => false,
        }
    }

    fn forget(&self, uid: &str) {
        self.stamp(uid, false);
    }

    /// A native write was served for `uid`: the child's recovered state for it is stale (Python's
    /// D5 forwarder calls `_rust_write_uncertain` after every served native write). Its stamp is
    /// voided, and the next forwarded request of the tenant tells the child (WRITTEN_HEADER).
    fn wrote(&self, uid: &str) {
        let mut map = self.written.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(uid.to_string()).or_insert((0, 0));
        entry.0 += 1;
        drop(map);
        self.forget(uid);
    }

    /// The tenant's native-write count at send time, and whether the child still has to be told.
    fn written_at_send(&self, uid: &str) -> (u64, bool) {
        let map = self.written.lock().unwrap_or_else(|e| e.into_inner());
        map.get(uid).map_or((0, false), |(n, told)| (*n, told < n))
    }

    /// A reply arrived for a request sent at write count `sent` (`told`: it carried the news). The
    /// stamp counts only when no native write happened since and the child knew of every one.
    fn settle(&self, uid: &str, sent: u64, told: bool, recovered: bool) {
        let fresh = {
            let mut map = self.written.lock().unwrap_or_else(|e| e.into_inner());
            match map.get_mut(uid) {
                None => true,
                Some((n, delivered)) => {
                    if told && *delivered < sent {
                        *delivered = sent;
                    }
                    *n == sent && *delivered >= sent
                }
            }
        };
        self.stamp(uid, recovered && fresh);
    }

    fn stamp(&self, uid: &str, recovered: bool) {
        let mut map = self.recovered.lock().unwrap_or_else(|e| e.into_inner());
        if recovered {
            map.insert(uid.to_string(), Instant::now());
        } else {
            map.remove(uid);
        }
    }
}

fn raw(status: u16, body: &'static [u8]) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap_or_default()
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .map(|v| crate::py::latin1(v.as_bytes()))
        .unwrap_or_default()
}

/// Does the request declare a body (Content-Length above 0, or any Transfer-Encoding)?
fn declares_body(headers: &HeaderMap) -> bool {
    headers.contains_key("transfer-encoding")
        || headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() != "0")
}

/// Python's `_json_media`: no Content-Type, or `application/json` (parameters ignored).
fn json_media(headers: &HeaderMap) -> bool {
    headers.get("content-type").is_none_or(|v| {
        crate::py::latin1(v.as_bytes())
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    })
}

/// Content-Length as declared (None: absent or not a number).
fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn is_decline(resp: &Response<Body>) -> bool {
    resp.status() == StatusCode::MISDIRECTED_REQUEST
        && resp
            .headers()
            .get(DECLINE_HEADER)
            .is_some_and(|v| v.as_bytes() == b"1")
}

/// The lower-cased tenant id a child stamp applies to (a UUID `x-cowork-user-id`, its first value
/// as Starlette and the verifier read it), else None.
fn stamp_tenant(headers: &HeaderMap) -> Option<String> {
    let uid = header_str(headers, "x-cowork-user-id").to_ascii_lowercase();
    server::is_uuid(&uid).then_some(uid)
}

/// The public listener's handler.
pub async fn public(front: Arc<Front>, req: Request<Body>) -> Response<Body> {
    let Some(app) = front.app().cloned() else {
        return raw(503, STARTING);
    };
    let (mut parts, body) = req.into_parts();
    // A client never speaks for this process or the child.
    let internal: Vec<HeaderName> = parts
        .headers
        .keys()
        .filter(|k| k.as_str().starts_with(INTERNAL_PREFIX))
        .cloned()
        .collect();
    for k in internal {
        parts.headers.remove(k);
    }
    let raw_path = parts.uri.path().to_string();
    let query: Vec<u8> = parts
        .uri
        .query()
        .map(|q| q.as_bytes().to_vec())
        .unwrap_or_default();

    // TenantAssertionMiddleware: a request naming a tenant, with a key set, is verified here.
    let names_tenant = !header_str(&parts.headers, "x-cowork-user-id").is_empty();
    let verify = names_tenant && !app.tenant_key.is_empty();
    let hashed = crate::assertion::body_is_hashed(&header_str(&parts.headers, "content-type"));
    let mut buffered: Option<axum::body::Bytes> = None;
    let mut stream: Option<Body> = Some(body);
    if verify && hashed {
        let Some(b) = stream.take() else {
            return server::internal_error();
        };
        match axum::body::to_bytes(b, server::MAX_BODY).await {
            Ok(bytes) => buffered = Some(bytes),
            Err(_) => return server::detail(413, "request body too large"),
        }
    }
    let asserted = if verify {
        match server::verify_assertion(
            &app,
            &parts.headers,
            parts.method.as_str(),
            &raw_path,
            &query,
            buffered.as_deref().unwrap_or_default(),
            server::now(),
        ) {
            Ok(v) => v,
            Err(reason) => {
                eprintln!("noevia-diary: tenant assertion rejected: {reason}");
                return server::detail(401, "invalid tenant assertion");
            }
        }
    } else {
        false
    };

    let path = server::decoded_path(&raw_path);
    // S4: the embedding endpoints are this process's while it owns the configuration (never the
    // child's, which only follows it).
    if let Some(owner) = front.owner.get() {
        if path == "/api/internal/embedding"
            && (parts.method == Method::GET || parts.method == Method::POST)
        {
            let bytes = match (buffered.take(), stream.take()) {
                (Some(b), _) => b,
                (None, Some(s)) => match tokio::time::timeout(
                    BODY_DEADLINE,
                    axum::body::to_bytes(s, server::MAX_BODY),
                )
                .await
                {
                    Ok(Ok(b)) => b,
                    Ok(Err(_)) => return raw(413, TOO_LARGE),
                    Err(_) => return raw(408, BODY_TIMEOUT),
                },
                (None, None) => axum::body::Bytes::new(),
            };
            return owner.route(&app, &parts.method, &parts.headers, &bytes).await;
        }
    }
    // A native read: no body, a read route, served here unless it declines.
    let named = front
        .native_routes
        .get()
        .is_some_and(|n| n.iter().any(|n| n == &path));
    if let Some(r) = server::route(&parts.method, &path).filter(|r| named && !r.write()) {
        if !declares_body(&parts.headers) {
            let mut headers = parts.headers.clone();
            // A tenant the child deleted (its fence) is the child's to answer (410), never native.
            if stamp_tenant(&headers)
                .is_some_and(|uid| !app.writers.is_fenced(&uid) && front.is_recovered(&uid))
            {
                headers.insert(RECOVERED_HEADER, HeaderValue::from_static("1"));
            }
            let resp = server::serve_native(app.clone(), r, query.clone(), headers, asserted).await;
            if !is_decline(&resp) {
                return resp;
            }
        }
    }
    // S3: a writer tenant's write is served here, under its writer slot (the child's lock is a
    // lease on the same slot), unless it declines; the child then serves it with the same bytes.
    let backup = parts.method == Method::POST && path == "/api/storage-backup";
    let write = server::route(&parts.method, &path).filter(|r| r.write());
    let writer = stamp_tenant(&parts.headers)
        .filter(|uid| front.is_writer(uid) && !app.writers.is_fenced(uid));
    if (write.is_some() || backup)
        && writer.is_some()
        && json_media(&parts.headers)
        && declared_length(&parts.headers).is_none_or(|n| n <= server::MAX_BODY as u64)
    {
        let bytes = match (buffered.take(), stream.take()) {
            (Some(b), _) => b,
            (None, Some(s)) => {
                match tokio::time::timeout(BODY_DEADLINE, axum::body::to_bytes(s, server::MAX_BODY))
                    .await
                {
                    Ok(Ok(b)) => b,
                    Ok(Err(_)) => return raw(413, TOO_LARGE),
                    Err(_) => return raw(408, BODY_TIMEOUT),
                }
            }
            (None, None) => axum::body::Bytes::new(),
        };
        let mut headers = parts.headers.clone();
        headers.insert(server::WRITER_HEADER, HeaderValue::from_static("1"));
        let resp = server::serve_native_write(
            app.clone(),
            write,
            backup,
            query.clone(),
            headers,
            bytes.clone(),
            asserted,
        )
        .await;
        if !is_decline(&resp) {
            if let Some(uid) = writer.as_deref() {
                front.wrote(uid);
            }
            return resp;
        }
        buffered = Some(bytes);
    }
    let body = match (buffered, stream) {
        (Some(b), _) => Body::from(b),
        (None, Some(s)) => s,
        (None, None) => Body::empty(),
    };
    forward(
        &front,
        parts.method,
        &raw_path,
        &query,
        parts.headers,
        body,
        asserted,
    )
    .await
}

/// Send one request to the child and stream its reply back.
async fn forward(
    front: &Front,
    method: Method,
    raw_path: &str,
    query: &[u8],
    headers: HeaderMap,
    body: Body,
    asserted: bool,
) -> Response<Body> {
    let mut url = format!("http://127.0.0.1:{}{raw_path}", front.child_port);
    if !query.is_empty() {
        url.push('?');
        url.push_str(&String::from_utf8_lossy(query));
    }
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return raw(503, CHILD_DOWN);
    };
    let tenant = stamp_tenant(&headers);
    // A request that may change the tenant's state voids its stamp until the child's reply sets
    // it again (a reply that never arrives, or one without a stamp, leaves the tenant forwarded).
    if let Some(uid) = tenant.as_deref() {
        if method != Method::GET && method != Method::HEAD {
            front.forget(uid);
        }
    }
    let (sent, tell) = tenant
        .as_deref()
        .map_or((0, false), |uid| front.written_at_send(uid));
    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        if !HOP.contains(&k.as_str()) {
            out.append(k.clone(), v.clone());
        }
    }
    if let Ok(v) = HeaderValue::from_str(&front.secret) {
        out.insert(PARENT_HEADER, v);
    }
    out.insert(
        ASSERTED_HEADER,
        HeaderValue::from_static(if asserted { "1" } else { "0" }),
    );
    if tell {
        out.insert(WRITTEN_HEADER, HeaderValue::from_static("1"));
    }
    let mut req = Request::new(body);
    *req.method_mut() = method;
    *req.uri_mut() = uri;
    *req.headers_mut() = out;
    let resp = match front.http.request(req).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "noevia-diary: child did not answer ({})",
                if e.is_connect() {
                    "connect"
                } else {
                    "transport"
                }
            );
            return raw(503, CHILD_DOWN);
        }
    };
    let status = resp.status().as_u16();
    let mut builder = Response::builder().status(status);
    let mut stamp = None;
    for (k, v) in resp.headers().iter() {
        let name = k.as_str();
        if name == RECOVERED_HEADER {
            stamp = Some(v.as_bytes() == b"1");
            continue;
        }
        if HOP.contains(&name) || name.starts_with(INTERNAL_PREFIX) {
            continue;
        }
        builder = builder.header(k, v);
    }
    // A reply without a stamp (one Starlette's error middleware wrote) leaves the tenant forwarded.
    if let Some(uid) = tenant {
        front.settle(&uid, sent, tell, stamp == Some(true));
    }
    builder
        .body(Body::new(resp.into_body()))
        .unwrap_or_default()
}

/// The control listener's handler: the child's start-up config, then this process's internal
/// routes (answer, reindex, fence, ready), all behind the control token.
pub async fn control(front: Arc<Front>, req: Request<Body>) -> Response<Body> {
    let path = req.uri().path().to_string();
    if !path.starts_with("/internal/") {
        return Response::builder()
            .status(404)
            .body(Body::empty())
            .unwrap_or_default();
    }
    if req.method() == Method::POST && path == "/internal/child/config" {
        let token = req
            .headers()
            .get(server::CONTROL_HEADER)
            .map(|v| v.as_bytes().to_vec())
            .unwrap_or_default();
        if !bool::from(token.as_slice().ct_eq(front.control_token.as_bytes())) {
            return Response::builder()
                .status(403)
                .body(Body::empty())
                .unwrap_or_default();
        }
        let Ok(bytes) = axum::body::to_bytes(req.into_body(), 1024 * 1024).await else {
            return Response::builder()
                .status(413)
                .body(Body::empty())
                .unwrap_or_default();
        };
        let cfg = match std::str::from_utf8(&bytes)
            .map_err(|_| "start-up config is not UTF-8".to_string())
            .and_then(crate::config::parse)
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("noevia-diary: {e}");
                return Response::builder()
                    .status(400)
                    .body(Body::empty())
                    .unwrap_or_default();
            }
        };
        return match front.install(cfg) {
            Ok(()) => {
                eprintln!("noevia-diary: child start-up config installed");
                Response::new(Body::from("ok"))
            }
            Err(e) => {
                eprintln!("noevia-diary: {e}");
                Response::builder()
                    .status(409)
                    .body(Body::empty())
                    .unwrap_or_default()
            }
        };
    }
    match front.app().cloned() {
        Some(app) => server::handle(app, req).await,
        None => Response::builder()
            .status(503)
            .body(Body::empty())
            .unwrap_or_default(),
    }
}

/// Serve `listener` with `handler` until the process stops.
pub async fn serve_with<F, Fut>(
    listener: tokio::net::TcpListener,
    front: Arc<Front>,
    handler: F,
) -> std::io::Result<()>
where
    F: Fn(Arc<Front>, Request<Body>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response<Body>> + Send + 'static,
{
    let router = axum::Router::new().fallback(move |req: Request<Body>| {
        let front = front.clone();
        let handler = handler.clone();
        async move { handler(front, req).await }
    });
    axum::serve(listener, router).await
}

/// 32 random bytes from the kernel, hex.
pub fn random_hex(n: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// The one line the child reads from its stdin.
pub fn child_handshake(front: &Front, control_port: u16) -> String {
    serde_json::json!({
        "control_token": front.control_token,
        "parent_secret": front.secret,
        "control_port": control_port,
        "embedding_epoch": front.embedding_epoch,
    })
    .to_string()
}

/// Run `--main`: bind both listeners, start the child, serve until the child exits or a stop
/// signal arrives. Returns the process exit code.
pub async fn run(args: Args) -> u8 {
    let (token, secret) = match (random_hex(16), random_hex(32)) {
        (Ok(t), Ok(s)) => (t, s),
        _ => {
            eprintln!("noevia-diary: no randomness for the child secrets");
            return 2;
        }
    };
    let front = Arc::new(Front::new(token, secret, args.child_port));
    let public_l = match tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "noevia-diary: cannot listen on {}:{}: {e}",
                args.host, args.port
            );
            return 3;
        }
    };
    let control_l = match tokio::net::TcpListener::bind(("127.0.0.1", args.control_port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "noevia-diary: cannot listen on 127.0.0.1:{}: {e}",
                args.control_port
            );
            return 3;
        }
    };
    let control_port = control_l
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(args.control_port);
    let Some((program, rest)) = args.child.split_first() else {
        return 2;
    };
    let mut child = match tokio::process::Command::new(program)
        .args(rest)
        .env("DIARY_PARENT", "rust")
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("noevia-diary: cannot start the Python child: {e}");
            return 2;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let line = child_handshake(&front, control_port) + "\n";
        if stdin.write_all(line.as_bytes()).await.is_err() {
            eprintln!("noevia-diary: the Python child did not take its handshake");
        }
        drop(stdin);
    }
    eprintln!(
        "noevia-diary: main process on {}:{} (child on 127.0.0.1:{}, control 127.0.0.1:{control_port})",
        args.host, args.port, args.child_port
    );
    tokio::spawn(serve_with(public_l, front.clone(), public));
    tokio::spawn(serve_with(control_l, front.clone(), control));
    let pid = child.id();
    let stop = stop_signal();
    tokio::select! {
        status = child.wait() => {
            eprintln!("noevia-diary: the Python child exited ({}); stopping", status.map(|s| s.to_string()).unwrap_or_else(|_| "unknown".into()));
            1
        }
        () = stop => {
            if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGTERM);
            }
            match tokio::time::timeout(STOP_GRACE, child.wait()).await {
                Ok(_) => 0,
                Err(_) => {
                    let _ = child.kill().await;
                    0
                }
            }
        }
    }
}

async fn stop_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) {
        (Ok(mut term), Ok(mut int)) => {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        _ => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn args_defaults_and_overrides() {
        let a = parse_args(&s(&["--main", "--", "uvicorn", "agent.app:app"])).unwrap();
        assert_eq!(
            (a.host.as_str(), a.port, a.control_port, a.child_port),
            ("0.0.0.0", 8010, 8011, 8012)
        );
        assert_eq!(a.child, s(&["uvicorn", "agent.app:app"]));
        let a = parse_args(&s(&[
            "--main",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--control-port",
            "2",
            "--child-port",
            "3",
            "--",
            "x",
        ]))
        .unwrap();
        assert_eq!((a.port, a.control_port, a.child_port), (1, 2, 3));
        assert!(parse_args(&s(&["--main"])).is_err());
        assert!(parse_args(&s(&["--main", "--port", "x", "--", "y"])).is_err());
        assert!(parse_args(&s(&["--main", "--what", "--", "y"])).is_err());
    }

    #[test]
    fn body_declaration() {
        let mut h = HeaderMap::new();
        assert!(!declares_body(&h));
        h.insert("content-length", HeaderValue::from_static("0"));
        assert!(!declares_body(&h));
        h.insert("content-length", HeaderValue::from_static("3"));
        assert!(declares_body(&h));
        let mut h = HeaderMap::new();
        h.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        assert!(declares_body(&h));
    }

    #[test]
    fn stamps_expire_and_clear() {
        let f = Front::new("t".into(), "s".into(), 1);
        f.stamp("a", true);
        assert!(f.is_recovered("a"));
        f.stamp("a", false);
        assert!(!f.is_recovered("a"));
        f.recovered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                "b".into(),
                Instant::now() - RECOVERED_TTL - Duration::from_secs(1),
            );
        assert!(!f.is_recovered("b"));
    }
}
