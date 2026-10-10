//! TEMPORARY (full-Rust migration, deleted in M11 together with Node): the streaming reverse
//! proxy to the Node server for every route routes.toml does not give to Rust.
//!
//! - One fresh loopback HTTP/1.1 connection per request, driven by a task that is aborted the
//!   moment the client's response is dropped: a client that goes away closes Node's socket, so
//!   Node sees `close` and stops (a chat stops generating), exactly as when it faced the client.
//! - Nothing is buffered: request and response bodies pass frame by frame, so SSE events and
//!   streamed fetch bodies reach the client as Node writes them.
//! - Request bodies are capped at [`BODY_CAP`], just above the largest body any Node route reads
//!   (32 MiB: a stored chat history, a workspace import ZIP), so Node's own per-route 413s still
//!   answer everything it would refuse itself.
//! - Headers pass unchanged (several Set-Cookie included) except hop-by-hop ones and
//!   X-Forwarded-For, which Node reads as the client address only with TRUST_PROXY=true
//!   (auth.cjs clientAddress: the rightmost entry, if it is an IP). The proxy works that address
//!   out the same way from what it received and sends exactly it, so a client cannot add entries
//!   Node would pick up; without TRUST_PROXY no X-Forwarded-For reaches Node at all.

use crate::config::Upstream;
use crate::reply;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use bytes::Bytes;
use http_body::{Body as HttpBody, Frame, SizeHint};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

/// 33 MiB: above every Node route's own limit (STORED_HISTORY_BYTES and the import MAX_UPLOAD,
/// both 32 MiB).
pub const BODY_CAP: u64 = 33 * 1024 * 1024;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(h: &mut HeaderMap) {
    let listed: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for name in listed {
        h.remove(name);
    }
    for name in HOP_BY_HOP {
        h.remove(*name);
    }
}

/// The client address Node's auth.cjs clientAddress(req, trustProxy) would derive from this
/// request, or `None` without TRUST_PROXY (Node then ignores the header).
pub fn forwarded_for(trust_proxy: bool, peer: IpAddr, headers: &HeaderMap) -> Option<String> {
    if !trust_proxy {
        return None;
    }
    // Node joins repeated X-Forwarded-For headers with ", ".
    let joined = headers
        .get_all("x-forwarded-for")
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    let last = joined
        .split(',')
        .map(str::trim)
        .rfind(|s| !s.is_empty())
        .and_then(|s| s.parse::<IpAddr>().ok());
    Some(last.unwrap_or(peer).to_string())
}

/// Aborts the connection task (closing Node's socket) when dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The client's request body on its way to Node, cut at [`BODY_CAP`].
struct CappedBody {
    inner: Body,
    seen: u64,
    over: Arc<AtomicBool>,
}

impl HttpBody for CappedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.seen = self.seen.saturating_add(data.len() as u64);
                    if self.seen > BODY_CAP {
                        self.over.store(true, Ordering::SeqCst);
                        return Poll::Ready(Some(Err("request body over the cap".into())));
                    }
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Node's response body; holds the connection task alive exactly as long as the client reads.
struct UpstreamBody {
    inner: hyper::body::Incoming,
    _conn: AbortOnDrop,
}

impl HttpBody for UpstreamBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Clone)]
pub struct LegacyProxy {
    upstream: Upstream,
    trust_proxy: bool,
}

impl LegacyProxy {
    pub fn new(upstream: Upstream, trust_proxy: bool) -> Self {
        LegacyProxy {
            upstream,
            trust_proxy,
        }
    }

    pub async fn forward(&self, peer: SocketAddr, req: Request<Body>) -> Response<Body> {
        let api = req.uri().path().starts_with("/api/");
        let too_large = || {
            reply::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds size limit",
                api,
            )
        };
        let unavailable = || {
            reply::error(
                StatusCode::BAD_GATEWAY,
                "The server is not answering. Please retry.",
                api,
            )
        };
        let declared = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        if declared.is_some_and(|n| n > BODY_CAP) {
            return too_large();
        }
        let (mut parts, body) = req.into_parts();
        let target = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".into());
        let Ok(uri) = target.parse() else {
            return reply::error(StatusCode::BAD_REQUEST, "invalid URL", api);
        };
        parts.uri = uri;
        parts.version = axum::http::Version::HTTP_11;
        strip_hop_by_hop(&mut parts.headers);
        let xff = forwarded_for(self.trust_proxy, peer.ip(), &parts.headers);
        parts.headers.remove("x-forwarded-for");
        if let Some(v) = xff.and_then(|a| HeaderValue::from_str(&a).ok()) {
            parts.headers.insert("x-forwarded-for", v);
        }
        let over = Arc::new(AtomicBool::new(false));
        let body = CappedBody {
            inner: body,
            seen: 0,
            over: Arc::clone(&over),
        };
        let upstream_req = Request::from_parts(parts, body);

        let Ok(stream) =
            tokio::net::TcpStream::connect((self.upstream.ip, self.upstream.port)).await
        else {
            return unavailable();
        };
        let _ = stream.set_nodelay(true);
        let Ok((mut sender, conn)) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await
        else {
            return unavailable();
        };
        let guard = AbortOnDrop(tokio::spawn(conn).abort_handle());
        match sender.send_request(upstream_req).await {
            Ok(res) => {
                let (mut parts, incoming) = res.into_parts();
                strip_hop_by_hop(&mut parts.headers);
                let body = UpstreamBody {
                    inner: incoming,
                    _conn: guard,
                };
                Response::from_parts(parts, Body::new(body))
            }
            Err(_) if over.load(Ordering::SeqCst) => too_large(),
            Err(_) => unavailable(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn xff(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn forwarded_for_matches_node_client_address() {
        let peer: IpAddr = "172.18.0.4".parse().unwrap();
        assert_eq!(forwarded_for(false, peer, &xff(&["1.2.3.4"])), None);
        assert_eq!(
            forwarded_for(true, peer, &xff(&[])).as_deref(),
            Some("172.18.0.4")
        );
        // A client-supplied first entry never wins: the rightmost is what the trusted proxy added.
        assert_eq!(
            forwarded_for(true, peer, &xff(&["6.6.6.6, 203.0.113.9"])).as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            forwarded_for(true, peer, &xff(&["6.6.6.6", "203.0.113.9"])).as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            forwarded_for(true, peer, &xff(&["203.0.113.9, not-an-ip"])).as_deref(),
            Some("172.18.0.4")
        );
        assert_eq!(
            forwarded_for(true, peer, &xff(&["2001:db8::1 , "])).as_deref(),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn hop_by_hop_headers_go() {
        let mut h = HeaderMap::new();
        h.insert(
            "connection",
            HeaderValue::from_static("keep-alive, x-secret-hop"),
        );
        h.insert("x-secret-hop", HeaderValue::from_static("1"));
        h.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        h.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        h.append("set-cookie", HeaderValue::from_static("a=1"));
        h.append("set-cookie", HeaderValue::from_static("b=2"));
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 2);
        assert_eq!(h.get_all("set-cookie").iter().count(), 2);
    }
}
