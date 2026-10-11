//! Private owned DNS/TCP/TLS/HTTP attempt with bounded response codecs; no activation.
#[path = "response_codecs.rs"]
mod response_codecs;
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
pub const TLS_WIRE: usize = 64 * 1024 * 1024;

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tls_wire_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    struct FixedInput {
        remaining: usize,
        maximum: usize,
    }
    impl AsyncRead for FixedInput {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            b: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            self.maximum = self.maximum.max(b.remaining());
            let n = b.remaining().min(self.remaining);
            b.put_slice([0; STEP].get(..n).unwrap_or_else(|| unreachable!()));
            self.remaining -= n;
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for FixedInput {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            b: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.maximum = self.maximum.max(b.len());
            Poll::Ready(Ok(b.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    #[test]
    fn encrypted_counter_boundary_and_shared_direction_budget() {
        runtime()
            .unwrap_or_else(|_| unreachable!())
            .block_on(async {
                let mut io = LimitedIo {
                    socket: FixedInput {
                        remaining: TLS_WIRE + 1,
                        maximum: 0,
                    },
                    wire: 0,
                    limit: TLS_WIRE,
                    invalid_headers: Arc::new(AtomicBool::new(false)),
                };
                let mut output = [0; STEP * 2];
                for _ in 0..TLS_WIRE / STEP {
                    assert_eq!(
                        io.read(&mut output)
                            .await
                            .unwrap_or_else(|_| unreachable!()),
                        STEP
                    );
                }
                assert_eq!(io.wire, TLS_WIRE);
                output.fill(7);
                assert!(io.read(&mut output).await.is_err());
                assert!(output.iter().all(|n| *n == 7));
                assert!(io.write(b"x").await.is_err());
                assert_eq!(io.socket.maximum, STEP);
                let mut io = LimitedIo {
                    socket: FixedInput {
                        remaining: 1,
                        maximum: 0,
                    },
                    wire: TLS_WIRE - 1,
                    limit: TLS_WIRE,
                    invalid_headers: Arc::new(AtomicBool::new(false)),
                };
                assert_eq!(io.write(b"ab").await.unwrap_or_else(|_| unreachable!()), 1);
                assert!(io.read(&mut output).await.is_err());
            });
    }
}
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
struct LimitedIo<IO> {
    socket: IO,
    wire: usize,
    limit: usize,
    invalid_headers: Arc<AtomicBool>,
}
impl<IO: AsyncRead + Unpin> AsyncRead for LimitedIo<IO> {
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
            .min(self.limit.saturating_sub(self.wire).saturating_add(1));
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
                if n > self.limit.saturating_sub(self.wire) {
                    return Poll::Ready(Err(io::Error::other("fixed wire refusal")));
                }
                self.wire += n;
                output.put_slice(input.filled());
                Poll::Ready(Ok(()))
            }
        }
    }
}
impl<IO: AsyncWrite + Unpin> AsyncWrite for LimitedIo<IO> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        let take = input
            .len()
            .min(STEP)
            .min(self.limit.saturating_sub(self.wire));
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
    execute_inner(request, None, None).await
}
/// Immutable production trust: no external config/root/identity constructor.
pub struct ProductionTls(Arc<rustls::ClientConfig>);
impl ProductionTls {
    /// Must complete before READY or secret-bearing input.
    pub fn new() -> Result<Self, Error> {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::Setup)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.resumption = rustls::client::Resumption::disabled();
        config.enable_early_data = false;
        config.enable_sni = true;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self(Arc::new(config)))
    }
}
pub async fn execute_tls(request: Request, policy: &ProductionTls) -> Result<Vec<u8>, Error> {
    execute_inner(request, Some(Arc::clone(&policy.0)), None).await
}
/// Opaque preparation; execution cannot replace the request, identity or deadline.
pub struct OwnedDnsAttempt(OwnedDnsState);
#[allow(clippy::large_enum_variant)] // Fixed 32 address slots stay inline, without a new heap box.
enum OwnedDnsState {
    Refused(Vec<u8>),
    Ready {
        transport: PreparedTransport,
        addresses: Addresses,
        deadline: std::time::Instant,
    },
}
struct Addresses {
    slots: [Option<SocketAddr>; 32],
    count: usize,
}
fn collect_addresses(
    mut results: impl Iterator<Item = SocketAddr>,
    deadline: std::time::Instant,
) -> Result<Addresses, TransportFailure> {
    let mut addresses = Addresses {
        slots: [None; 32],
        count: 0,
    };
    // At most 33 iterator results. Result 33 refuses before a retained copy.
    for _ in 0..33 {
        if std::time::Instant::now() >= deadline {
            return Err(TransportFailure::Timeout);
        }
        let Some(address) = results.next() else {
            return if addresses.count == 0 {
                Err(TransportFailure::Dns)
            } else {
                Ok(addresses)
            };
        };
        if std::time::Instant::now() >= deadline {
            return Err(TransportFailure::Timeout);
        }
        let Some(slot) = addresses.slots.get_mut(addresses.count) else {
            return Err(TransportFailure::Dns);
        };
        *slot = Some(address);
        addresses.count += 1;
    }
    Err(TransportFailure::Dns)
}
struct PreparedTransport {
    outgoing: HttpRequest<Empty>,
    invalid_headers: Arc<AtomicBool>,
    tls: Option<Arc<rustls::ClientConfig>>,
    identity: Option<rustls::pki_types::ServerName<'static>>,
    cap: usize,
}
/// Resolve synchronously on the calling owned main thread, outside the runtime.
/// Std/libc internal allocation is not bounded by our fixed retained slots.
pub fn prepare_owned_dns(
    request: Request,
    policy: &ProductionTls,
) -> Result<OwnedDnsAttempt, Error> {
    use std::net::ToSocketAddrs;
    prepare_dns_inner(request, Arc::clone(&policy.0), |host, port| {
        (host, port).to_socket_addrs()
    })
}
// Source-inclusion fixture seam only; no production resolver/config constructor.
pub(crate) fn prepare_dns_inner<I: Iterator<Item = SocketAddr>>(
    request: Request,
    tls: Arc<rustls::ClientConfig>,
    resolver: impl FnOnce(&str, u16) -> io::Result<I>,
) -> Result<OwnedDnsAttempt, Error> {
    if !matches!(request.body_cap, crate::LIST_BYTES | crate::TEXT_BYTES)
        || request.url.len() > 65536
        || request.headers.len() > 100
        || request.attempt_timeout.is_zero()
        || request.attempt_timeout > Duration::from_secs(20)
    {
        return Err(Error::Input);
    }
    let cap = request.body_cap;
    let deadline = std::time::Instant::now()
        .checked_add(request.attempt_timeout)
        .ok_or(Error::Input)?;
    let refuse = |phase, error| {
        failure(phase, error, cap).map(|bytes| OwnedDnsAttempt(OwnedDnsState::Refused(bytes)))
    };
    let uri: Uri = match request.url.parse() {
        Ok(uri) => uri,
        Err(_) => return refuse(Phase::Headers, TransportFailure::Header),
    };
    let encrypted = uri.scheme_str() == Some("https");
    if uri.scheme_str() != Some("http") && !encrypted {
        return refuse(Phase::Tls, TransportFailure::Tls);
    }
    let Some(authority) = uri.authority() else {
        return refuse(Phase::Headers, TransportFailure::Header);
    };
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let port = authority
        .port_u16()
        .unwrap_or(if encrypted { 443 } else { 80 });
    let literal = host.parse::<IpAddr>().ok();
    let canonical;
    let identity_host = if literal.is_some() {
        host
    } else {
        canonical = match url::Url::parse(&request.url) {
            Ok(url) => url,
            Err(_) => return refuse(Phase::Dns, TransportFailure::Dns),
        };
        let Some(url::Host::Domain(name)) = canonical.host() else {
            return refuse(Phase::Dns, TransportFailure::Dns);
        };
        if name.is_empty()
            || !name.is_ascii()
            || name.len() > 253
            || name.as_bytes().contains(&0)
            || !name.eq_ignore_ascii_case(host)
            || canonical.scheme() != uri.scheme_str().unwrap_or("")
            || canonical.port_or_known_default() != Some(port)
            || !canonical.username().is_empty()
            || canonical.password().is_some()
        {
            return refuse(Phase::Dns, TransportFailure::Dns);
        }
        name
    };
    let identity = if encrypted {
        match rustls::pki_types::ServerName::try_from(identity_host.to_owned()) {
            Ok(name) => Some(name),
            Err(_) => return refuse(Phase::Tls, TransportFailure::Tls),
        }
    } else {
        None
    };
    let path = uri.path_and_query().map(|s| s.as_str()).unwrap_or("/");
    let mut outgoing = match HttpRequest::builder()
        .method(match request.method {
            Method::Get => "GET",
            Method::Propfind => "PROPFIND",
        })
        .uri(path)
        .body(Empty)
    {
        Ok(request) => request,
        Err(_) => return refuse(Phase::Headers, TransportFailure::Header),
    };
    let mut aggregate = 0usize;
    for (name, value) in request.headers {
        if name.len() > 128 || value.len() > 12 * 1024 * 1024 {
            return refuse(Phase::Headers, TransportFailure::Header);
        }
        aggregate = match aggregate
            .checked_add(name.len())
            .and_then(|n| n.checked_add(value.len()))
        {
            Some(n) if n <= crate::isolated::INPUT => n,
            _ => return refuse(Phase::Headers, TransportFailure::Header),
        };
        let name = match HeaderName::from_bytes(name.as_bytes()) {
            Ok(name) => name,
            Err(_) => return refuse(Phase::Headers, TransportFailure::Header),
        };
        let mut value = match HeaderValue::from_bytes(value.as_bytes()) {
            Ok(value) => value,
            Err(_) => return refuse(Phase::Headers, TransportFailure::Header),
        };
        value.set_sensitive(true);
        outgoing.headers_mut().append(name, value);
    }
    if !outgoing.headers().contains_key("host") {
        let value = match HeaderValue::from_str(authority.as_str()) {
            Ok(value) => value,
            Err(_) => return refuse(Phase::Headers, TransportFailure::Header),
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
    if std::time::Instant::now() >= deadline {
        return refuse(Phase::Dns, TransportFailure::Timeout);
    }
    let addresses = if let Some(ip) = literal {
        collect_addresses(std::iter::once(SocketAddr::new(ip, port)), deadline)
    } else {
        let results = resolver(identity_host, port);
        if std::time::Instant::now() >= deadline {
            return refuse(Phase::Dns, TransportFailure::Timeout);
        }
        match results {
            Ok(results) => collect_addresses(results, deadline),
            Err(_) => Err(TransportFailure::Dns),
        }
    };
    let addresses = match addresses {
        Ok(addresses) => addresses,
        Err(error) => return refuse(Phase::Dns, error),
    };
    if std::time::Instant::now() >= deadline {
        return refuse(Phase::Dns, TransportFailure::Timeout);
    }
    Ok(OwnedDnsAttempt(OwnedDnsState::Ready {
        transport: PreparedTransport {
            outgoing,
            invalid_headers,
            tls: encrypted.then_some(tls),
            identity,
            cap,
        },
        addresses,
        deadline,
    }))
}
/// Serial OS iterator order; first TCP success wins, with no fallback after TLS.
pub async fn execute_owned_dns(attempt: OwnedDnsAttempt) -> Result<Vec<u8>, Error> {
    let (transport, addresses, deadline) = match attempt.0 {
        OwnedDnsState::Refused(bytes) => return Ok(bytes),
        OwnedDnsState::Ready {
            transport,
            addresses,
            deadline,
        } => (transport, addresses, deadline),
    };
    if std::time::Instant::now() >= deadline {
        return failure(Phase::Dns, TransportFailure::Timeout, transport.cap);
    }
    let deadline = Instant::from_std(deadline);
    let mut last = TransportFailure::Other;
    for endpoint in addresses.slots.into_iter().take(addresses.count).flatten() {
        if Instant::now() >= deadline {
            return failure(Phase::Tcp, TransportFailure::Timeout, transport.cap);
        }
        match timeout_at(deadline, TcpStream::connect(endpoint)).await {
            Ok(Ok(socket)) => return connected(socket, transport, deadline).await,
            Ok(Err(error)) => last = typed_io(&error).unwrap_or(TransportFailure::Other),
            Err(_) => return failure(Phase::Tcp, TransportFailure::Timeout, transport.cap),
        }
    }
    failure(Phase::Tcp, last, transport.cap)
}
// Crate-private only. A separate synthetic artifact includes this actual module
// as local source to supply fixed fixture trust/address; production exports neither.
pub(crate) async fn execute_inner(
    request: Request,
    tls: Option<Arc<rustls::ClientConfig>>,
    fixture_target: Option<SocketAddr>,
) -> Result<Vec<u8>, Error> {
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
    let encrypted = uri.scheme_str() == Some("https");
    if uri.scheme_str() != Some("http") && !encrypted || encrypted && tls.is_none() {
        return failure(Phase::Tls, TransportFailure::Tls, cap);
    }
    let Some(authority) = uri.authority() else {
        return failure(Phase::Headers, TransportFailure::Header, cap);
    };
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let endpoint = match fixture_target {
        Some(endpoint) if endpoint.ip().is_loopback() && host == "localhost" => endpoint,
        Some(_) => return Err(Error::Input),
        None => {
            let ip: IpAddr = match host.parse() {
                Ok(ip) => ip,
                Err(_) => return failure(Phase::Dns, TransportFailure::Dns, cap),
            };
            SocketAddr::new(
                ip,
                authority
                    .port_u16()
                    .unwrap_or(if encrypted { 443 } else { 80 }),
            )
        }
    };
    let identity = if encrypted {
        match rustls::pki_types::ServerName::try_from(host.to_owned()) {
            Ok(name) => Some(name),
            Err(_) => return failure(Phase::Tls, TransportFailure::Tls, cap),
        }
    } else {
        None
    };
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
    connected(
        socket,
        PreparedTransport {
            outgoing,
            invalid_headers,
            tls: if encrypted { tls } else { None },
            identity,
            cap,
        },
        deadline,
    )
    .await
}
async fn connected(
    socket: TcpStream,
    transport: PreparedTransport,
    deadline: Instant,
) -> Result<Vec<u8>, Error> {
    let PreparedTransport {
        outgoing,
        invalid_headers,
        tls,
        identity,
        cap,
    } = transport;
    if tls.is_some() {
        let policy = tls.ok_or(Error::Input)?;
        let name = identity.ok_or(Error::Input)?;
        let wire = LimitedIo {
            socket,
            wire: 0,
            limit: TLS_WIRE,
            invalid_headers: Arc::clone(&invalid_headers),
        };
        let connector = tokio_rustls::TlsConnector::from(policy);
        let stream = match timeout_at(
            deadline,
            connector.connect_with(name, wire, |session| session.set_buffer_limit(Some(65536))),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) => return failure(Phase::Tls, TransportFailure::Tls, cap),
            Err(_) => return failure(Phase::Tls, TransportFailure::Timeout, cap),
        };
        exchange(stream, outgoing, invalid_headers, deadline, cap).await
    } else {
        exchange(socket, outgoing, invalid_headers, deadline, cap).await
    }
}
async fn exchange<IO: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    socket: IO,
    outgoing: HttpRequest<Empty>,
    invalid_headers: Arc<AtomicBool>,
    deadline: Instant,
    cap: usize,
) -> Result<Vec<u8>, Error> {
    let mut builder = http1::Builder::new();
    builder.max_headers(100).max_buf_size(65536);
    let (mut sender, connection) = match timeout_at(
        deadline,
        builder.handshake(TokioIo::new(LimitedIo {
            socket,
            wire: 0,
            limit: WIRE,
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
    let body_deadline = deadline.min(Instant::now() + Duration::from_secs(10));
    if status == 204 || status == 205 {
        return response_ipc::encode(
            Reply::Http {
                status,
                capped: false,
                payload: &[],
            },
            cap,
        );
    }
    let codec_plan = match response_codecs::Plan::headers(response.headers()) {
        Ok(plan) => plan,
        Err(_) => return failure(Phase::Body, TransportFailure::Body, cap),
    };
    let mut codecs = match response_codecs::Pipeline::new(codec_plan, body_deadline.into_std()) {
        Ok(pipeline) => pipeline,
        Err(response_codecs::Fault::Timeout) => {
            return failure(Phase::Body, TransportFailure::Timeout, cap)
        }
        Err(response_codecs::Fault::Body) => {
            return failure(Phase::Body, TransportFailure::Body, cap)
        }
    };
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
                let progress = match codecs.feed(part, &mut payload, cap) {
                    Ok(progress) => progress,
                    Err(response_codecs::Fault::Timeout) => {
                        return failure(Phase::Body, TransportFailure::Timeout, cap)
                    }
                    Err(response_codecs::Fault::Body) => {
                        return failure(Phase::Body, TransportFailure::Body, cap)
                    }
                };
                if matches!(progress, response_codecs::Progress::Capped) {
                    // Compressed cap+1 remains refused pending the actual malformed-overcap gate.
                    if codecs.compressed() {
                        return failure(Phase::Body, TransportFailure::Body, cap);
                    }
                    if Instant::now() >= body_deadline {
                        return failure(Phase::Body, TransportFailure::Timeout, cap);
                    }
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
    match codecs.finish(&mut payload, cap) {
        Ok(response_codecs::Progress::Complete) => {}
        Err(response_codecs::Fault::Timeout) => {
            return failure(Phase::Body, TransportFailure::Timeout, cap)
        }
        _ => return failure(Phase::Body, TransportFailure::Body, cap),
    }
    if Instant::now() >= body_deadline {
        return failure(Phase::Body, TransportFailure::Timeout, cap);
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

#[cfg(test)]
mod owned_dns_tests {
    use super::*;
    #[test]
    fn dns_fixed_collector_preserves_duplicates_and_ipv6_representation() {
        let v6 = SocketAddr::V6(std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::LOCALHOST,
            81,
            7,
            9,
        ));
        let v4 = SocketAddr::from(([127, 0, 0, 1], 80));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let addresses = collect_addresses([v6, v4, v6].into_iter(), deadline)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(addresses.count, 3);
        assert_eq!(
            addresses.slots.get(..3),
            Some([Some(v6), Some(v4), Some(v6)].as_slice())
        );
        let addresses = collect_addresses(std::iter::repeat_n(v4, 32), deadline)
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(addresses.count, 32);
        assert!(addresses.slots.iter().all(|slot| *slot == Some(v4)));
        let mut consumed = 0;
        let refused = collect_addresses(
            std::iter::repeat_n(v4, 34).inspect(|_| consumed += 1),
            deadline,
        );
        assert!(matches!(refused, Err(TransportFailure::Dns)));
        assert_eq!(consumed, 33);
        assert!(matches!(
            collect_addresses(std::iter::empty(), deadline),
            Err(TransportFailure::Dns)
        ));
    }
}
