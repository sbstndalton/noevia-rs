//! `startEngineRelayFromEnv`'s server (#1876): HTTP/1.1 on one address, the relay's handler behind
//! it, and the limits that keep slow or token-less connections from pinning it: the request head
//! within `headers_timeout`, the whole request body within `request_timeout`, at most
//! `max_connections` open (a connection over the limit is closed at once). Every JSON answer carries
//! `Content-Length` and `Cache-Control: no-store`; a relayed answer streams with the machine's
//! status and content type. A [`Reply::Cut`] (the lease closed) drops the connection without an
//! answer, and so does a cut in the middle of a relayed answer.
//!
//! As Node's server (noevia#2676 item 5): a request whose body has not arrived within
//! `request_timeout` is answered `408` with `Connection: close` and the connection closes
//! (Node writes `HTTP/1.1 408 Request Timeout` and `Connection: close` with no body; here the
//! answer also carries `content-length: 0`); a keep-alive connection with no request in flight
//! closes once `keep_alive_timeout` has passed since its last answer ended, checked every
//! `connections_checking_interval` (so up to one interval later than Node's socket timer).
use crate::bind::Limits;
use crate::{BodyFail, BodySource, Chunk, Machines, Relay, Reply, Request};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, Error>;

/// The lease closed while the request was in flight: the connection goes, with no answer.
#[derive(Debug)]
struct Cut;

impl std::fmt::Display for Cut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the relay lease closed")
    }
}

impl std::error::Error for Cut {}

/// The request body as hyper gives it, read whole within the request's time. `timed_out` tells
/// the listener that a [`Reply::Cut`] was the request timeout (Node's 408), not a closed lease.
struct ReqBody {
    body: Incoming,
    within: Duration,
    timed_out: Arc<AtomicBool>,
}

impl BodySource for ReqBody {
    async fn read(self, cap: usize) -> Result<Vec<u8>, BodyFail> {
        let mut body = self.body;
        let reading = async {
            let mut out = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|_| BodyFail::Failed)?;
                if let Ok(data) = frame.into_data() {
                    if out.len() + data.len() > cap {
                        return Err(BodyFail::TooLarge);
                    }
                    out.extend_from_slice(&data);
                }
            }
            Ok(out)
        };
        let read = tokio::time::timeout(self.within, reading)
            .await
            .unwrap_or(Err(BodyFail::TimedOut));
        if matches!(read, Err(BodyFail::TimedOut)) {
            self.timed_out.store(true, Ordering::SeqCst);
        }
        read
    }
}

/// One connection's requests in flight and when its last answer ended (`None`: none yet; the
/// head timeout covers a connection that never sent a request).
#[derive(Default)]
struct Idle {
    state: Mutex<(usize, Option<Instant>)>,
}

impl Idle {
    /// How long the connection has had nothing in flight since its last answer ended.
    fn idle_for(&self) -> Option<Duration> {
        let s = self.state.lock().ok()?;
        if s.0 > 0 {
            return None;
        }
        s.1.map(|at| at.elapsed())
    }
}

/// A request in flight on a connection, until its answer (body included) is dropped.
struct Busy(Arc<Idle>);

impl Busy {
    fn new(idle: &Arc<Idle>) -> Busy {
        if let Ok(mut s) = idle.state.lock() {
            s.0 += 1;
        }
        Busy(Arc::clone(idle))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Ok(mut s) = self.0.state.lock() {
            s.0 = s.0.saturating_sub(1);
            s.1 = Some(Instant::now());
        }
    }
}

/// Node's answer to a request that did not arrive within `requestTimeout`.
fn request_timeout() -> Response<Body> {
    let mut res = Response::new(
        Full::new(Bytes::new())
            .map_err(|never: std::convert::Infallible| -> Error { match never {} })
            .boxed_unsync(),
    );
    *res.status_mut() = StatusCode::REQUEST_TIMEOUT;
    res.headers_mut()
        .insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    res
}

