//! The in-app server's loopback listener: noevia-core's `server/mcp-internal.cjs`
//! `startInternalServer` in Rust, on tokio + hyper. Unused until a front wires it in behind a switch.
//!
//! What it does, as core's does:
//! - binds only a loopback address ([`bind`] refuses any other before a socket is made; the listener
//!   answers to a capability token, not to a session cookie, so it must never be reachable off-box);
//! - answers `POST <path>` only (the query ignored), anything else 404 `{"error":"not found"}`, before
//!   the body is read;
//! - reads at most [`MAX_BODY_BYTES`] of body, JSON-parses it and hands it with the token after
//!   `Bearer ` to [`Handler::listen`]; a project tool runs through [`project_tools::call`], any other
//!   admitted name goes to the caller's `other` (core keeps the Diary group);
//! - replies `Content-Type: application/json` with `JSON.stringify(body)` (empty for the 202).
//!
//! Named differences from the Node listener, all on paths where Node misbehaves or waits forever:
//! - a body past the limit gets the 400 `invalid request` reply; Node writes that reply after
//!   `req.destroy()`, so its client sees the connection reset instead (filed as noevia#2783);
//! - every wait has a deadline: request headers [`Deadlines::headers`], the body
//!   [`Deadlines::body`] (a body that stops arriving is answered 400 `invalid request`; Node waits for
//!   its server-wide 300 s request timeout and answers 408), and a whole connection
//!   [`Deadlines::connection`];
//! - an accepted connection from a non-loopback peer is dropped unanswered (unreachable once [`bind`]
//!   has refused every non-loopback address; defence in depth);
//! - a tool that panics is answered as Node answers a handler that throws past `handle`: 400
//!   `invalid request`.
//!
//! The tool call itself has no deadline (core's has none); it runs on tokio's blocking pool, since the
//! [`Ports`] are synchronous.
use crate::project_tools::{self, Ports};
use crate::{Handler, Invoke, Reply, Step, MAX_BODY_BYTES, RPC_PATH};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::{TcpListener, TcpStream};

/// A tool outside the project group: `Ok(Some(text))`, `Ok(None)` for core's `null`, or the message
/// it threw.
pub type Other = dyn Fn(&Invoke) -> Result<Option<String>, String> + Send + Sync;
/// Milliseconds since the epoch, as `Date.now()`.
pub type Clock = dyn Fn() -> f64 + Send + Sync;

/// How long the listener waits for each part of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    /// From the connection (or the previous reply) to a complete request head.
    pub headers: Duration,
    /// From the head to the whole body.
    pub body: Duration,
    /// A connection's whole life, keep-alive included.
    pub connection: Duration,
}

impl Default for Deadlines {
    fn default() -> Deadlines {
        Deadlines {
            headers: Duration::from_secs(30),
            body: Duration::from_secs(30),
            connection: Duration::from_secs(300),
        }
    }
}

/// `Date.now()`.
pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// What one listener serves: the handler (its key, catalogue and replay guard), the project ports, the
/// rest of the catalogue's tools, the path, the clock and the deadlines.
pub struct Server {
    handler: Handler,
    ports: Arc<dyn Ports + Send + Sync>,
    other: Arc<Other>,
    path: String,
    clock: Arc<Clock>,
    deadlines: Deadlines,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("path", &self.path)
            .field("deadlines", &self.deadlines)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server at [`RPC_PATH`] with the real clock and the default deadlines.
    pub fn new(
        handler: Handler,
        ports: Arc<dyn Ports + Send + Sync>,
        other: Arc<Other>,
    ) -> Server {
        Server {
            handler,
            ports,
            other,
            path: RPC_PATH.to_owned(),
            clock: Arc::new(now_ms),
            deadlines: Deadlines::default(),
        }
    }

    /// `path: rpcPath`.
    pub fn with_path(mut self, path: &str) -> Server {
        path.clone_into(&mut self.path);
        self
    }

    pub fn with_clock(mut self, clock: Arc<Clock>) -> Server {
        self.clock = clock;
        self
    }

