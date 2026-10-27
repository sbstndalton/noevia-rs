//! Native controller and data authority on one accepted endpoint. Not selected
//! by CLI/defaults until full paired source/assembly qualification is complete.
use crate::{
    control::{
        engine::{Engine, OperationGuard},
        transport::{self, PrefixIo, Transport},
    },
    *,
};
use std::task::{Context, Poll};
use tokio::task::{JoinHandle, JoinSet};

const INGRESS: usize = 16;
const HEAD_TIME: Duration = Duration::from_secs(2);
type NativeBody = http_body_util::combinators::BoxBody<Bytes, io::Error>;
type LeaseCell = Arc<Mutex<Option<Arc<OperationGuard>>>>;
struct Child {
    handle: JoinHandle<()>,
    tunnel: bool,
}
impl Drop for Child {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
type ChildCell = Arc<Mutex<Option<Child>>>;

impl Proxy {
    /// Native-only service; even explicitly configured legacy grants/keys refuse
    /// mixed-mode startup. The passed listener is owned by this future.
    pub async fn serve_native(
        self: Arc<Self>,
        listener: TcpListener,
        engine: Arc<Engine>,
    ) -> io::Result<()> {
        if self.key_source.is_some()
            || !self.lock().store.is_empty()
            || self.limits.max_connections == 0
            || self.limits.max_connections > 256
            || self.limits.max_connections_per_task != 64
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "native service requires isolated authority and supported limits",
            ));
        }
        let control = Arc::new(Transport::new(engine.clone()));
        let ingress = Arc::new(Semaphore::new(INGRESS));
        let data = Arc::new(Semaphore::new(self.limits.max_connections.max(1)));
        let mut tasks = JoinSet::new();
        let mut maintenance = tokio::time::interval(Duration::from_millis(250));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _=maintenance.tick()=>{let _=engine.maintenance();}
                Some(_)=tasks.join_next(),if !tasks.is_empty()=>{}
                accepted=listener.accept()=>{
                    let (mut stream,_)=accepted?;
                    if tasks.len()>=self.limits.max_connections.max(1).saturating_add(2*INGRESS){continue;}
                    let Ok(pending)=ingress.clone().try_acquire_owned() else {continue;};
                    let proxy=self.clone();let control=control.clone();let engine=engine.clone();let data=data.clone();
                    tasks.spawn(async move {
                        let head=match tokio::time::timeout(HEAD_TIME,transport::read_head(&mut stream)).await {Ok(Ok(h))=>h,_=>return};
                        let is_control=std::str::from_utf8(&head).ok().and_then(|s|s.lines().next())
                            .and_then(|s|s.split(' ').nth(1)).is_some_and(|p|p.starts_with("/v1/control/"));
                        if is_control {drop(pending);control.connection_head(stream,head).await;}
                        else {
                            let Ok(slot)=data.try_acquire_owned() else {return;};
                            drop(pending);
                            if !data_head(&head){return;}
                            native_connection(proxy,engine,stream,head,Arc::new(slot)).await;
                        }
                    });
                }
            }
        }
    }
}
// Preserve critical duplicate framing before Hyper normalization for data too.
// Transfer coding is permitted only without Content-Length (normal HTTP upload).
fn data_head(head: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    let Some(text) = text.strip_suffix("\r\n\r\n") else {
        return false;
    };
    let mut lines = text.split("\r\n");
    if lines.next().is_none() {
        return false;
    }
    let mut h = HeaderMap::new();
    for (n, line) in lines.enumerate() {
        if n >= 100 {
            return false;
        }
        let Some((k, v)) = line.split_once(':') else {
            return false;
        };
        let Ok(k) = HeaderName::from_bytes(k.as_bytes()) else {
            return false;
        };
        let Ok(v) = HeaderValue::from_str(v.trim_matches([' ', '\t'])) else {
            return false;
        };
        if h.contains_key(&k)
            && matches!(
                k.as_str(),
                "host" | "content-length" | "transfer-encoding" | "proxy-authorization"
            )
        {
            return false;
        }
        h.append(k, v);
    }
    !(h.contains_key(hyper::header::TRANSFER_ENCODING)
        && h.contains_key(hyper::header::CONTENT_LENGTH))
}
async fn cancelled(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    let _ = rx.wait_for(|v| *v).await;
}
async fn native_connection(
    proxy: Arc<Proxy>,
    engine: Arc<Engine>,
    stream: TcpStream,
    head: Vec<u8>,
    slot: Slot,
) {
    let lease: LeaseCell = Arc::new(Mutex::new(None));
    let children: ChildCell = Arc::new(Mutex::new(None));
    let (signal, mut changed) = watch::channel(None::<watch::Receiver<bool>>);
    let service = hyper::service::service_fn({
        let lease = lease.clone();
        let children = children.clone();
        let proxy = proxy.clone();
        move |req| {
            let lease = lease.clone();
            let children = children.clone();
            let proxy = proxy.clone();
            let engine = engine.clone();
            let signal = signal.clone();
            let slot = slot.clone();
            async move {
                Ok::<_, Infallible>(
                    native_handle(proxy, engine, req, slot, lease, children, signal).await,
                )
            }
        }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(HEAD_TIME)
        .max_buf_size(proxy.limits.max_header_bytes.max(8192))
        .max_headers(proxy.limits.max_headers)
        .keep_alive(false);
    let conn = builder
        .serve_connection(TokioIo::new(PrefixIo::new(head, stream)), service)
        .with_upgrades();
    let stop = async {
        loop {
            let next = changed.borrow().clone();
            if let Some(rx) = next {
                cancelled(rx).await;
                return;
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    };
    let normal = tokio::select! {_=conn=>true,_=stop=>false};
    // Take out before await. Owned child aborts on outer cancellation; scope
    // ownership persists inside that child until its actual I/O future drops.
    let child = children.lock().ok().and_then(|mut c| c.take());
    if let Some(mut child) = child {
        if !normal || !child.tunnel {
            child.handle.abort();
        }
        let _ = (&mut child.handle).await;
    }
    drop(lease);
}
async fn decide(
    proxy: &Arc<Proxy>,
    engine: &Engine,
    authorization: Option<&[u8]>,
    target: Option<&str>,
    port: u16,
    cell: &LeaseCell,
    signal: &watch::Sender<Option<watch::Receiver<bool>>>,
) -> Result<(Arc<OperationGuard>, Allowed), Refusal> {
    let guard = Arc::new(engine.request(authorization, target, port)?);
    *cell.lock().map_err(|_| native_refusal(503))? = Some(guard.clone());
    signal.send_replace(Some(guard.subscribe()));
    let pending = guard.pending().ok_or_else(|| native_refusal(503))?;
    let addresses = if pending.literal {
        Vec::new()
    } else {
        let mut cancel = guard.subscribe();
        let resolution = tokio::time::timeout(
            proxy.limits.resolve_timeout,
            proxy.resolver.resolve_scoped(&pending.host, guard.clone()),
        );
        tokio::select! {
            result=resolution=>match result{Ok(Ok(a))=>a,_=>Vec::new()},
            _=cancel.wait_for(|v|*v)=>return Err(native_refusal(503)),
        }
    };
    let allowed = guard.resolved(&addresses)?;
    Ok((guard, allowed))
}
fn native_refusal(status: u16) -> Refusal {
    Refusal {
        status,
        reason: "task token was revoked".into(),
        task_id: None,
        host: None,
    }
}
async fn connect(
    proxy: &Proxy,
    guard: &OperationGuard,
    allowed: &Allowed,
) -> io::Result<TcpStream> {
    guard
        .recheck()
        .map_err(|_| io::ErrorKind::PermissionDenied)?;
    let cancel = guard.subscribe();
    let stream = tokio::select! {s=proxy.connect_upstream(allowed)=>s?,_=cancelled(cancel)=>return Err(io::ErrorKind::PermissionDenied.into())};
    guard
        .recheck()
        .map_err(|_| io::ErrorKind::PermissionDenied)?;
    Ok(stream)
}
async fn native_handle(
    proxy: Arc<Proxy>,
    engine: Arc<Engine>,
    mut req: Request<Incoming>,
    slot: Slot,
    lease: LeaseCell,
    children: ChildCell,
    signal: watch::Sender<Option<watch::Receiver<bool>>>,
) -> Response<NativeBody> {
    let is_connect = req.method() == Method::CONNECT;
    if !is_connect && req.uri().scheme_str() != Some("http") {
        return connect_refusal(400);
    }
    let target = req.uri().authority().map(|a| {
        if is_connect {
            a.as_str()
        } else {
            a.as_str().rsplit('@').next().unwrap_or("")
        }
        .to_owned()
    });
    if is_connect && target.as_ref().is_some_and(|t| t.contains('@')) {
        return connect_refusal(400);
    }
    let auth = req
        .headers()
        .get(hyper::header::PROXY_AUTHORIZATION)
        .map(|v| v.as_bytes().to_vec());
    let (guard, allowed) = match decide(
        &proxy,
        &engine,
        auth.as_deref(),
        target.as_deref(),
        if is_connect { 443 } else { 80 },
        &lease,
        &signal,
    )
    .await
    {
        Ok(v) => v,
        Err(r) => {
            return if is_connect {
                connect_refusal(r.status)
            } else {
                text_refusal(&r)
            }
        }
    };
    let upstream = match connect(&proxy, &guard, &allowed).await {
        Ok(s) => s,
        Err(_) => return upstream_failed(),
    };
    if is_connect {
        let upgrade = hyper::upgrade::on(&mut req);
        let cancel = guard.subscribe();
        let handle = tokio::spawn(async move {
            let _slot = slot;
            let upgraded = tokio::select! {v=upgrade=>match v{Ok(v)=>v,Err(_)=>return},_=cancelled(cancel)=>return};
            if guard.recheck().is_err() {
                return;
            }
            native_tunnel(&proxy, TokioIo::new(upgraded), upstream, guard).await;
        });
        let Ok(mut c) = children.lock() else {
            handle.abort();
            return upstream_failed();
        };
        *c = Some(Child {
            handle,
            tunnel: true,
        });
        let mut out = response(200, empty());
        out.extensions_mut()
            .insert(hyper::ext::ReasonPhrase::from_static(
                b"Connection Established",
            ));
        return out;
    }
    let uri = req.uri().clone();
    let (mut parts, body) = req.into_parts();
    let had_te = parts.headers.contains_key(hyper::header::TRANSFER_ENCODING);
    strip_hop_by_hop(&mut parts.headers);
    if had_te {
        parts.headers.remove(hyper::header::CONTENT_LENGTH);
    }
    let Some(host) = target
        .as_deref()
        .and_then(|h| HeaderValue::from_str(h).ok())
    else {
        return upstream_failed();
    };
    parts.headers.insert(hyper::header::HOST, host);
    parts
        .headers
        .insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    let Ok(path) = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .parse::<hyper::Uri>()
    else {
        return upstream_failed();
    };
    parts.uri = path;
    parts.version = hyper::Version::HTTP_11;
    let handshake = hyper::client::conn::http1::handshake::<_, GuardedBody>(TokioIo::new(upstream));
    let (mut sender, driver) = tokio::select! {r=handshake=>match r{Ok(v)=>v,Err(_)=>return upstream_failed()},_=cancelled(guard.subscribe())=>return upstream_failed()};
    if guard.recheck().is_err() {
        return upstream_failed();
    }
    let driver_guard = guard.clone();
    let driver_cancel = guard.subscribe();
    let handle = tokio::spawn(async move {
        let _slot = slot;
        let _guard = driver_guard;
        tokio::select! {_=driver=>{},_=cancelled(driver_cancel)=>{}}
    });
    match children.lock() {
        Ok(mut c) => {
            *c = Some(Child {
                handle,
                tunnel: false,
            })
        }
        Err(_) => {
            handle.abort();
            return upstream_failed();
        }
    }
    let upstream_request = Request::from_parts(
        parts,
        GuardedBody {
            body,
            guard: guard.clone(),
        },
    );
    let reply = tokio::select! {r=sender.send_request(upstream_request)=>r,_=cancelled(guard.subscribe())=>return upstream_failed()};
    match reply {
        Ok(upstream) => {
            if guard.touch_data().is_err() {
                return upstream_failed();
            }
            let (mut parts, body) = upstream.into_parts();
            let had_te = parts.headers.contains_key(hyper::header::TRANSFER_ENCODING);
            strip_hop_by_hop(&mut parts.headers);
            if had_te {
                parts.headers.remove(hyper::header::CONTENT_LENGTH);
            }
            parts
                .headers
                .insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
            Response::from_parts(
                parts,
                GuardedBody {
                    body,
                    guard: guard.clone(),
                }
                .boxed(),
            )
        }
        Err(_) => upstream_failed(),
    }
}
async fn native_tunnel<C: AsyncRead + AsyncWrite + Unpin>(
    proxy: &Proxy,
    client: C,
    upstream: TcpStream,
    guard: Arc<OperationGuard>,
) {
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = upstream.into_split();
    let last = Mutex::new(tokio::time::Instant::now());
    let up = pump(&mut cr, &mut uw, &guard, &last);
    let down = pump(&mut ur, &mut cw, &guard, &last);
    let idle = async {
        loop {
            let deadline = match last.lock() {
                Ok(t) => *t + proxy.limits.tunnel_idle_timeout,
                Err(_) => return,
            };
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    };
    tokio::select! {_=async{tokio::try_join!(up,down)}=>{},_=cancelled(guard.subscribe())=>{},_=idle=>{}}
}
async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
    guard: &OperationGuard,
    last: &Mutex<tokio::time::Instant>,
) -> io::Result<()> {
    let mut bytes = [0u8; 16 * 1024];
    loop {
        let n = r.read(&mut bytes).await?;
        guard
            .recheck()
            .map_err(|_| io::ErrorKind::PermissionDenied)?;
        if n == 0 {
            return w.shutdown().await;
        }
        guard
            .touch_data()
            .map_err(|_| io::ErrorKind::PermissionDenied)?;
        *last
            .lock()
            .map_err(|_| io::Error::other("stream state unavailable"))? =
            tokio::time::Instant::now();
        w.write_all(bytes.get(..n).ok_or(io::ErrorKind::InvalidData)?)
            .await?;
        guard
            .recheck()
            .map_err(|_| io::ErrorKind::PermissionDenied)?;
    }
}

struct GuardedBody {
    body: Incoming,
    guard: Arc<OperationGuard>,
}
impl hyper::body::Body for GuardedBody {
    type Data = Bytes;
    type Error = io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Bytes>, io::Error>>> {
        if self.guard.recheck().is_err() {
            return Poll::Ready(Some(Err(io::ErrorKind::PermissionDenied.into())));
        }
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(mut frame))) => {
                if let Some(trailers) = frame.trailers_mut() {
                    strip_hop_by_hop(trailers);
                }
                if frame.data_ref().is_some_and(|b| !b.is_empty())
                    && self.guard.touch_data().is_err()
                {
                    return Poll::Ready(Some(Err(io::ErrorKind::PermissionDenied.into())));
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(_))) => {
                Poll::Ready(Some(Err(io::Error::other("upstream body unavailable"))))
            }
            other => other
                .map(|v| v.map(|r| r.map_err(|_| io::Error::other("upstream body unavailable")))),
        }
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}
fn adapt(r: Response<Body>) -> Response<NativeBody> {
    r.map(|b| {
        b.map_err(|_| io::Error::other("proxy body unavailable"))
            .boxed()
    })
}
fn connect_refusal(status: u16) -> Response<NativeBody> {
    adapt(crate::connect_refusal(status))
}
fn text_refusal(r: &Refusal) -> Response<NativeBody> {
    adapt(crate::text_refusal(r))
}
fn upstream_failed() -> Response<NativeBody> {
    adapt(crate::upstream_failed())
}
fn response(status: u16, body: Body) -> Response<NativeBody> {
    adapt(crate::response(status, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egress::control::{derive_control_key, derive_epoch_keys, FileKey, RequestMetadata, Route};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    type TestError = Box<dyn std::error::Error>;
    struct Keys(Mutex<Option<FileKey>>);
    impl crate::control::engine::KeyProvider for Keys {
        fn current(&self) -> Result<FileKey, crate::control::engine::Error> {
            self.0
                .lock()
                .map_err(|_| crate::control::engine::Error::Key)?
                .clone()
                .ok_or(crate::control::engine::Error::Key)
        }
    }
    struct Running {
        addr: SocketAddr,
        job: JoinHandle<io::Result<()>>,
        file: FileKey,
        keys: Arc<Keys>,
        _retained_engine: Arc<Engine>,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.job.abort();
        }
    }
    struct Fixed;
    impl Resolver for Fixed {
        fn resolve<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            Box::pin(async { Ok(vec![IpAddr::from([93, 184, 216, 34])]) })
        }
    }
    struct Bridge {
        addr: SocketAddr,
        calls: Arc<AtomicUsize>,
    }
    impl Connector for Bridge {
        fn connect(&self, addr: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let checked = addr.ip() == IpAddr::from([93, 184, 216, 34]);
            let local = self.addr;
            Box::pin(async move {
                if !checked {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                TcpStream::connect(local).await
            })
        }
    }
    async fn start(
        resolver: Arc<dyn Resolver>,
        connector: Arc<dyn Connector>,
        cap: usize,
    ) -> Result<Running, TestError> {
        let file = FileKey::from_bytes([37; 32]);
        let keys = Arc::new(Keys(Mutex::new(Some(file.clone()))));
        let engine = Arc::new(Engine::new(keys.clone())?);
        let proxy = ProxyBuilder::new(vec![])
            .resolver(resolver)
            .connector(connector)
            .limits(Limits {
                max_connections: cap,
                ..Limits::default()
            })
            .log(Arc::new(|_| {}))
            .build();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let job = tokio::spawn(proxy.serve_native(listener, engine.clone()));
        Ok(Running {
            addr,
            job,
            file,
            keys,
            _retained_engine: engine,
        })
    }
    fn wall() -> Result<u64, TestError> {
        Ok(u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        )?)
    }
    fn command(
        s: &Running,
        route: Route,
        body: Value,
        id: u64,
        boot: Option<&str>,
    ) -> Result<Vec<u8>, TestError> {
        let body = serde_json::to_vec(&body)?;
        let id = format!("{id:032x}");
        let fp = s.file.fingerprint();
        let deadline = wall()? + 30_000;
        let mac = derive_control_key(&s.file)?.sign_request(
            route,
            RequestMetadata {
                request_id: &id,
                deadline_ms: deadline,
                boot_id: boot,
                key_fingerprint: &fp,
            },
            &body,
        )?;
        let mut raw=format!("POST {} HTTP/1.1\r\nHost: egress:3001\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-noevia-request-id: {id}\r\nx-noevia-deadline-ms: {deadline}\r\nx-noevia-key-fingerprint: {fp}\r\nx-noevia-control-mac: {mac}\r\n",route.path(),body.len());
        if let Some(boot) = boot {
            raw.push_str(&format!("x-noevia-boot-id: {boot}\r\n"));
        }
        raw.push_str("Connection: close\r\n\r\n");
        let mut raw = raw.into_bytes();
        raw.extend(body);
        Ok(raw)
    }
    async fn exchange(addr: SocketAddr, raw: &[u8]) -> Result<Vec<u8>, TestError> {
        let mut c = TcpStream::connect(addr).await?;
        c.write_all(raw).await?;
        let mut out = Vec::new();
        match tokio::time::timeout(Duration::from_secs(3), c.read_to_end(&mut out)).await? {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => return Err(e.into()),
        };
        Ok(out)
    }
    fn body(raw: &[u8]) -> Result<Value, TestError> {
        let p = raw
            .windows(4)
            .position(|b| b == b"\r\n\r\n")
            .ok_or("response head")?;
        Ok(serde_json::from_slice(raw.get(p + 4..).ok_or("body")?)?)
    }
    fn hex16(s: &str) -> Result<[u8; 16], TestError> {
        let b = s.as_bytes().as_chunks::<2>();
        if !b.1.is_empty() || b.0.len() != 16 {
            return Err("identity".into());
        }
        let mut out = [0; 16];
        for (a, p) in out.iter_mut().zip(b.0) {
            *a = u8::from_str_radix(std::str::from_utf8(p)?, 16)?;
        }
        Ok(out)
    }
    struct GrantContext {
        boot: String,
        session: String,
        token: String,
        claims: SignedGrant,
    }
    async fn ready(s: &Running) -> Result<GrantContext, TestError> {
        let raw = command(
            s,
            Route::Ready,
            json!({"version":1,"mode":"initial","challenge":format!("{:032x}",1),"expectedKeyFingerprint":s.file.fingerprint(),"priorSession":null}),
            1,
            None,
        )?;
        let out = exchange(s.addr, &raw).await?;
        assert!(out.starts_with(b"HTTP/1.1 200"));
        let v = body(&out)?;
        let boot = v
            .get("bootId")
            .and_then(Value::as_str)
            .ok_or("boot")?
            .to_owned();
        let session = v
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("session")?
            .to_owned();
        let keys = derive_epoch_keys(&s.file, &hex16(&boot)?, &hex16(&session)?)?;
        let now = wall()?;
        let claims = SignedGrant {
            act: Act::Grant,
            task: "job.1".into(),
            hosts: vec!["example.com".into()],
            iat: now,
            exp: now + 60_000,
            idle: 60_000,
            nonce: egress::b64url_encode(&[1; 16]),
        };
        let token = egress::mint(&keys.grant, &claims).ok_or("mint")?;
        Ok(GrantContext {
            boot,
            session,
            token,
            claims,
        })
    }
    fn auth(c: &GrantContext) -> String {
        format!(
            "Basic {}",
            egress::b64url_encode(format!("task:{}", c.token).as_bytes())
        )
    }
    fn revoke(s: &Running, c: &GrantContext, id: u64) -> Result<Vec<u8>, TestError> {
        let mut g = c.claims.clone();
        g.act = Act::Revoke;
        g.hosts.clear();
        g.nonce = egress::b64url_encode(&[2; 16]);
        g.iat = wall()?;
        g.exp = g.iat + 60_000;
        let k = derive_epoch_keys(&s.file, &hex16(&c.boot)?, &hex16(&c.session)?)?;
        let token = egress::mint(&k.grant, &g).ok_or("revoke")?;
        command(
            s,
            Route::Revoke,
            json!({"version":1,"sessionId":c.session,"taskId":c.claims.task,"leaseNonce":c.claims.nonce,"grantIssuedAt":c.claims.iat,"signedGrant":c.token,"signedRevoke":token}),
            id,
            Some(&c.boot),
        )
    }
    async fn tunnel(s: &Running, c: &GrantContext) -> Result<TcpStream, TestError> {
        let mut stream = TcpStream::connect(s.addr).await?;
        stream.write_all(format!("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Authorization: {}\r\n\r\n",auth(c)).as_bytes()).await?;
        let head = tokio::time::timeout(Duration::from_secs(2), transport::read_head(&mut stream))
            .await??;
        assert!(head.starts_with(b"HTTP/1.1 200"));
        Ok(stream)
    }
    async fn echo() -> Result<(SocketAddr, Child, Arc<AtomicUsize>), TestError> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let handle = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {v=l.accept()=>{let Ok((s,_))=v else{return;};tasks.spawn(async move{let(mut r,mut w)=s.into_split();let _=tokio::io::copy(&mut r,&mut w).await;});},Some(_)=tasks.join_next(),if !tasks.is_empty()=>{}}
            }
        });
        Ok((
            addr,
            Child {
                handle,
                tunnel: false,
            },
            calls,
        ))
    }
    #[tokio::test]
    async fn saturated_native_data_still_accepts_control_and_drains_tunnel() -> Result<(), TestError>
    {
        let (addr, _echo, calls) = echo().await?;
        let s = start(
            Arc::new(Fixed),
            Arc::new(Bridge {
                addr,
                calls: calls.clone(),
            }),
            1,
        )
        .await?;
        let c = ready(&s).await?;
        let mut stream = tunnel(&s, &c).await?;
        stream.write_all(b"hello").await?;
        let mut echoed = [0; 5];
        stream.read_exact(&mut echoed).await?;
        assert_eq!(&echoed, b"hello");
        let extra=exchange(s.addr,format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        assert!(extra.is_empty());
        let out = exchange(s.addr, &revoke(&s, &c, 2)?).await?;
        assert!(out.starts_with(b"HTTP/1.1 200"));
        assert_eq!(body(&out)?.get("drained"), Some(&json!(true)));
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut tail)).await??;
        assert!(tail.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }
    #[tokio::test]
    async fn forward_stream_is_scoped_and_proxy_credentials_never_reach_upstream(
    ) -> Result<(), TestError> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let remote = Child {
            tunnel: false,
            handle: tokio::spawn({
                let seen = seen.clone();
                let closed = closed.clone();
                async move {
                    let Ok((mut socket, _)) = l.accept().await else {
                        return;
                    };
                    let Ok(head) = transport::read_head(&mut socket).await else {
                        return;
                    };
                    if let Ok(mut s) = seen.lock() {
                        *s = head;
                    }
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\na")
                        .await;
                    let mut b = [0];
                    let _ = socket.read(&mut b).await;
                    closed.store(true, Ordering::SeqCst);
                }
            }),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let s = start(
            Arc::new(Fixed),
            Arc::new(Bridge {
                addr,
                calls: calls.clone(),
            }),
            2,
        )
        .await?;
        let c = ready(&s).await?;
        let mut client = TcpStream::connect(s.addr).await?;
        client.write_all(format!("GET http://example.com/private/path HTTP/1.1\r\nHost: wrong\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        let head = transport::read_head(&mut client).await?;
        assert!(head.starts_with(b"HTTP/1.1 200"));
        let mut b = [0];
        client.read_exact(&mut b).await?;
        assert_eq!(b, [b'a']);
        let out = exchange(s.addr, &revoke(&s, &c, 3)?).await?;
        assert_eq!(body(&out)?.get("drained"), Some(&json!(true)));
        let mut tail = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut tail)).await?;
        assert!(tail.is_empty());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !closed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await
            }
        })
        .await?;
        let wire = String::from_utf8(seen.lock().map_err(|_| "seen")?.clone())?;
        assert!(wire.starts_with("GET /private/path HTTP/1.1"));
        assert!(!wire.to_lowercase().contains("proxy-authorization"));
        assert!(!wire.contains("ngr1."));
        assert!(wire.to_lowercase().contains("host: example.com"));
        drop(remote);
        Ok(())
    }
    #[tokio::test]
    async fn maintenance_key_failure_closes_quiet_native_tunnel_without_cached_fallback(
    ) -> Result<(), TestError> {
        let (addr, _echo, calls) = echo().await?;
        let s = start(Arc::new(Fixed), Arc::new(Bridge { addr, calls }), 2).await?;
        let c = ready(&s).await?;
        let mut stream = tunnel(&s, &c).await?;
        *s.keys.0.lock().map_err(|_| "keys")? = None;
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut out)).await??;
        assert!(out.is_empty());
        let out=exchange(s.addr,format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        assert!(!out.starts_with(b"HTTP/1.1 200"));
        Ok(())
    }
    #[tokio::test]
    async fn quiet_native_tunnel_expires_at_session_deadline_without_web_renewal(
    ) -> Result<(), TestError> {
        let (addr, _echo, calls) = echo().await?;
        let s = start(Arc::new(Fixed), Arc::new(Bridge { addr, calls }), 2).await?;
        let c = ready(&s).await?;
        let mut stream = tunnel(&s, &c).await?;
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(17), stream.read_to_end(&mut out)).await??;
        assert!(out.is_empty());
        Ok(())
    }
    struct MarkDrop(Arc<AtomicBool>);
    impl Drop for MarkDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct BlockingConnect {
        started: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }
    impl Connector for BlockingConnect {
        fn connect(&self, _: SocketAddr) -> BoxFuture<'_, io::Result<TcpStream>> {
            let started = self.started.clone();
            let dropped = self.dropped.clone();
            Box::pin(async move {
                let _drop = MarkDrop(dropped);
                started.store(true, Ordering::SeqCst);
                std::future::pending().await
            })
        }
    }
    async fn until(flag: &AtomicBool) -> Result<(), TestError> {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::task::yield_now().await
            }
        })
        .await?;
        Ok(())
    }
    #[tokio::test]
    async fn scoped_revoke_cancels_connect_before_proving_actual_drain() -> Result<(), TestError> {
        let started = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let s = start(
            Arc::new(Fixed),
            Arc::new(BlockingConnect {
                started: started.clone(),
                dropped: dropped.clone(),
            }),
            2,
        )
        .await?;
        let c = ready(&s).await?;
        let mut client = TcpStream::connect(s.addr).await?;
        client.write_all(format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        until(&started).await?;
        let out = exchange(s.addr, &revoke(&s, &c, 8)?).await?;
        assert_eq!(body(&out)?.get("drained"), Some(&json!(true)));
        assert!(dropped.load(Ordering::SeqCst));
        Ok(())
    }
    struct Gate {
        open: Mutex<bool>,
        wake: std::sync::Condvar,
        started: AtomicBool,
        done: AtomicBool,
    }
    struct Release(Arc<Gate>);
    impl Release {
        fn release(&self) {
            if let Ok(mut open) = self.0.open.lock() {
                *open = true;
                self.0.wake.notify_all();
            }
        }
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.release();
        }
    }
    struct BlockingDns(Arc<Gate>);
    impl Resolver for BlockingDns {
        fn resolve<'a>(&'a self, _: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            Box::pin(async { Err(io::ErrorKind::Unsupported.into()) })
        }
        fn resolve_scoped<'a>(
            &'a self,
            _: &'a str,
            guard: Arc<OperationGuard>,
        ) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            let gate = self.0.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    let _guard = guard;
                    gate.started.store(true, Ordering::SeqCst);
                    let mut open = gate.open.lock().map_err(|_| io::Error::other("gate"))?;
                    while !*open {
                        open = gate.wake.wait(open).map_err(|_| io::Error::other("gate"))?;
                    }
                    gate.done.store(true, Ordering::SeqCst);
                    Ok(vec![IpAddr::from([93, 184, 216, 34])])
                })
                .await
                .map_err(|_| io::Error::other("resolver job"))?
            })
        }
    }
    #[tokio::test]
    async fn revoke_does_not_claim_dns_drain_until_actual_blocking_lookup_finishes(
    ) -> Result<(), TestError> {
        let gate = Arc::new(Gate {
            open: Mutex::new(false),
            wake: std::sync::Condvar::new(),
            started: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let release = Release(gate.clone());
        let (addr, _echo, calls) = echo().await?;
        let s = start(
            Arc::new(BlockingDns(gate.clone())),
            Arc::new(Bridge {
                addr,
                calls: calls.clone(),
            }),
            2,
        )
        .await?;
        let c = ready(&s).await?;
        let mut client = TcpStream::connect(s.addr).await?;
        client.write_all(format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        until(&gate.started).await?;
        let address = s.addr;
        let take = revoke(&s, &c, 9)?;
        let task =
            tokio::spawn(async move { exchange(address, &take).await.map_err(|e| e.to_string()) });
        let mut tail = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut tail)).await?;
        assert!(!task.is_finished());
        assert!(!gate.done.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        release.release();
        let out = tokio::time::timeout(Duration::from_secs(2), task).await???;
        assert_eq!(body(&out)?.get("drained"), Some(&json!(true)));
        assert!(gate.done.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }
    #[tokio::test]
    async fn native_listener_rejects_legacy_grants_and_key_fallback_configuration(
    ) -> Result<(), TestError> {
        let file = FileKey::from_bytes([37; 32]);
        let engine = Arc::new(Engine::new(Arc::new(Keys(Mutex::new(Some(file)))))?);
        let legacy = ProxyBuilder::new(vec![Grant {
            token: "synthetic-only".into(),
            task_id: "job.1".into(),
            domains: vec!["example.com".into()],
            idle_ttl_ms: 1000,
        }])
        .build();
        assert!(legacy
            .serve_native(TcpListener::bind("127.0.0.1:0").await?, engine.clone())
            .await
            .is_err());
        let key = GrantKey::from_hex(&"07".repeat(32))?;
        let legacy = ProxyBuilder::new(vec![])
            .grant_key(KeySource::Static(key))
            .build();
        assert!(legacy
            .serve_native(TcpListener::bind("127.0.0.1:0").await?, engine)
            .await
            .is_err());
        Ok(())
    }
    #[tokio::test]
    async fn mixed_port_rejects_ambiguous_data_heads_and_old_control_routes(
    ) -> Result<(), TestError> {
        let (addr, _echo, calls) = echo().await?;
        let s = start(
            Arc::new(Fixed),
            Arc::new(Bridge {
                addr,
                calls: calls.clone(),
            }),
            2,
        )
        .await?;
        let c = ready(&s).await?;
        let raw=format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\nContent-Length: 0\r\nProxy-Authorization: {}\r\n\r\n",auth(&c));
        assert!(exchange(s.addr, raw.as_bytes()).await?.is_empty());
        let raw=format!("POST /v1/revoke HTTP/1.1\r\nHost: egress\r\nContent-Length: 0\r\nAuthorization: Bearer {}\r\n\r\n",c.token);
        assert!(exchange(s.addr, raw.as_bytes())
            .await?
            .starts_with(b"HTTP/1.1 400"));
        let raw=format!("CONNECT user@secret.example.com:443 HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",auth(&c));
        assert!(exchange(s.addr, raw.as_bytes())
            .await?
            .starts_with(b"HTTP/1.1 400"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }
    #[tokio::test]
    async fn cancelling_native_listener_closes_owned_upgraded_tunnel() -> Result<(), TestError> {
        let (addr, _echo, calls) = echo().await?;
        let s = start(Arc::new(Fixed), Arc::new(Bridge { addr, calls }), 2).await?;
        let c = ready(&s).await?;
        let mut stream = tunnel(&s, &c).await?;
        s.job.abort();
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut tail)).await??;
        assert!(tail.is_empty());
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn actual_owned_keyfile_permission_loss_retires_live_native_tunnel(
    ) -> Result<(), TestError> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        struct Temp(std::path::PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| "fixture entropy")?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir()
            .canonicalize()?
            .join(format!("noevia-native-key-test-{name}"));
        std::fs::create_dir(&dir)?;
        let owned = Temp(dir);
        std::fs::set_permissions(&owned.0, std::fs::Permissions::from_mode(0o700))?;
        let path = owned.0.join("control.key");
        std::fs::write(&path, format!("{}\n", "25".repeat(32)))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o440))?;
        let meta = std::fs::metadata(&path)?;
        let provider = Arc::new(crate::control::keyfile::KeyFile::new(
            &path,
            meta.uid(),
            meta.gid(),
        )?);
        let engine = Arc::new(Engine::new(provider)?);
        let file = FileKey::from_bytes([37; 32]);
        let (addr, _echo, calls) = echo().await?;
        let proxy = ProxyBuilder::new(vec![])
            .resolver(Arc::new(Fixed))
            .connector(Arc::new(Bridge { addr, calls }))
            .log(Arc::new(|_| {}))
            .build();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let keys = Arc::new(Keys(Mutex::new(Some(file.clone()))));
        let job = tokio::spawn(proxy.serve_native(listener, engine.clone()));
        let s = Running {
            addr,
            job,
            file,
            keys,
            _retained_engine: engine,
        };
        let c = ready(&s).await?;
        let mut stream = tunnel(&s, &c).await?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut tail)).await??;
        assert!(tail.is_empty());
        let out=exchange(s.addr,format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        assert!(!out.starts_with(b"HTTP/1.1 200"));
        Ok(())
    }
    #[tokio::test]
    async fn chunked_upload_refreshes_native_idle_and_does_not_forward_proxy_trailers(
    ) -> Result<(), TestError> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?;
        let seen = Arc::new(Mutex::new((Vec::new(), false)));
        let backend_seen = seen.clone();
        let backend = Child {
            tunnel: false,
            handle: tokio::spawn(async move {
                let Ok((socket, _)) = l.accept().await else {
                    return;
                };
                let service = hyper::service::service_fn(move |r: Request<Incoming>| {
                    let seen = backend_seen.clone();
                    async move {
                        let proxy_header =
                            r.headers().contains_key(hyper::header::PROXY_AUTHORIZATION);
                        let collected = r.into_body().collect().await;
                        if let Ok(b) = collected {
                            let leaked = proxy_header
                                || b.trailers().is_some_and(|h| {
                                    h.contains_key(hyper::header::PROXY_AUTHORIZATION)
                                });
                            if let Ok(mut seen) = seen.lock() {
                                *seen = (b.to_bytes().to_vec(), leaked);
                            }
                        }
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                            b"uploaded",
                        ))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            }),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let s = start(Arc::new(Fixed), Arc::new(Bridge { addr, calls }), 2).await?;
        let mut c = ready(&s).await?;
        c.claims.idle = 1000;
        let k = derive_epoch_keys(&s.file, &hex16(&c.boot)?, &hex16(&c.session)?)?;
        c.token = egress::mint(&k.grant, &c.claims).ok_or("mint")?;
        let mut client = TcpStream::connect(s.addr).await?;
        client.write_all(format!("POST http://example.com/upload HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\nTrailer: Proxy-Authorization, X-End\r\nProxy-Authorization: {}\r\n\r\n",auth(&c)).as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        client.write_all(b"1\r\na\r\n").await?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        client
            .write_all(
                b"1\r\nb\r\n0\r\nProxy-Authorization: fixture-secret\r\nX-End: allowed\r\n\r\n",
            )
            .await?;
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut out)).await??;
        assert!(out.starts_with(b"HTTP/1.1 200"));
        assert!(out.ends_with(b"uploaded"));
        let seen = seen.lock().map_err(|_| "seen")?;
        assert_eq!(seen.0, b"ab");
        assert!(!seen.1);
        drop(backend);
        Ok(())
    }
}
