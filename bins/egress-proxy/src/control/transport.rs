//! Live socket adapter to the existing authenticated controller. No production
//! listener or CLI enables it yet. The eventual single-port dispatcher must
//! reserve this control budget independently of established data operations.
use super::{engine::Engine, http};
use bytes::Bytes;
use egress::control::{Route, MAX_REQUEST_BYTES};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::Semaphore,
};

const HEAD_TIMEOUT: Duration = Duration::from_secs(2);
const BODY_TIMEOUT: Duration = Duration::from_secs(2);
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(14);
const CONTROL_SLOTS: usize = 16;

/// Shared by all control connections on the selected native endpoint. Never
/// recreate per request, or borrow the proxy's occupied data-connection pool.
pub struct Transport {
    engine: Arc<Engine>,
    slots: Arc<Semaphore>,
}
impl Transport {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            slots: Arc::new(Semaphore::new(CONTROL_SLOTS)),
        }
    }
    /// Owns a previously accepted control socket; no DNS, listener bind or
    /// fallback to legacy grants. Drops on capacity/framing/time failure.
    /// Cancellation drops the engine future without removing its replay fence.
    pub async fn connection(&self, mut stream: TcpStream) {
        let Ok(_permit) = self.slots.clone().try_acquire_owned() else {
            return;
        };
        let engine = self.engine.clone();
        let run = async move {
            let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
                Ok(Ok(h)) => h,
                _ => return,
            };
            if validate_head(&head).is_err() {
                return;
            }
            let io = PrefixIo {
                prefix: std::io::Cursor::new(head),
                stream,
            };
            let service = hyper::service::service_fn(move |req| {
                let engine = engine.clone();
                async move { Ok::<_, Infallible>(handle(engine, req).await) }
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(HEAD_TIMEOUT)
                .max_buf_size(http::MAX_HEADER_BYTES)
                .max_headers(http::MAX_HEADERS)
                .keep_alive(false);
            let _ = builder.serve_connection(TokioIo::new(io), service).await;
        };
        let _ = tokio::time::timeout(CONNECTION_TIMEOUT, run).await;
    }
}
async fn read_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    while head.len() < http::MAX_HEADER_BYTES {
        head.push(stream.read_u8().await?);
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
    }
    Err(io::ErrorKind::InvalidData.into())
}
/// Preserve raw duplicates: Hyper may normalize identical Content-Length
/// fields. Reject those, obs-fold, transfer coding, trailers and upgrades BEFORE
/// handing the stream to Hyper. This parser is only for the closed control API.
fn validate_head(head: &[u8]) -> Result<(), ()> {
    let text = std::str::from_utf8(head).map_err(|_| ())?;
    let mut lines = text.strip_suffix("\r\n\r\n").ok_or(())?.split("\r\n");
    let line = lines.next().ok_or(())?;
    let mut words = line.split(' ');
    if words.next() != Some("POST") {
        return Err(());
    }
    let route = words.next().ok_or(())?;
    Route::parse(route).map_err(|_| ())?;
    if words.next() != Some("HTTP/1.1") || words.next().is_some() {
        return Err(());
    }
    let mut headers = hyper::HeaderMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(())?;
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| ())?;
        if headers.contains_key(&name) || headers.len() >= http::MAX_HEADERS {
            return Err(());
        }
        if !matches!(
            name.as_str(),
            "host" | "content-type" | "content-length" | "connection" | "user-agent" | "accept"
        ) && ![
            http::ID_HEADER,
            http::DEADLINE_HEADER,
            http::BOOT_HEADER,
            http::FINGERPRINT_HEADER,
            http::MAC_HEADER,
        ]
        .contains(&name.as_str())
        {
            return Err(());
        }
        if value.bytes().any(|b| !(32..=126).contains(&b)) {
            return Err(());
        }
        headers.insert(
            name,
            hyper::header::HeaderValue::from_str(value.trim_matches(' ')).map_err(|_| ())?,
        );
    }
    let len = headers
        .get("content-length")
        .ok_or(())?
        .to_str()
        .map_err(|_| ())?;
    let n: usize = len.parse().map_err(|_| ())?;
    if n > MAX_REQUEST_BYTES || len != n.to_string() {
        return Err(());
    }
    if headers.get("content-type").and_then(|v| v.to_str().ok()) != Some("application/json") {
        return Err(());
    }
    if headers.get("host").is_none() {
        return Err(());
    }
    Ok(())
}
async fn handle(engine: Arc<Engine>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let (parts, mut body) = req.into_parts();
    let read = async {
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| ())?;
            let data = frame.into_data().map_err(|_| ())?; // trailers forbidden
            if data.len() > MAX_REQUEST_BYTES.saturating_sub(bytes.len()) {
                return Err(());
            }
            bytes.extend_from_slice(&data);
        }
        Ok::<_, ()>(bytes)
    };
    let bytes = match tokio::time::timeout(BODY_TIMEOUT, read).await {
        Ok(Ok(b)) => b,
        _ => return refusal(400),
    };
    match tokio::time::timeout(
        EXECUTE_TIMEOUT,
        engine.execute(Request::from_parts(parts, bytes)),
    )
    .await
    {
        Ok(Ok(response)) => response.map(|b| Full::new(Bytes::from(b))),
        // This unsigned response conveys no successful authority or result.
        _ => refusal(503),
    }
}
fn refusal(status: u16) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    if let Ok(s) = hyper::StatusCode::from_u16(status) {
        *r.status_mut() = s;
    }
    r.headers_mut().insert(
        "connection",
        hyper::header::HeaderValue::from_static("close"),
    );
    r.headers_mut().insert(
        "cache-control",
        hyper::header::HeaderValue::from_static("no-store"),
    );
    r
}
struct PrefixIo {
    prefix: std::io::Cursor<Vec<u8>>,
    stream: TcpStream,
}
impl AsyncRead for PrefixIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let pos = usize::try_from(this.prefix.position()).unwrap_or(usize::MAX);
        if let Some(rest) = this.prefix.get_ref().get(pos..).filter(|r| !r.is_empty()) {
            let n = rest.len().min(buf.remaining());
            buf.put_slice(rest.get(..n).ok_or(io::ErrorKind::InvalidData)?);
            this.prefix
                .set_position(u64::try_from(pos + n).map_err(|_| io::ErrorKind::InvalidData)?);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for PrefixIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egress::control::{derive_control_key, FileKey, RequestMetadata};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    type TestResult = Result<(), Box<dyn std::error::Error>>;
    struct Key(Arc<AtomicUsize>);
    impl super::super::engine::KeyProvider for Key {
        fn current(&self) -> Result<FileKey, super::super::engine::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(FileKey::from_bytes([71; 32]))
        }
    }
    fn fixture() -> Result<(Arc<Transport>, Arc<AtomicUsize>), super::super::engine::Error> {
        let reads = Arc::new(AtomicUsize::new(0));
        let engine = Engine::new(Arc::new(Key(reads.clone())))?;
        Ok((Arc::new(Transport::new(Arc::new(engine))), reads))
    }
    fn signed(id: u64) -> Result<(Vec<u8>, String), Box<dyn std::error::Error>> {
        let file = FileKey::from_bytes([71; 32]);
        let fp = file.fingerprint();
        let id = format!("{id:032x}");
        let body = json!({"version":1,"mode":"initial","challenge":id,"expectedKeyFingerprint":fp,"priorSession":null}).to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let deadline = u64::try_from(now)? + 30_000;
        let mac = derive_control_key(&file)?.sign_request(
            Route::Ready,
            RequestMetadata {
                request_id: &id,
                deadline_ms: deadline,
                boot_id: None,
                key_fingerprint: &fp,
            },
            body.as_bytes(),
        )?;
        Ok((format!("POST {} HTTP/1.1\r\nHost: egress-proxy:3001\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-noevia-request-id: {id}\r\nx-noevia-deadline-ms: {deadline}\r\nx-noevia-key-fingerprint: {fp}\r\nx-noevia-control-mac: {mac}\r\nConnection: close\r\n\r\n{body}",Route::Ready.path(),body.len()).into_bytes(), mac))
    }
    async fn connected(
        t: Arc<Transport>,
    ) -> Result<(TcpStream, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let client = TcpStream::connect(l.local_addr()?).await?;
        let (server, _) = l.accept().await?;
        Ok((
            client,
            tokio::spawn(async move { t.connection(server).await }),
        ))
    }
    async fn exchange(
        t: Arc<Transport>,
        req: &[u8],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let (mut c, j) = connected(t).await?;
        c.write_all(req).await?;
        let mut out = Vec::new();
        match tokio::time::timeout(Duration::from_secs(4), c.read_to_end(&mut out)).await? {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => return Err(e.into()),
        }
        j.await?;
        Ok(out)
    }
    fn split(raw: &[u8]) -> Result<(&str, &[u8]), Box<dyn std::error::Error>> {
        let n = raw
            .windows(4)
            .position(|x| x == b"\r\n\r\n")
            .ok_or("head")?;
        Ok((
            std::str::from_utf8(raw.get(..n).ok_or("head slice")?)?,
            raw.get(n + 4..).ok_or("body slice")?,
        ))
    }
    #[tokio::test]
    async fn raw_ready_is_authenticated_and_exact_result_replayed_over_hyper() -> TestResult {
        let (t, _) = fixture()?;
        let (raw, mac) = signed(1)?;
        let first = exchange(t.clone(), &raw).await?;
        let second = exchange(t, &raw).await?;
        let (h, b) = split(&first)?;
        assert!(h.starts_with("HTTP/1.1 200"));
        let v: serde_json::Value = serde_json::from_slice(b)?;
        assert_eq!(v.get("state").and_then(|x| x.as_str()), Some("ready"));
        let tag = h
            .lines()
            .find_map(|l| l.strip_prefix("x-noevia-control-mac: "))
            .ok_or("response mac")?;
        let file = FileKey::from_bytes([71; 32]);
        let expected = derive_control_key(&file)?.sign_response(&mac, 200, b)?;
        assert_eq!(tag, expected);
        let (h2, b2) = split(&second)?;
        assert!(h2.starts_with("HTTP/1.1 200"));
        assert_eq!(b, b2);
        assert!(h.contains("connection: close"));
        Ok(())
    }
    #[tokio::test]
    async fn raw_duplicates_transfer_folding_and_size_refuse_before_key_read() -> TestResult {
        let (t, reads) = fixture()?;
        let before = reads.load(Ordering::SeqCst);
        let base = format!(
            "POST {} HTTP/1.1\r\nHost: e\r\nContent-Type: application/json\r\n",
            Route::Ready.path()
        );
        for tail in [
            "Content-Length: 0\r\nContent-Length: 0\r\n",
            "Content-Length: 0\r\nTransfer-Encoding: chunked\r\n",
            "Content-Length: 32769\r\n",
            "Content-Length: 00\r\n",
            "Content-Length: 0\r\nExpect: 100-continue\r\n",
            "Content-Length: 0\r\n host: fold\r\n",
            "Content-Length: 0\r\nhOst: duplicate\r\n",
        ] {
            let raw = format!("{base}{tail}\r\n");
            let out = exchange(t.clone(), raw.as_bytes()).await?;
            assert!(out.is_empty(), "unexpected successful framing");
        }
        let (raw, _) = signed(19)?;
        let text = String::from_utf8(raw)?;
        let len = text
            .lines()
            .find(|l| l.starts_with("Content-Length: "))
            .ok_or("content length")?;
        let duplicate = text.replacen(
            "Host: egress-proxy:3001",
            &format!("{len}\r\nHost: egress-proxy:3001"),
            1,
        );
        assert!(exchange(t.clone(), duplicate.as_bytes()).await?.is_empty());
        assert_eq!(reads.load(Ordering::SeqCst), before);
        Ok(())
    }
    #[tokio::test]
    async fn no_keepalive_or_pipelined_second_mutation() -> TestResult {
        let (t, _) = fixture()?;
        let (mut a, _) = signed(2)?;
        let (b, _) = signed(3)?;
        a.extend(b);
        let out = exchange(t, &a).await?;
        assert_eq!(
            String::from_utf8_lossy(&out).matches("HTTP/1.1 ").count(),
            1
        );
        assert!(out.starts_with(b"HTTP/1.1 200"));
        Ok(())
    }
    #[tokio::test]
    async fn body_deadline_and_slot_release_are_bounded() -> TestResult {
        let (t, reads) = fixture()?;
        let (raw, _) = signed(4)?;
        let end = raw
            .windows(4)
            .position(|b| b == b"\r\n\r\n")
            .ok_or("header")?
            + 4;
        let (mut c, j) = connected(t.clone()).await?;
        c.write_all(raw.get(..end).ok_or("header slice")?).await?;
        let before = reads.load(Ordering::SeqCst);
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(4), c.read_to_end(&mut out)).await??;
        j.await?;
        assert!(out.starts_with(b"HTTP/1.1 400"));
        assert_eq!(reads.load(Ordering::SeqCst), before);
        assert_eq!(t.slots.available_permits(), CONTROL_SLOTS);
        Ok(())
    }
    #[tokio::test]
    async fn control_capacity_is_independent_and_released_on_abort() -> TestResult {
        let (t, _) = fixture()?;
        let mut peers = Vec::new();
        let mut tasks = Vec::new();
        for _ in 0..CONTROL_SLOTS {
            let (mut c, j) = connected(t.clone()).await?;
            c.write_all(b"POST ").await?; // actual slow-head clients occupy control slots
            peers.push(c);
            tasks.push(j);
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while t.slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let (mut c, j) = connected(t.clone()).await?;
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), c.read_to_end(&mut out)).await??;
        j.await?;
        assert!(out.is_empty());
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        drop(peers);
        assert_eq!(t.slots.available_permits(), CONTROL_SLOTS);
        let (raw, _) = signed(5)?;
        assert!(exchange(t, &raw).await?.starts_with(b"HTTP/1.1 200"));
        Ok(())
    }
    #[tokio::test]
    async fn slow_head_deadline_and_oversize_never_reach_controller() -> TestResult {
        let (t, reads) = fixture()?;
        let before = reads.load(Ordering::SeqCst);
        let (mut c, j) = connected(t.clone()).await?;
        c.write_all(b"POST ").await?;
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(4), c.read_to_end(&mut out)).await??;
        j.await?;
        assert!(out.is_empty());
        let oversized = vec![b'x'; http::MAX_HEADER_BYTES];
        let (mut c, j) = connected(t.clone()).await?;
        c.write_all(&oversized).await?;
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), c.read_to_end(&mut out)).await??;
        j.await?;
        assert!(out.is_empty());
        assert_eq!(reads.load(Ordering::SeqCst), before);
        assert_eq!(t.slots.available_permits(), CONTROL_SLOTS);
        Ok(())
    }
    #[tokio::test]
    async fn tampered_mac_never_executes_or_reserves_ready() -> TestResult {
        let (t, _) = fixture()?;
        let (raw, _) = signed(20)?;
        let mut bad = raw.clone();
        let marker = b"x-noevia-control-mac: ";
        let pos = bad
            .windows(marker.len())
            .position(|v| v == marker)
            .ok_or("MAC header")?
            + marker.len();
        let byte = bad.get_mut(pos).ok_or("MAC byte")?;
        *byte = if *byte == b'0' { b'1' } else { b'0' };
        let out = exchange(t.clone(), &bad).await?;
        assert!(out.starts_with(b"HTTP/1.1 503"));
        let (h, _) = split(&out)?;
        assert!(!h.contains("x-noevia-control-mac:"));
        assert!(exchange(t, &raw).await?.starts_with(b"HTTP/1.1 200"));
        Ok(())
    }
}
