//! `startEngineRelayFromEnv`'s server (#1876): HTTP/1.1 on one address, the relay's handler behind
//! it, and the limits that keep slow or token-less connections from pinning it: the request head
//! within `headers_timeout`, the whole request body within `request_timeout`, at most
//! `max_connections` open (a connection over the limit is closed at once). Every JSON answer carries
//! `Content-Length` and `Cache-Control: no-store`; a relayed answer streams with the machine's
//! status and content type. A [`Reply::Cut`] (the lease closed) drops the connection without an
//! answer, and so does a cut in the middle of a relayed answer.
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
use std::sync::Arc;
use std::time::Duration;
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

/// The request body as hyper gives it, read whole within the request's time.
struct ReqBody {
    body: Incoming,
    within: Duration,
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
        tokio::time::timeout(self.within, reading)
            .await
            .unwrap_or(Err(BodyFail::TimedOut))
    }
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
        },
    };
    match relay.handle(request).await {
        Reply::Json { status, body } => Ok(json(status, body)),
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
            let service = service_fn(move |req| {
                let relay = Arc::clone(&relay);
                async move { answer(&relay, req, limits).await }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.headers_timeout)
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}
