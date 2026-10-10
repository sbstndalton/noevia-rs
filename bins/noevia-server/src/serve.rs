//! The front's HTTP/1.1 accept loop (sbstndalton/noevia#1246, #1247).
//!
//! axum::serve has no header or idle timeouts and no connection cap, so connections are served
//! with hyper directly:
//! - every request carries [`Conn`] (peer AND local address, as Node's `req.socket`), which the
//!   code-network guard reads before any dispatch;
//! - [`Limits::header_read`]: the request line and headers must arrive within it from when the
//!   connection starts waiting for them (hyper's header_read_timeout), so a half-sent request is
//!   closed (Node: headersTimeout);
//! - [`Limits::keep_alive_idle`]: a keep-alive connection with no request in flight and nothing
//!   read for this long is closed (Node: keepAliveTimeout, 5 s). A streaming response (SSE)
//!   counts as in flight until its body ends or is dropped;
//! - [`Limits::max_connections`]: no more are accepted until one closes (they wait in the
//!   kernel's backlog).

use crate::App;
use axum::body::Body;
use axum::extract::connect_info::Connected;
use axum::http::{Request, Response};
use axum::serve::IncomingStream;
use axum::Router;
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

/// The connection a request arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conn {
    pub peer: SocketAddr,
    /// `None` only when the OS could not report it; the guard then refuses (fails closed).
    pub local: Option<SocketAddr>,
}

impl Connected<IncomingStream<'_, TcpListener>> for Conn {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Conn {
            peer: *stream.remote_addr(),
            local: stream.io().local_addr().ok(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub header_read: Duration,
    pub keep_alive_idle: Duration,
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            header_read: Duration::from_secs(30),
            keep_alive_idle: Duration::from_secs(5),
            max_connections: 4096,
        }
    }
}

/// Per-connection activity: requests in flight and the last time anything happened.
struct Activity {
    start: Instant,
    in_flight: AtomicUsize,
    last_ms: AtomicU64,
}

impl Activity {
    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }
    fn idle_for(&self) -> Duration {
        let last = Duration::from_millis(self.last_ms.load(Ordering::Relaxed));
        self.start.elapsed().saturating_sub(last)
    }
}

/// Marks a request in flight until the response body ends or is dropped.
struct InFlight(Arc<Activity>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.touch();
        self.0.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

struct TrackedBody {
    inner: Body,
    _flight: InFlight,
}

impl HttpBody for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The socket, noting every read that returns bytes.
struct TrackedIo {
    inner: TcpStream,
    activity: Arc<Activity>,
}

impl AsyncRead for TrackedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.activity.touch();
        }
        r
    }
}

impl AsyncWrite for TrackedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Serves `app` on `listener` until `shutdown` resolves; then stops accepting and lets open
/// connections finish their current request (the caller bounds how long).
pub async fn serve(
    listener: TcpListener,
    app: Arc<App>,
    limits: Limits,
    shutdown: impl Future<Output = ()> + Send + 'static,
) {
    let router = crate::router(app);
    let max = limits
        .max_connections
        .clamp(1, tokio::sync::Semaphore::MAX_PERMITS);
    let permits = Arc::new(tokio::sync::Semaphore::new(max));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    });
    let mut stop = stop_rx.clone();
    accept_loop(&listener, &permits, &router, limits, &stop_rx, &mut stop).await;
    drop(listener);
    // Every connection holds a permit until it closes: wait for all of them.
    let _ = permits
        .acquire_many(u32::try_from(max).unwrap_or(u32::MAX))
        .await;
}

async fn accept_loop(
    listener: &TcpListener,
    permits: &Arc<tokio::sync::Semaphore>,
    router: &Router,
    limits: Limits,
    stop_rx: &tokio::sync::watch::Receiver<bool>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let permit = tokio::select! {
            p = Arc::clone(permits).acquire_owned() => match p { Ok(p) => p, Err(_) => return },
            () = stopped(stop) => return,
        };
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(a) => a,
                Err(_) => {
                    // EMFILE and the like: back off instead of spinning.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            () = stopped(stop) => return,
        };
        let _ = stream.set_nodelay(true);
        let conn = Conn {
            peer,
            local: stream.local_addr().ok(),
        };
        let router = router.clone();
        let stop = stop_rx.clone();
        tokio::spawn(async move {
            serve_connection(stream, conn, router, limits, stop).await;
            drop(permit);
        });
    }
}

async fn serve_connection(
    stream: TcpStream,
    conn: Conn,
    router: Router,
    limits: Limits,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let activity = Arc::new(Activity {
        start: Instant::now(),
        in_flight: AtomicUsize::new(0),
        last_ms: AtomicU64::new(0),
    });
    let io = TrackedIo {
        inner: stream,
        activity: Arc::clone(&activity),
    };
    let act = Arc::clone(&activity);
    let svc = hyper::service::service_fn(move |mut req: Request<Incoming>| {
        let router = router.clone();
        act.in_flight.fetch_add(1, Ordering::Relaxed);
        act.touch();
        let flight = InFlight(Arc::clone(&act));
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(conn));
        async move {
            let res: Response<Body> = match router.oneshot(req.map(Body::new)).await {
                Ok(r) => r,
                Err(e) => match e {},
            };
            Ok::<_, Infallible>(res.map(|inner| TrackedBody {
                inner,
                _flight: flight,
            }))
        }
    });
    let connection = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read)
        .keep_alive(true)
        .serve_connection(TokioIo::new(io), svc);
    tokio::pin!(connection);
    let tick = (limits.keep_alive_idle / 4).max(Duration::from_millis(10));
    let mut closing = false;
    loop {
        tokio::select! {
            _ = connection.as_mut() => return,
            _ = tokio::time::sleep(tick), if !closing => {
                if activity.in_flight.load(Ordering::Relaxed) == 0
                    && activity.idle_for() >= limits.keep_alive_idle
                {
                    closing = true;
                    connection.as_mut().graceful_shutdown();
                }
            }
            () = stopped(&mut stop), if !closing => {
                closing = true;
                connection.as_mut().graceful_shutdown();
            }
        }
    }
}

/// Resolves once the stop flag is set (or its sender is gone).
async fn stopped(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await.map(|_| ());
}