    pub fn with_deadlines(mut self, deadlines: Deadlines) -> Server {
        self.deadlines = deadlines;
        self
    }

    /// The handler (its replay guard, for tests and metrics).
    pub fn handler(&self) -> &Handler {
        &self.handler
    }

    /// One request as [`project_tools::serve`] answers it, at this server's path: the listener rule,
    /// then a project tool here and any other admitted name through `other`.
    pub fn answer(&self, method: &str, url: &str, body: Option<&[u8]>, auth: Option<&str>) -> Reply {
        match self
            .handler
            .listen(method, url, &self.path, body, auth, (self.clock)())
        {
            Step::Reply(r) => r,
            Step::Invoke(inv) => {
                let answered = project_tools::call(&*self.ports, &inv.name, &inv.args, &inv.ctx)
                    .unwrap_or_else(|| (self.other)(&inv));
                inv.finish(answered)
            }
        }
    }

    /// Whether `startInternalServer` reads the body at all: `POST` to the path (the query ignored).
    fn wants_body(&self, method: &str, url: &str) -> bool {
        method == "POST" && url.split('?').next() == Some(self.path.as_str())
    }
}

/// `server.listen(port, host)`: refuses any address that is not loopback, before a socket is made.
pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    if !addr.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("the in-app MCP listener binds loopback only, not {}", addr.ip()),
        ));
    }
    TcpListener::bind(addr).await
}

/// Accepts connections until `shutdown` completes. Connections already accepted finish on their own
/// tasks (each within [`Deadlines::connection`]).
pub async fn run<F>(listener: TcpListener, server: Arc<Server>, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        drop(stream);
                        continue;
                    }
                    let server = Arc::clone(&server);
                    tokio::spawn(async move {
                        let limit = server.deadlines.connection;
                        let _ = tokio::time::timeout(limit, connection(server, stream)).await;
                    });
                }
                // Out of descriptors and the like: wait a little rather than spin.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
        }
    }
}

async fn connection(server: Arc<Server>, stream: TcpStream) -> Result<(), hyper::Error> {
    let headers = server.deadlines.headers;
    let service = service_fn(move |req| {
        let server = Arc::clone(&server);
        async move { Ok::<_, Infallible>(request(server, req).await) }
    });
    http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(headers)
        .serve_connection(TokioIo::new(stream), service)
        .await
}

/// `req.url`: the request target as sent (an absolute-form target stays absolute, so it is a 404).
fn url_of(req: &Request<Incoming>) -> String {
    let uri = req.uri();
    if uri.authority().is_some() {
        return uri.to_string();
    }
    uri.path_and_query()
        .map_or_else(|| uri.path().to_owned(), |pq| pq.as_str().to_owned())
}

async fn request(server: Arc<Server>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let method = req.method().as_str().to_owned();
    let url = url_of(&req);
    // `req.headers.authorization || ''`: a value that is not visible ASCII reads as no token.
    let auth = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = if server.wants_body(&method, &url) {
        let limited = Limited::new(req.into_body(), MAX_BODY_BYTES);
        match tokio::time::timeout(server.deadlines.body, limited.collect()).await {
            Ok(Ok(collected)) => Some(collected.to_bytes()),
            // Too large, broken off or too slow: `listen` answers 400 `invalid request` for no body.
            _ => None,
        }
    } else {
        drop(req);
        None
    };
    let reply = tokio::task::spawn_blocking(move || {
        server.answer(&method, &url, body.as_deref(), auth.as_deref())
    })
    .await
    .unwrap_or_else(|_| invalid_request());
    respond(&reply)
}

/// `.catch(() => reply(400, rpcError(null, -32600, 'invalid request')))`.
fn invalid_request() -> Reply {
    Reply {
        status: 400,
        body: Some(crate::rpc_error(&js_json::JValue::Null, -32600.0, "invalid request")),
    }
}

/// `res.writeHead(status, { 'Content-Type': 'application/json', 'Content-Length': … }); res.end(payload)`.
fn respond(reply: &Reply) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(Bytes::from(reply.payload())));
    *res.status_mut() =
        StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}
