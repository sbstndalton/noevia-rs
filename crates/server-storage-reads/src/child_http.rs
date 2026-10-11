//! Private one-attempt literal-IP plain HTTP; no resolver/TLS/codecs/activation.
use crate::{
    isolated::Error,
    protocol::{Method, Request, TransportFailure},
    response_ipc::{self, Phase, Reply},
};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use hyper::{
    client::conn::http1,
    http::{HeaderMap, HeaderName, HeaderValue, Uri},
    Request as HttpRequest,
};
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    error::Error as StdError,
    future::poll_fn,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::{timeout_at, Instant},
};
pub const WIRE: usize = 64 * 1024 * 1024;
pub const HEADERS: usize = 65536;
const STEP: usize = 4096;
struct Empty;
impl Body for Empty {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(None)
    }
    fn is_end_stream(&self) -> bool {
        true
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(0)
    }
}
struct LimitedIo {
    socket: TcpStream,
    wire: usize,
    invalid_headers: Arc<AtomicBool>,
}
impl AsyncRead for LimitedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.invalid_headers.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::other("fixed header refusal")));
        }
        let mut scratch = [0; STEP];
        let take = output
            .remaining()
            .min(STEP)
            .min(WIRE.saturating_sub(self.wire).saturating_add(1));
        let mut input = ReadBuf::new(
            scratch
                .get_mut(..take)
                .ok_or_else(|| io::Error::other("fixed IO refusal"))?,
        );
        match Pin::new(&mut self.socket).poll_read(cx, &mut input) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {
                let n = input.filled().len();
                if n > WIRE.saturating_sub(self.wire) {
                    return Poll::Ready(Err(io::Error::other("fixed wire refusal")));
                }
                self.wire += n;
                output.put_slice(input.filled());
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl AsyncWrite for LimitedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        let take = input.len().min(STEP).min(WIRE.saturating_sub(self.wire));
        if take == 0 && !input.is_empty() {
            return Poll::Ready(Err(io::Error::other("fixed wire refusal")));
        }
        match Pin::new(&mut self.socket).poll_write(
            cx,
            input
                .get(..take)
                .ok_or_else(|| io::Error::other("fixed IO refusal"))?,
        ) {
            Poll::Ready(Ok(n)) => {
                self.wire += n;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}
fn failure(phase: Phase, error: TransportFailure, cap: usize) -> Result<Vec<u8>, Error> {
    response_ipc::encode(
        Reply::Failure {
            phase,
            failure: error,
        },
        cap,
    )
}
fn typed_io(error: &(dyn StdError + 'static)) -> Option<TransportFailure> {
    let mut current = Some(error);
    for _ in 0..16 {
        let Some(e) = current else {
            break;
        };
        if let Some(e) = e.downcast_ref::<io::Error>() {
            return match e.kind() {
                io::ErrorKind::ConnectionReset => Some(TransportFailure::PreHeaderReset),
                io::ErrorKind::ConnectionAborted => Some(TransportFailure::PreHeaderAborted),
                io::ErrorKind::BrokenPipe => Some(TransportFailure::PreHeaderBrokenPipe),
                io::ErrorKind::UnexpectedEof => Some(TransportFailure::PreHeaderUnexpectedEof),
                _ => None,
            };
        }
        current = e.source();
    }
    None
}
fn header_budget(headers: &HeaderMap) -> bool {
    let mut total = 0usize;
    for (name, value) in headers {
        let Some(n) = total
            .checked_add(name.as_str().len())
            .and_then(|n| n.checked_add(value.as_bytes().len()))
        else {
            return false;
        };
        if n > HEADERS {
            return false;
        }
        total = n;
    }
    headers.len() <= 100
}
struct Driver(tokio::task::JoinHandle<Result<(), hyper::Error>>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// Inputs come from the validated Request codec; caller retains authorization.
pub async fn execute(request: Request) -> Result<Vec<u8>, Error> {
    if !matches!(request.body_cap, crate::LIST_BYTES | crate::TEXT_BYTES)
        || request.url.len() > 65536
        || request.headers.len() > 100
        || request.attempt_timeout.is_zero()
        || request.attempt_timeout > Duration::from_secs(20)
    {
        return Err(Error::Input);
    }
    let cap = request.body_cap;
    let deadline = Instant::now() + request.attempt_timeout;
    let uri: Uri = match request.url.parse() {
        Ok(u) => u,
        Err(_) => return failure(Phase::Headers, TransportFailure::Header, cap),
    };
    if uri.scheme_str() != Some("http") {
        return failure(Phase::Tls, TransportFailure::Tls, cap);
    }
    let Some(authority) = uri.authority() else {
        return failure(Phase::Headers, TransportFailure::Header, cap);
    };
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let ip: IpAddr = match host.parse() {
        Ok(ip) => ip,
        Err(_) => return failure(Phase::Dns, TransportFailure::Dns, cap),
    };
    let endpoint = SocketAddr::new(ip, authority.port_u16().unwrap_or(80));
    let path = uri.path_and_query().map(|s| s.as_str()).unwrap_or("/");
    let mut outgoing = match HttpRequest::builder()
        .method(match request.method {
            Method::Get => "GET",
            Method::Propfind => "PROPFIND",
        })
        .uri(path)
        .body(Empty)
    {
        Ok(r) => r,
        Err(_) => return failure(Phase::Headers, TransportFailure::Header, cap),
    };
    let mut aggregate = 0usize;
    for (name, value) in request.headers {
        if name.len() > 128 || value.len() > 12 * 1024 * 1024 {
            return failure(Phase::Headers, TransportFailure::Header, cap);
        }
        aggregate = match aggregate
            .checked_add(name.len())
            .and_then(|n| n.checked_add(value.len()))
        {
            Some(n) if n <= crate::isolated::INPUT => n,
            _ => return failure(Phase::Headers, TransportFailure::Header, cap),
        };
        let name = match HeaderName::from_bytes(name.as_bytes()) {
            Ok(n) => n,
            Err(_) => return failure(Phase::Headers, TransportFailure::Header, cap),
        };
        let mut value = match HeaderValue::from_bytes(value.as_bytes()) {
            Ok(v) => v,
            Err(_) => return failure(Phase::Headers, TransportFailure::Header, cap),
        };
        value.set_sensitive(true);
        outgoing.headers_mut().append(name, value);
    }
    if !outgoing.headers().contains_key("host") {
        let value = match HeaderValue::from_str(authority.as_str()) {
            Ok(v) => v,
            Err(_) => return failure(Phase::Headers, TransportFailure::Header, cap),
        };
        outgoing.headers_mut().insert("host", value);
    }
    let invalid_headers = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&invalid_headers);
    hyper::ext::on_informational(&mut outgoing, move |response| {
        if !header_budget(response.headers()) {
            observed.store(true, Ordering::Release);
        }
    });
    let socket = match timeout_at(deadline, TcpStream::connect(endpoint)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return failure(
                Phase::Tcp,
                typed_io(&e).unwrap_or(TransportFailure::Other),
                cap,
            )
        }
        Err(_) => return failure(Phase::Tcp, TransportFailure::Timeout, cap),
    };
    let mut builder = http1::Builder::new();
    builder.max_headers(100).max_buf_size(65536);
    let (mut sender, connection) = match timeout_at(
        deadline,
        builder.handshake(TokioIo::new(LimitedIo {
            socket,
            wire: 0,
            invalid_headers: Arc::clone(&invalid_headers),
        })),
    )
    .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            return failure(
                Phase::Headers,
                typed_io(&e).unwrap_or(TransportFailure::Header),
                cap,
            )
        }
        Err(_) => return failure(Phase::Headers, TransportFailure::Timeout, cap),
    };
    let _driver = Driver(tokio::spawn(connection));
    let response = match timeout_at(deadline, sender.send_request(outgoing)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return failure(
                Phase::Headers,
                typed_io(&e).unwrap_or(TransportFailure::Header),
                cap,
            )
        }
        Err(_) => return failure(Phase::Headers, TransportFailure::Timeout, cap),
    };
    if invalid_headers.load(Ordering::Acquire) || !header_budget(response.headers()) {
        return failure(Phase::Headers, TransportFailure::Header, cap);
    }
    let status = response.status().as_u16();
    if !(200..=599).contains(&status) {
        return failure(Phase::Headers, TransportFailure::Header, cap);
    }
    if (300..400).contains(&status) && response.headers().contains_key("location") {
        return failure(Phase::Headers, TransportFailure::Redirect, cap);
    }
    if status >= 300 {
        return response_ipc::encode(
            Reply::Http {
                status,
                capped: false,
                payload: &[],
            },
            cap,
        );
    }
    // Plain identity slice only. Codec behavior must be approved independently.
    if response
        .headers()
        .get_all("content-encoding")
        .iter()
        .any(|v| v.as_bytes() != b"identity")
    {
        return failure(Phase::Body, TransportFailure::Body, cap);
    }
    let body_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
    let mut body = response.into_body();
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(cap + 1)
        .map_err(|_| Error::Output)?;
    loop {
        let frame = match timeout_at(
            body_deadline,
            poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        {
            Ok(Some(Ok(f))) => f,
            Ok(Some(Err(_))) => return failure(Phase::Body, TransportFailure::Body, cap),
            Ok(None) => break,
            Err(_) => return failure(Phase::Body, TransportFailure::Timeout, cap),
        };
        if let Some(trailers) = frame.trailers_ref() {
            if !header_budget(trailers) {
                return failure(Phase::Body, TransportFailure::Body, cap);
            }
        }
        if let Ok(data) = frame.into_data() {
            for part in data.chunks(STEP) {
                if Instant::now() >= body_deadline {
                    return failure(Phase::Body, TransportFailure::Timeout, cap);
                }
                let take = part.len().min(cap + 1 - payload.len());
                payload.extend_from_slice(part.get(..take).ok_or(Error::Output)?);
                if payload.len() == cap + 1 {
                    return response_ipc::encode(
                        Reply::Http {
                            status,
                            capped: true,
                            payload: &payload,
                        },
                        cap,
                    );
                }
            }
        }
    }
    response_ipc::encode(
        Reply::Http {
            status,
            capped: false,
            payload: &payload,
        },
        cap,
    )
}
/// Runtime startup precedes READY. No DNS worker is created in this phase.
pub fn runtime() -> Result<tokio::runtime::Runtime, Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .thread_stack_size(2 * 1024 * 1024)
        .build()
        .map_err(|_| Error::Setup)
}