fn text(v: Option<&HeaderValue>) -> Option<String> {
    // Node reads header values as Latin-1.
    v.map(|v| v.as_bytes().iter().map(|&b| char::from(b)).collect())
}

fn json(status: u16, body: String) -> Response<Body> {
    let n = body.len();
    let mut res = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never: std::convert::Infallible| -> Error { match never {} })
            .boxed_unsync(),
    );
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(CONTENT_LENGTH, HeaderValue::from(n));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

async fn answer<M: Machines>(
    relay: &Relay<M>,
    req: hyper::Request<Incoming>,
    limits: Limits,
) -> Result<Response<Body>, Cut> {
    let (parts, body) = req.into_parts();
    let timed_out = Arc::new(AtomicBool::new(false));
    let request = Request {
        method: parts.method.as_str().to_string(),
        url: parts
            .uri
            .path_and_query()
            .map_or_else(|| "/".to_string(), |p| p.as_str().to_string()),
        authorization: text(parts.headers.get(hyper::header::AUTHORIZATION)),
        accept: text(parts.headers.get(hyper::header::ACCEPT)),
        body: ReqBody {
            body,
            within: limits.request_timeout,
            timed_out: Arc::clone(&timed_out),
        },
    };
    match relay.handle(request).await {
        Reply::Json { status, body } => Ok(json(status, body)),
        Reply::Cut if timed_out.load(Ordering::SeqCst) => Ok(request_timeout()),
        Reply::Cut => Err(Cut),
        Reply::Stream(relayed) => {
            let status = StatusCode::from_u16(relayed.status()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ctype = HeaderValue::from_str(relayed.content_type())
                .unwrap_or_else(|_| HeaderValue::from_static("application/json"));
            let frames = futures_util::stream::unfold(Some(relayed), |state| async move {
                let mut relayed = state?;
                match relayed.next().await {
                    Chunk::Data(d) => Some((Ok(Frame::data(Bytes::from(d))), Some(relayed))),
                    Chunk::End => None,
                    Chunk::Cut => Some((Err(Box::new(Cut) as Error), None)),
                }
            });
            let mut res = Response::new(BodyExt::boxed_unsync(StreamBody::new(frames)));
            *res.status_mut() = status;
            res.headers_mut().insert(CONTENT_TYPE, ctype);
            res.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(res)
        }
    }
}

/// Serve `relay` on `listener` until `stop` resolves (connections already open finish on their own).
pub async fn serve<M: Machines>(
    listener: TcpListener,
    relay: Arc<Relay<M>>,
    limits: Limits,
    stop: impl Future<Output = ()>,
) {
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    tokio::pin!(stop);
    loop {
        let (stream, _) = tokio::select! {
            () = &mut stop => return,
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                // A failed accept (out of descriptors, a reset) is that connection's alone.
                Err(_) => continue,
            },
        };
        // `maxConnections`: one over the limit is closed at once.
        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let relay = Arc::clone(&relay);
        tokio::spawn(async move {
            let _slot = slot;
            let idle = Arc::new(Idle::default());
            let mine = Arc::clone(&idle);
            let service = service_fn(move |req| {
                let relay = Arc::clone(&relay);
                let busy = Busy::new(&mine);
                async move {
                    let res = answer(&relay, req, limits).await?;
                    // The request stays in flight until its answer's body is gone.
                    Ok::<_, Cut>(res.map(|b| {
                        b.map_frame(move |f| {
                            let _held = &busy;
                            f
                        })
                        .boxed_unsync()
                    }))
                }
            });
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.headers_timeout)
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service);
            tokio::pin!(conn);
            // `keepAliveTimeout`: an idle connection is shut down (an answer in flight finishes).
            let mut check = tokio::time::interval(limits.connections_checking_interval);
            let mut closing = false;
            loop {
                tokio::select! {
                    _ = conn.as_mut() => break,
                    _ = check.tick(), if !closing => {
                        if idle.idle_for().is_some_and(|d| d >= limits.keep_alive_timeout) {
                            conn.as_mut().graceful_shutdown();
                            closing = true;
                        }
                    }
                }
            }
        });
    }
}
