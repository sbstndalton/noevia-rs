//! A deliberately read-only Unix Docker transport. Bollard query/model/version types are
//! reused; its unbounded aggregate JSON and frame codecs are not used at the ingress boundary.
use crate::{Container, DockerRead, Job, INVENTORY_CAP, LOG_CAP};
use bollard::models::{ContainerInspectResponse, ContainerSummary, ImageInspect};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{client::conn::http1, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{
    de::{IgnoredAny, SeqAccess, Visitor},
    Deserializer,
};
use std::path::PathBuf;
use tokio::net::UnixStream;

pub struct UnixDocker {
    pub socket: PathBuf,
}
impl UnixDocker {
    #[cfg(feature = "native-diagnose")]
    pub(crate) async fn diagnosis_read(
        &self,
        operation: crate::diagnose_docker::Read<'_>,
        context: &crate::diagnose_docker::Context,
    ) -> Result<(StatusCode, Vec<u8>), crate::diagnose_docker::Error> {
        crate::diagnose_docker::read(&self.socket, operation, context).await
    }
    pub fn from_env() -> Result<Self, &'static str> {
        let host = match std::env::var("DOCKER_HOST") {
            Ok(h) => h,
            Err(std::env::VarError::NotPresent) => "unix:///var/run/docker.sock".into(),
            Err(_) => return Err("DOCKER_HOST must be UTF-8"),
        };
        let path = host
            .strip_prefix("unix://")
            .filter(|p| p.starts_with('/') && p.len() <= 4096)
            .ok_or("only a local Unix Docker socket is supported")?;
        Ok(Self {
            socket: path.into(),
        })
    }
    async fn read(&self, path: &str) -> Result<(StatusCode, Vec<u8>), &'static str> {
        if !allowed_read(path) {
            return Err("Docker operation refused");
        }
        send(&self.socket, "GET", path).await
    }
    async fn inspect(&self, name: &str) -> Result<ContainerInspectResponse, &'static str> {
        let path = format!("/containers/{}/json", segment(name)?);
        let (status, raw) = self.read(&path).await?;
        if status == StatusCode::NOT_FOUND {
            return Err("container not found");
        }
        if status != StatusCode::OK {
            return Err("Docker inspect refused");
        }
        serde_json::from_slice(&raw).map_err(|_| "Docker inspect response refused")
    }
}

async fn send(
    socket: &std::path::Path,
    method: &'static str,
    path: &str,
) -> Result<(StatusCode, Vec<u8>), &'static str> {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("connection", "close")
        .body(Empty::<Bytes>::new())
        .map_err(|_| "Docker request refused")?;
    send_request(socket, method, request).await
}

/// A request with a JSON body; only the sweep allowlist sends one (`POST /containers/create`).
#[cfg(feature = "native-benchmark-runner")]
async fn send_json(
    socket: &std::path::Path,
    path: &str,
    body: Vec<u8>,
) -> Result<(StatusCode, Vec<u8>), &'static str> {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("connection", "close")
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(Bytes::from(body)))
        .map_err(|_| "Docker request refused")?;
    send_request(socket, "POST", request).await
}

async fn send_request<B>(
    socket: &std::path::Path,
    method: &'static str,
    request: Request<B>,
) -> Result<(StatusCode, Vec<u8>), &'static str>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|_| "docker unreachable")?;
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(8192)
        .handshake(TokioIo::new(stream))
        .await
        .map_err(|_| "Docker protocol refused")?;
    // The driver belongs to this request. Every return from the exchange observes its join
    // before the caller can release admission. If this future is dropped instead (timeout or
    // cancellation), the driver is aborted and its join handed to the request's [`ORPHANS`],
    // which the admission observes before the permit is returned (noevia#1385).
    let driver = AbortDriver::spawn(connection);
    let result = exchange(&mut sender, method, request).await;
    driver.finish().await;
    result
}

async fn exchange<B>(
    sender: &mut http1::SendRequest<B>,
    method: &'static str,
    request: Request<B>,
) -> Result<(StatusCode, Vec<u8>), &'static str>
where
    B: hyper::body::Body + Send + 'static,
{
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| "Docker protocol refused")?;
    if response.headers().len() > 32
        || response
            .headers()
            .iter()
            .map(|(k, v)| k.as_str().len().saturating_add(v.as_bytes().len()))
            .sum::<usize>()
            > 8192
    {
        return Err("Docker headers exceeded limit");
    }
    if response.headers().contains_key("transfer-encoding")
        && response.headers().contains_key("content-length")
    {
        return Err("Docker ambiguous framing refused");
    }
    if let Some(te) = response.headers().get("transfer-encoding") {
        if te.as_bytes() != b"chunked" {
            return Err("Docker transfer encoding refused");
        }
    }
    if let Some(length) = response.headers().get("content-length") {
        let length = length
            .to_str()
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or("Docker content length refused")?;
        if length > LOG_CAP {
            return Err("Docker response exceeded limit");
        }
    }
    let status = response.status();
    // Docker answers 304 to stopping an already-stopped container; that is not a redirect.
    if status.is_redirection() && !(method == "POST" && status == StatusCode::NOT_MODIFIED) {
        return Err("Docker redirect refused");
    }
    let mut body = response.into_body();
    let mut raw = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| "Docker response truncated")?;
        let data = frame.into_data().map_err(|_| "Docker trailers refused")?;
        if raw.len().saturating_add(data.len()) > LOG_CAP {
            return Err("Docker response exceeded limit");
        }
        raw.try_reserve(data.len())
            .map_err(|_| "allocation refused")?;
        raw.extend_from_slice(&data);
    }
    Ok((status, raw))
}

/// Joins of connection drivers whose request future was dropped before `finish` (deadline or
/// client gone). The admitted request owns one; its admission observes every join in it before
/// the permit is given back (noevia#1385).
pub(crate) type Orphans = std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>;

tokio::task_local! {
    /// The orphan list of the admitted request this task is running, if any.
    pub(crate) static ORPHANS: Orphans;
}

/// Awaits every join in `orphans`, including any added while waiting.
pub(crate) async fn observe(orphans: &Orphans) {
    loop {
        let batch = match orphans.lock() {
            Ok(mut v) => std::mem::take(&mut *v),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        if batch.is_empty() {
            return;
        }
        for task in batch {
            let _ = task.await;
        }
    }
}

/// Owns one connection task. `finish` aborts it and observes its join. Dropping it without
/// `finish` (the request future was cancelled) requests the abort and hands the join to the
/// request's [`ORPHANS`]; outside an admitted request there is nobody to hand it to.
struct AbortDriver(Option<tokio::task::JoinHandle<()>>);
impl AbortDriver {
    fn spawn<C, T>(connection: C) -> Self
    where
        C: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        Self(Some(spawn_driver(async move {
            let _ = connection.await;
        })))
    }
    async fn finish(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for AbortDriver {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = ORPHANS.try_with(|orphans| match orphans.lock() {
                Ok(mut v) => v.push(task),
                Err(poisoned) => poisoned.into_inner().push(task),
            });
        }
    }
}

#[cfg(not(test))]
fn spawn_driver<C, T>(connection: C) -> tokio::task::JoinHandle<T>
where
    C: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(connection)
}

/// Test builds count each driver from spawn until its future is dropped; see `live`.
#[cfg(test)]
fn spawn_driver<C, T>(connection: C) -> tokio::task::JoinHandle<T>
where
    C: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let live = live::enter();
    tokio::spawn(async move {
        let _live = live;
        connection.await
    })
}

#[cfg(test)]
mod live {
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Counted from spawn until the driver's future is dropped, whether it finished or was aborted.
    pub struct Guard;

    pub fn enter() -> Guard {
        COUNT.fetch_add(1, Ordering::SeqCst);
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            COUNT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn segment(value: &str) -> Result<&str, &'static str> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
    {
        return Err("Docker identifier refused");
    }
    Ok(value)
}
/// The only Docker writes the model manager may make (#1668): restart or stop of the one
/// configured embedding container. Never other containers, never create, exec or kill.
pub fn allowed_write(path: &str, container: &str) -> bool {
    segment(container).is_ok()
        && (path == format!("/containers/{container}/restart?t=10")
            || path == format!("/containers/{container}/stop?t=10"))
}

/// Docker control scoped to one container fixed at construction.
pub struct ScopedControl {
    pub socket: PathBuf,
    container: String,
}
impl ScopedControl {
    pub fn new(socket: PathBuf, container: &str) -> Result<Self, &'static str> {
        segment(container)?;
        // Docker resolves an all-hex name as an id prefix too, which could reach another
        // container; only a real container name is accepted.
        if container.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("EMBED_CONTAINER must be a container name, not an id");
        }
        Ok(Self {
            socket,
            container: container.to_owned(),
        })
    }
}
impl crate::embedding::ServiceControl for ScopedControl {
    fn act(&self, action: crate::embedding::Action) -> Job<'_, ()> {
        Box::pin(async move {
            let verb = match action {
                crate::embedding::Action::Restart => "restart",
                crate::embedding::Action::Stop => "stop",
            };
            let path = format!("/containers/{}/{verb}?t=10", self.container);
            if !allowed_write(&path, &self.container) {
                return Err("Docker operation refused");
            }
            let (status, _) = send(&self.socket, "POST", &path).await?;
            match status.as_u16() {
                // 304: already stopped.
                204 | 304 => Ok(()),
                404 => Err("embedding container not found"),
                _ => Err("Docker refused the embedding container action"),
            }
        })
    }
}

/// noevia#1318: the backend restart route may restart a llama.cpp backend (the caller has
/// checked it is one of the effective backends): exactly `restart?t=15`, by name, never an id.
pub fn allowed_backend_restart(path: &str, container: &str) -> bool {
    segment(container).is_ok()
        && !container.bytes().all(|c| c.is_ascii_hexdigit())
        && path == format!("/containers/{container}/restart?t=15")
}

fn allowed_read(path: &str) -> bool {
    if path == "/containers/json?all=true&limit=1025&size=false" {
        return true;
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let parts: Vec<_> = path.split('/').collect();
    match parts.as_slice() {
        ["", "containers" | "images", id, "json"] => query.is_empty() && segment(id).is_ok(),
        ["", "containers", id, "logs"] => {
            segment(id).is_ok()
                && query
                    .strip_prefix("stdout=true&stderr=true&timestamps=false&follow=false&tail=")
                    .and_then(|s| s.parse::<usize>().ok())
                    .is_some_and(|n| (20..=2000).contains(&n))
        }
        _ => false,
    }
}
impl DockerRead for UnixDocker {
    fn backend_spec<'a>(&'a self, name: &'a str) -> Job<'a, Option<crate::BackendSpec>> {
        Box::pin(async move {
            let inspect = match self.inspect(name).await {
                Ok(i) => i,
                Err("container not found") => return Ok(None),
                Err(e) => return Err(e),
            };
            let mut ports: Vec<(String, Vec<String>)> = inspect
                .network_settings
                .and_then(|n| n.ports)
                .unwrap_or_default()
                .into_iter()
                .map(|(port, bindings)| {
                    let hosts = bindings
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|b| b.host_port.filter(|h| !h.is_empty()))
                        .collect();
                    (port, hosts)
                })
                .collect();
            ports.sort();
            let config = inspect.config.unwrap_or_default();
            Ok(Some(crate::BackendSpec {
                ports,
                cmd: config.cmd.unwrap_or_default(),
                entrypoint: config.entrypoint.unwrap_or_default(),
                env: config.env.unwrap_or_default(),
            }))
        })
    }
    fn restart_backend<'a>(&'a self, name: &'a str) -> Job<'a, ()> {
        Box::pin(async move {
            let path = format!("/containers/{name}/restart?t=15");
            if !allowed_backend_restart(&path, name) {
                return Err("Docker operation refused");
            }
            let (status, _) = send(&self.socket, "POST", &path).await?;
            match status.as_u16() {
                204 => Ok(()),
                404 => Err("container not found"),
                _ => Err("Docker refused the restart"),
            }
        })
    }
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async move {
            // The explicit query mirrors Bollard ListContainersOptions, with a sentinel extra
            // result: reaching the sentinel refuses rather than silently hiding older backends.
            let options = bollard::query_parameters::ListContainersOptions {
                all: true,
                limit: Some(1025),
                size: false,
                filters: None,
            };
            let path = format!(
                "/containers/json?all={}&limit={}&size={}",
                options.all,
                options.limit.unwrap_or(1025),
                options.size
            );
            let (status, raw) = self.read(&path).await?;
            if status != StatusCode::OK {
                return Err("docker unreachable");
            }
            let inventory = bounded_inventory(&raw)?;
            if inventory.len() > INVENTORY_CAP {
                return Err("container inventory exceeded limit");
            }
            let mut out = Vec::new();
            out.try_reserve(inventory.len())
                .map_err(|_| "allocation refused")?;
            for summary in inventory {
                let Some(id) = summary.id else {
                    return Err("Docker inventory refused");
                };
                let inspected = self.inspect(&id).await?;
                let name = inspected
                    .name
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_owned();
                if name.len() > 256 {
                    return Err("container name exceeded limit");
                }
                let Some(image) = inspected.image else {
                    continue;
                };
                let (status, raw) = self
                    .read(&format!("/images/{}/json", segment(&image)?))
                    .await?;
                // Python image access errors make this container undiscoverable.
                if status != StatusCode::OK {
                    continue;
                }
                let image: ImageInspect =
                    serde_json::from_slice(&raw).map_err(|_| "Docker image response refused")?;
                let tag = image
                    .repo_tags
                    .and_then(|tags| tags.into_iter().next())
                    .unwrap_or_default();
                if tag.len() > 4096 {
                    return Err("image tag exceeded limit");
                }
                out.push(Container {
                    name,
                    first_image_tag: tag,
                });
            }
            Ok(out)
        })
    }
    fn logs<'a>(&'a self, name: &'a str, tail: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async move {
            if !(20..=2000).contains(&tail) {
                return Err("Docker tail refused");
            }
            let inspected = self.inspect(name).await?;
            let tty = inspected.config.and_then(|c| c.tty).unwrap_or(false);
            let options = bollard::query_parameters::LogsOptions {
                stdout: true,
                stderr: true,
                timestamps: false,
                follow: false,
                tail: tail.to_string(),
                ..Default::default()
            };
            let path = format!(
                "/containers/{}/logs?stdout={}&stderr={}&timestamps={}&follow={}&tail={}",
                segment(name)?,
                options.stdout,
                options.stderr,
                options.timestamps,
                options.follow,
                options.tail
            );
            let (status, raw) = self.read(&path).await?;
            if status != StatusCode::OK {
                return Err("log fetch failed");
            }
            decode_frames(&raw, tty)
        })
    }
}

fn bounded_inventory(raw: &[u8]) -> Result<Vec<ContainerSummary>, &'static str> {
    let exceeded = std::cell::Cell::new(false);
    struct Inventory<'a>(&'a std::cell::Cell<bool>);
    impl<'de> Visitor<'de> for Inventory<'_> {
        type Value = Vec<ContainerSummary>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bounded Docker container array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            loop {
                if out.len() == INVENTORY_CAP {
                    if seq.next_element::<IgnoredAny>()?.is_some() {
                        self.0.set(true);
                        return Err(serde::de::Error::custom("inventory cap"));
                    }
                    break;
                }
                let Some(container) = seq.next_element::<ContainerSummary>()? else {
                    break;
                };
                out.try_reserve(1)
                    .map_err(|_| serde::de::Error::custom("allocation refused"))?;
                out.push(container);
            }
            Ok(out)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(raw);
    let result = deserializer
        .deserialize_seq(Inventory(&exceeded))
        .map_err(|_| {
            if exceeded.get() {
                "container inventory exceeded limit"
            } else {
                "Docker inventory refused"
            }
        })?;
    deserializer.end().map_err(|_| "Docker inventory refused")?;
    Ok(result)
}

pub fn decode_frames(raw: &[u8], tty: bool) -> Result<Vec<u8>, &'static str> {
    if raw.len() > LOG_CAP {
        return Err("Docker response exceeded limit");
    }
    let mut out = Vec::new();
    out.try_reserve(raw.len())
        .map_err(|_| "allocation refused")?;
    if tty {
        out.extend_from_slice(raw);
        return Ok(out);
    }
    let mut remaining = raw;
    while !remaining.is_empty() {
        let header = remaining.get(..8).ok_or("Docker log frame truncated")?;
        let kind = header
            .first()
            .copied()
            .ok_or("Docker log frame truncated")?;
        if !matches!(kind, 1 | 2) || header.get(1..4) != Some(&[0, 0, 0][..]) {
            return Err("Docker log frame refused");
        }
        let length_bytes: [u8; 4] = header
            .get(4..8)
            .ok_or("Docker log frame truncated")?
            .try_into()
            .map_err(|_| "Docker log frame refused")?;
        let length = usize::try_from(u32::from_be_bytes(length_bytes))
            .map_err(|_| "Docker log frame exceeded limit")?;
        if length > LOG_CAP || out.len().saturating_add(length) > LOG_CAP {
            return Err("Docker log frame exceeded limit");
        }
        let end = 8usize
            .checked_add(length)
            .ok_or("Docker log frame exceeded limit")?;
        let payload = remaining.get(8..end).ok_or("Docker log frame truncated")?;
        out.extend_from_slice(payload);
        remaining = remaining.get(end..).ok_or("Docker log frame truncated")?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The driver counter is process-wide, so these controls run one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A synthetic Unix Docker peer: reads one request head, writes `response`, then keeps the
    /// socket open until the client closes it, so a refused read never sees an end of stream.
    fn serve(response: Vec<u8>) -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<()>) {
        let root = tempfile::Builder::new()
            .prefix("mm-docker-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let peer = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 512];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match peer.read(&mut chunk).await {
                    Ok(n) if n > 0 => head.extend_from_slice(chunk.get(..n).unwrap()),
                    _ => break,
                }
            }
            let _ = peer.write_all(&response).await;
            let _ = tokio::time::timeout(Duration::from_secs(5), peer.read(&mut chunk)).await;
        });
        (root, socket, peer)
    }

    #[tokio::test]
    async fn answered_read_observes_its_driver_join() {
        let _serial = SERIAL.lock().await;
        let (_root, socket, peer) =
            serve(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_vec());
        let docker = UnixDocker { socket };
        let (status, body) = docker.read("/containers/synthetic/json").await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"{}");
        // Admission may be released once read returns, so its driver must already be joined.
        assert_eq!(live::COUNT.load(Ordering::SeqCst), 0);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn refused_framing_observes_its_driver_join() {
        let _serial = SERIAL.lock().await;
        let (_root, socket, peer) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{}".to_vec(),
        );
        let docker = UnixDocker { socket };
        assert!(docker.read("/containers/synthetic/json").await.is_err());
        assert_eq!(live::COUNT.load(Ordering::SeqCst), 0);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn early_refusal_with_pending_body_observes_its_driver_join() {
        let _serial = SERIAL.lock().await;
        // The declared length is over the cap, so read refuses from the headers while the body
        // is still pending on the open socket: the driver is live when the refusal is returned.
        let (_root, socket, peer) = serve(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", LOG_CAP + 1).into_bytes(),
        );
        let docker = UnixDocker { socket };
        assert!(matches!(
            docker.read("/containers/synthetic/json").await,
            Err("Docker response exceeded limit")
        ));
        assert_eq!(live::COUNT.load(Ordering::SeqCst), 0);
        peer.await.unwrap();
    }

    /// noevia#1385: a Docker peer that accepts and then never answers, held open until dropped.
    fn silent() -> (tempfile::TempDir, PathBuf, tokio::sync::oneshot::Sender<()>) {
        let root = tempfile::Builder::new()
            .prefix("mm-docker-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = root.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut held = Vec::new();
            let accept = async {
                loop {
                    if let Ok((peer, _)) = listener.accept().await {
                        held.push(peer);
                    }
                }
            };
            tokio::select! { _ = accept => {}, _ = stopped => {} }
        });
        (root, socket, stop)
    }

    fn admitted_state(socket: PathBuf) -> std::sync::Arc<crate::StateData> {
        std::sync::Arc::new(crate::StateData {
            #[cfg(feature = "native-diagnose")]
            diagnosis: None,
            #[cfg(feature = "native-prompts")]
            prompts: None,
            #[cfg(feature = "native-settings")]
            settings: None,
            #[cfg(feature = "native-model-updates")]
            model_updates: None,
            #[cfg(feature = "native-model-detail")]
            model_detail: None,
            sections: None,
            embedding: None,
            host: None,
            token: String::new(),
            configured: vec!["synthetic".into()],
            docker: std::sync::Arc::new(UnixDocker { socket }),
            redactor: crate::Redactor::new().unwrap(),
            permits: tokio::sync::Semaphore::new(4),
            legacy: crate::legacy_client().unwrap(),
            legacy_base: crate::LEGACY_BASE.to_owned(),
            idle_unload: None,
            #[cfg(feature = "native-download-targets")]
            download_targets: None,
            #[cfg(feature = "native-badges")]
            badges: None,
            #[cfg(feature = "native-bench-runs")]
            bench_runs: None,
            #[cfg(feature = "native-section-writes")]
            section_writes: None,
            #[cfg(feature = "native-settings-write")]
            settings_write: None,
            #[cfg(feature = "native-benchmark-overview")]
            benchmark_overview: None,
            #[cfg(feature = "native-check-updates")]
            check_updates: None,
            #[cfg(feature = "native-benchmark-runner")]
            bench_runner: None,
            #[cfg(feature = "native-backend-control")]
            backends: None,
        })
    }

    fn logs_request() -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .uri("/api/v1/backends/synthetic/logs")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_request_returns_its_permit_only_after_the_driver_join() {
        use tower::ServiceExt;
        let _serial = SERIAL.lock().await;
        let (_root, socket, _stop) = silent();
        let state = admitted_state(socket);
        let response = crate::router(state.clone())
            .oneshot(logs_request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        // The permit is back, and the driver the timed-out read left behind is already joined.
        assert_eq!(state.permits.available_permits(), 4);
        assert_eq!(live::COUNT.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_dropped_request_holds_its_permit_until_the_driver_join_is_observed() {
        use tower::ServiceExt;
        let _serial = SERIAL.lock().await;
        let (_root, socket, _stop) = silent();
        let state = admitted_state(socket);
        let mut request = Box::pin(crate::router(state.clone()).oneshot(logs_request()));
        let started = std::time::Instant::now();
        while live::COUNT.load(Ordering::SeqCst) == 0 {
            tokio::select! {
                biased;
                _ = &mut request => panic!("the silent peer answered"),
                _ = tokio::task::yield_now() => {}
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "no driver started"
            );
        }
        assert_eq!(state.permits.available_permits(), 3);
        // The client goes away: the request future is dropped mid-read.
        drop(request);
        let live_now = live::COUNT.load(Ordering::SeqCst);
        assert!(
            state.permits.available_permits() == 3 || live_now == 0,
            "the permit came back while a driver ({live_now}) was still unjoined"
        );
        // Whenever the permit is back, no driver may still be live.
        let started = std::time::Instant::now();
        loop {
            let permits = state.permits.available_permits();
            if permits == 4 {
                assert_eq!(live::COUNT.load(Ordering::SeqCst), 0);
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "permit never returned"
            );
            tokio::task::yield_now().await;
        }
    }
}

/// noevia#1318: the Docker access llama-bench sweeps need, and nothing more.
///
/// [`sweep::allowed_sweep`] is the sweep allowlist. Every request this module sends passes it
/// first, so a sweep can never reach a container it did not create or find by the exact sweep
/// label:
///
/// * `POST /containers/create`, only with a body built here from a typed [`sweep::CreateBody`]
///   and checked by [`sweep::check_create`] against the backend it mirrors;
/// * `start`, `kill`, inspect, `logs` and `DELETE ?force=true`, only for a full container id
///   this process created (and recorded in memory) or saw in a label-filtered list;
/// * `GET /containers/json`, only with the sweep label filter.
///
/// The backend's own inspect and image reads go through the existing read allowlist.
#[cfg(feature = "native-benchmark-runner")]
pub mod sweep {
    use super::{allowed_read, decode_frames, segment, send, send_json, StatusCode};
    use crate::Job;
    use serde::{Deserialize, Serialize};
    use std::{
        collections::{BTreeMap, HashSet},
        path::PathBuf,
        sync::Mutex,
    };

    /// `bench.SWEEP_LABEL`, stamped `"1"` on every sweep container.
    pub const SWEEP_LABEL: &str = "noevia.model-manager.bench";
    /// The only entrypoint a sweep container runs.
    pub const ENTRYPOINT: &str = "/app/llama";
    /// Label-filtered list: the exact query `GET /containers/json` may carry.
    pub const LIST_PATH: &str =
        "/containers/json?all=true&filters=%7B%22label%22%3A%5B%22noevia.model-manager.bench%3D1%22%5D%7D";
    /// How many leftover sweep containers one list may return.
    pub const LIST_CAP: usize = 64;

    /// A GPU request as Docker reports and accepts it.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
    #[serde(rename_all = "PascalCase")]
    pub struct DeviceRequest {
        #[serde(default, deserialize_with = "nullable")]
        pub driver: String,
        #[serde(default, deserialize_with = "nullable")]
        pub count: i64,
        #[serde(rename = "DeviceIDs", default, deserialize_with = "nullable")]
        pub device_ids: Vec<String>,
        #[serde(default, deserialize_with = "nullable")]
        pub capabilities: Vec<Vec<String>>,
        #[serde(default, deserialize_with = "nullable")]
        pub options: BTreeMap<String, String>,
    }

    /// Docker writes `null` for an unset list or map.
    fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: Default + Deserialize<'de>,
    {
        Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
    }

    /// What a sweep mirrors from the backend container, read live by inspect.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BackendRuntime {
        /// `c.image.tags[0]`.
        pub image: String,
        /// `(Source, Destination)` of each mount with both.
        pub mounts: Vec<(String, String)>,
        pub device_requests: Vec<DeviceRequest>,
        /// `Config.WorkingDir` ("/" when unset), where a relative model path resolves.
        pub working_dir: String,
        /// The models directory (`MODELS_DIR`): the only place a sweep may mount from.
        pub models_dir: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "PascalCase")]
    pub struct HostConfig {
        pub binds: Vec<String>,
        pub network_mode: String,
        pub device_requests: Vec<DeviceRequest>,
    }

    /// The whole `POST /containers/create` body. Its fields are the only ones a sweep sets:
    /// no env, ports, privileges, capabilities, devices, namespaces or security options exist
    /// to be set.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "PascalCase")]
    pub struct CreateBody {
        pub image: String,
        pub cmd: Vec<String>,
        pub entrypoint: Vec<String>,
        pub labels: BTreeMap<String, String>,
        pub host_config: HostConfig,
    }

    /// `/`-joined lexical normal form (no `.`, `..` pops).
    fn normal(path: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                p => out.push(p),
            }
        }
        out
    }

    /// Whether a mount destination lies in the models directory.
    fn in_models(runtime: &BackendRuntime, dst: &str) -> bool {
        let (d, m) = (normal(dst), normal(&runtime.models_dir));
        !m.is_empty() && d.starts_with(&m)
    }

    /// The backend mounts a sweep of `model_path` needs: those in the models directory whose
    /// destination holds the model (a relative path resolves against the backend's working
    /// directory, as the container resolves it). Nothing outside the models directory is ever
    /// mounted, whatever the model path says.
    pub fn model_mounts(runtime: &BackendRuntime, model_path: &str) -> Vec<(String, String)> {
        let full = if model_path.starts_with('/') {
            model_path.to_owned()
        } else {
            format!("{}/{model_path}", runtime.working_dir)
        };
        let model = normal(&full);
        runtime
            .mounts
            .iter()
            .filter(|(_, dst)| {
                let d = normal(dst);
                in_models(runtime, dst) && model.len() > d.len() && model.starts_with(&d)
            })
            .cloned()
            .collect()
    }

    /// The create body for one sweep of `model_path` with llama-bench arguments `cmd`.
    pub fn create_body(runtime: &BackendRuntime, model_path: &str, cmd: Vec<String>) -> CreateBody {
        CreateBody {
            image: runtime.image.clone(),
            cmd,
            entrypoint: vec![ENTRYPOINT.to_owned()],
            labels: BTreeMap::from([(SWEEP_LABEL.to_owned(), "1".to_owned())]),
            host_config: HostConfig {
                binds: model_mounts(runtime, model_path)
                    .into_iter()
                    .map(|(src, dst)| format!("{src}:{dst}:ro"))
                    .collect(),
                network_mode: "none".to_owned(),
                device_requests: runtime.device_requests.clone(),
            },
        }
    }

    /// The hard limits on a create, checked again on the finished body right before it is sent.
    pub fn check_create(body: &CreateBody, runtime: &BackendRuntime) -> Result<(), &'static str> {
        if body.labels.len() != 1 || body.labels.get(SWEEP_LABEL).map(String::as_str) != Some("1") {
            return Err("sweep container must carry exactly the sweep label");
        }
        if body.image.is_empty() || body.image != runtime.image {
            return Err("sweep image must be the backend's image");
        }
        if body.entrypoint != [ENTRYPOINT] {
            return Err("sweep entrypoint refused");
        }
        if body.host_config.network_mode != "none" {
            return Err("sweep network must be none");
        }
        if body.host_config.device_requests != runtime.device_requests {
            return Err("sweep GPU request must be the backend's");
        }
        for bind in &body.host_config.binds {
            let Some(spec) = bind.strip_suffix(":ro") else {
                return Err("sweep binds must be read-only");
            };
            if !runtime
                .mounts
                .iter()
                .any(|(src, dst)| spec == format!("{src}:{dst}") && in_models(runtime, dst))
            {
                return Err("sweep bind is not a backend models mount");
            }
        }
        Ok(())
    }

    /// A full container id as Docker returns it from create and list (never a name or prefix).
    pub fn full_id(id: &str) -> bool {
        id.len() == 64 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    }

    /// The sweep allowlist. `known` holds the ids this process created or listed by label.
    pub fn allowed_sweep(method: &str, path: &str, known: &HashSet<String>) -> bool {
        if (method, path) == ("POST", "/containers/create") || (method, path) == ("GET", LIST_PATH)
        {
            return true;
        }
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let parts: Vec<&str> = route.split('/').collect();
        let ours = |id: &str| full_id(id) && known.contains(id);
        match (method, parts.as_slice(), query) {
            ("POST", ["", "containers", id, "start" | "kill"], "")
            | ("GET", ["", "containers", id, "json"], "")
            | ("DELETE", ["", "containers", id], "force=true") => ours(id),
            ("GET", ["", "containers", id, "logs"], q) => {
                ours(id)
                    && (q == "stdout=true&stderr=false&timestamps=false&follow=false&tail=all"
                        || q == "stdout=false&stderr=true&timestamps=false&follow=false&tail=all")
            }
            _ => false,
        }
    }

    /// docker-py's exception for an answer: `NotFound` for 404, else `APIError`, with Docker's
    /// own message.
    fn docker_error(status: StatusCode, raw: &[u8]) -> String {
        #[derive(Deserialize)]
        struct Message {
            message: String,
        }
        let kind = if status == StatusCode::NOT_FOUND {
            "NotFound"
        } else {
            "APIError"
        };
        let message = serde_json::from_slice::<Message>(raw)
            .map(|m| m.message)
            .unwrap_or_else(|_| status.to_string());
        let message: String = message.chars().take(500).collect();
        format!("{kind}: {message}")
    }

    /// The sweep side of the Unix Docker transport.
    pub struct UnixSweepDocker {
        pub socket: PathBuf,
        models_dir: String,
        known: Mutex<HashSet<String>>,
    }

    impl UnixSweepDocker {
        pub fn new(socket: PathBuf, models_dir: &str) -> Self {
            Self {
                socket,
                models_dir: models_dir.to_owned(),
                known: Mutex::new(HashSet::new()),
            }
        }

        fn known(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
            self.known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        async fn sweep(
            &self,
            method: &'static str,
            path: &str,
        ) -> Result<(StatusCode, Vec<u8>), String> {
            if !allowed_sweep(method, path, &self.known()) {
                return Err("Docker operation refused".into());
            }
            send(&self.socket, method, path)
                .await
                .map_err(str::to_owned)
        }

        async fn read(&self, path: &str) -> Result<(StatusCode, Vec<u8>), String> {
            if !allowed_read(path) {
                return Err("Docker operation refused".into());
            }
            send(&self.socket, "GET", path).await.map_err(str::to_owned)
        }
    }

    impl UnixSweepDocker {
        /// `bench._resolve_endpoint(container)`: `http://{container}:{port}` with the internal
        /// port `services._extract_ports` picks (the last published key whose number parses, in
        /// Docker's order), or Python's error text.
        pub async fn endpoint(&self, container: &str) -> Result<String, String> {
            let Ok(name) = segment(container) else {
                return Err(format!("NotFound: No such container: {container}"));
            };
            let (status, raw) = self.read(&format!("/containers/{name}/json")).await?;
            if status != StatusCode::OK {
                return Err(docker_error(status, &raw));
            }
            let text = String::from_utf8(raw).map_err(|_| "no internal port exposed".to_owned())?;
            let parsed = model_files::json::parse_with_limits(
                &text,
                model_files::json::Limits {
                    depth: 512,
                    string_chars: usize::MAX,
                    ..model_files::json::Limits::default()
                },
            )
            .map_err(|_| "no internal port exposed".to_owned())?;
            let mut internal: Option<i64> = None;
            if let Some(model_files::json::Value::Obj(ports)) =
                parsed.get("NetworkSettings").and_then(|n| n.get("Ports"))
            {
                for (key, _) in ports {
                    let head = key.split('/').next().unwrap_or_default();
                    if let Ok(Some(n)) =
                        crate::bench_runner::py_int(&model_files::json::Value::Str(head.to_owned()))
                    {
                        internal = Some(n);
                    }
                }
            }
            match internal {
                Some(port) if port != 0 => Ok(format!("http://{container}:{port}")),
                _ => Err("no internal port exposed".into()),
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Inspect {
        #[serde(default)]
        image: Option<String>,
        #[serde(default)]
        mounts: Option<Vec<Mount>>,
        #[serde(default)]
        host_config: Option<Host>,
        #[serde(default)]
        config: Option<Config>,
        #[serde(default)]
        state: Option<State>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Mount {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        destination: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Host {
        #[serde(default)]
        device_requests: Option<Vec<DeviceRequest>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Config {
        #[serde(default)]
        working_dir: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct State {
        #[serde(default)]
        status: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Image {
        #[serde(default)]
        repo_tags: Option<Vec<String>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Created {
        id: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Listed {
        id: String,
        #[serde(default)]
        labels: Option<BTreeMap<String, String>>,
    }

    impl crate::bench_runner::SweepDocker for UnixSweepDocker {
        fn runtime<'a>(&'a self, backend: &'a str) -> Job<'a, Result<BackendRuntime, String>> {
            Box::pin(async move {
                let Ok(name) = segment(backend) else {
                    return Ok(Err(format!("NotFound: No such container: {backend}")));
                };
                let (status, raw) = match self.read(&format!("/containers/{name}/json")).await {
                    Ok(r) => r,
                    Err(e) => return Ok(Err(e)),
                };
                if status != StatusCode::OK {
                    return Ok(Err(docker_error(status, &raw)));
                }
                let Ok(inspect) = serde_json::from_slice::<Inspect>(&raw) else {
                    return Ok(Err("APIError: Docker inspect response refused".into()));
                };
                let image = match inspect.image.as_deref().map(segment) {
                    Some(Ok(id)) => match self.read(&format!("/images/{id}/json")).await {
                        Ok((StatusCode::OK, raw)) => serde_json::from_slice::<Image>(&raw)
                            .ok()
                            .and_then(|i| i.repo_tags)
                            .and_then(|t| t.into_iter().next())
                            .unwrap_or_default(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                if image.is_empty() {
                    return Ok(Err("cannot determine backend image".into()));
                }
                let mounts = inspect
                    .mounts
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|m| match (m.source, m.destination) {
                        (Some(s), Some(d)) if !s.is_empty() && !d.is_empty() => Some((s, d)),
                        _ => None,
                    })
                    .collect();
                let working_dir = inspect
                    .config
                    .and_then(|c| c.working_dir)
                    .filter(|w| !w.is_empty())
                    .unwrap_or_else(|| "/".into());
                Ok(Ok(BackendRuntime {
                    image,
                    mounts,
                    device_requests: inspect
                        .host_config
                        .and_then(|h| h.device_requests)
                        .unwrap_or_default(),
                    working_dir,
                    models_dir: self.models_dir.clone(),
                }))
            })
        }

        fn run<'a>(
            &'a self,
            body: &'a CreateBody,
            runtime: &'a BackendRuntime,
        ) -> Job<'a, Result<String, String>> {
            Box::pin(async move {
                if let Err(e) = check_create(body, runtime) {
                    return Ok(Err(e.to_owned()));
                }
                let Ok(json) = serde_json::to_vec(body) else {
                    return Ok(Err("Docker request refused".into()));
                };
                if !allowed_sweep("POST", "/containers/create", &self.known()) {
                    return Ok(Err("Docker operation refused".into()));
                }
                let (status, raw) = match send_json(&self.socket, "/containers/create", json).await
                {
                    Ok(r) => r,
                    Err(e) => return Ok(Err(e.to_owned())),
                };
                if status != StatusCode::CREATED {
                    return Ok(Err(docker_error(status, &raw)));
                }
                let Some(id) = serde_json::from_slice::<Created>(&raw)
                    .ok()
                    .map(|c| c.id)
                    .filter(|id| full_id(id))
                else {
                    return Ok(Err("APIError: Docker create response refused".into()));
                };
                self.known().insert(id.clone());
                let started = self.sweep("POST", &format!("/containers/{id}/start")).await;
                match started {
                    Ok((StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED, _)) => Ok(Ok(id)),
                    other => {
                        // run() = create + start: a failed start leaves our created container;
                        // remove it by the id we hold.
                        let _ = self.remove(&id).await;
                        Ok(Err(match other {
                            Ok((status, raw)) => docker_error(status, &raw),
                            Err(e) => e,
                        }))
                    }
                }
            })
        }

        fn status<'a>(&'a self, id: &'a str) -> Job<'a, Result<String, String>> {
            Box::pin(async move {
                let (status, raw) = match self.sweep("GET", &format!("/containers/{id}/json")).await
                {
                    Ok(r) => r,
                    Err(e) => return Ok(Err(e)),
                };
                if status != StatusCode::OK {
                    return Ok(Err(docker_error(status, &raw)));
                }
                Ok(serde_json::from_slice::<Inspect>(&raw)
                    .ok()
                    .and_then(|i| i.state)
                    .and_then(|s| s.status)
                    .ok_or_else(|| "APIError: Docker inspect response refused".to_owned()))
            })
        }

        fn kill<'a>(&'a self, id: &'a str) -> Job<'a, Result<(), String>> {
            Box::pin(async move {
                match self.sweep("POST", &format!("/containers/{id}/kill")).await {
                    Ok((StatusCode::NO_CONTENT, _)) => Ok(Ok(())),
                    Ok((status, raw)) => Ok(Err(docker_error(status, &raw))),
                    Err(e) => Ok(Err(e)),
                }
            })
        }

        fn logs<'a>(&'a self, id: &'a str, stdout: bool) -> Job<'a, Result<Vec<u8>, String>> {
            Box::pin(async move {
                let query = if stdout {
                    "stdout=true&stderr=false"
                } else {
                    "stdout=false&stderr=true"
                };
                let path =
                    format!("/containers/{id}/logs?{query}&timestamps=false&follow=false&tail=all");
                match self.sweep("GET", &path).await {
                    // Sweep containers are created without a TTY: multiplexed frames.
                    Ok((StatusCode::OK, raw)) => {
                        Ok(decode_frames(&raw, false).map_err(str::to_owned))
                    }
                    Ok((status, raw)) => Ok(Err(docker_error(status, &raw))),
                    Err(e) => Ok(Err(e)),
                }
            })
        }

        fn remove<'a>(&'a self, id: &'a str) -> Job<'a, Result<(), String>> {
            Box::pin(async move {
                let answer = self
                    .sweep("DELETE", &format!("/containers/{id}?force=true"))
                    .await;
                let out = match answer {
                    Ok((StatusCode::NO_CONTENT, _)) => Ok(()),
                    Ok((status, raw)) => Err(docker_error(status, &raw)),
                    Err(e) => Err(e),
                };
                if out.is_ok() || out.as_ref().is_err_and(|e| e.starts_with("NotFound:")) {
                    self.known().remove(id);
                }
                Ok(out)
            })
        }

        fn list_labelled(&self) -> Job<'_, Result<Vec<String>, String>> {
            Box::pin(async move {
                let (status, raw) = match self.sweep("GET", LIST_PATH).await {
                    Ok(r) => r,
                    Err(e) => return Ok(Err(e)),
                };
                if status != StatusCode::OK {
                    return Ok(Err(docker_error(status, &raw)));
                }
                let Ok(listed) = serde_json::from_slice::<Vec<Listed>>(&raw) else {
                    return Ok(Err("APIError: Docker list response refused".into()));
                };
                if listed.len() > LIST_CAP {
                    return Ok(Err("APIError: too many sweep containers listed".into()));
                }
                // Docker filtered by label; the label is checked again here, and only full ids
                // carrying it exactly are ever acted on.
                let ids: Vec<String> = listed
                    .into_iter()
                    .filter(|c| {
                        full_id(&c.id)
                            && c.labels
                                .as_ref()
                                .and_then(|l| l.get(SWEEP_LABEL))
                                .is_some_and(|v| v == "1")
                    })
                    .map(|c| c.id)
                    .collect();
                self.known().extend(ids.iter().cloned());
                Ok(Ok(ids))
            })
        }
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
        use super::*;

        fn runtime() -> BackendRuntime {
            BackendRuntime {
                image: "ghcr.io/example/llama:full".into(),
                mounts: vec![
                    ("/mnt/models".into(), "/models".into()),
                    ("/mnt/cache".into(), "/root/.cache".into()),
                    ("/var/run/docker.sock".into(), "/var/run/docker.sock".into()),
                ],
                device_requests: vec![DeviceRequest {
                    driver: "nvidia".into(),
                    count: -1,
                    capabilities: vec![vec!["gpu".into()]],
                    ..DeviceRequest::default()
                }],
                working_dir: "/app".into(),
                models_dir: "/models".into(),
            }
        }

        #[test]
        fn the_create_body_has_only_the_mirrored_fields() {
            let body = create_body(&runtime(), "/models/a/../b.gguf", vec!["bench".into()]);
            check_create(&body, &runtime()).unwrap();
            let json = serde_json::to_value(&body).unwrap();
            let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
            assert_eq!(keys, ["Cmd", "Entrypoint", "HostConfig", "Image", "Labels"]);
            let host: Vec<&String> = json["HostConfig"].as_object().unwrap().keys().collect();
            assert_eq!(host, ["Binds", "DeviceRequests", "NetworkMode"]);
            assert_eq!(
                json["HostConfig"]["Binds"],
                serde_json::json!(["/mnt/models:/models:ro"])
            );
            assert_eq!(json["HostConfig"]["NetworkMode"], "none");
            assert_eq!(json["Entrypoint"], serde_json::json!(["/app/llama"]));
            assert_eq!(json["Labels"], serde_json::json!({SWEEP_LABEL: "1"}));
            // A relative model path resolves against the backend's working directory; a path
            // outside every mount mounts nothing (never the Docker socket).
            let rel = create_body(&runtime(), "../models/x.gguf", vec![]);
            assert_eq!(rel.host_config.binds, ["/mnt/models:/models:ro"]);
            for path in ["/var/run/docker.sock/x", "/elsewhere/x.gguf", "/models"] {
                let body = create_body(&runtime(), path, vec![]);
                assert!(
                    body.host_config
                        .binds
                        .iter()
                        .all(|b| !b.contains("docker.sock"))
                        && (path != "/elsewhere/x.gguf" || body.host_config.binds.is_empty())
                        && (path != "/models" || body.host_config.binds.is_empty()),
                    "{path}: {:?}",
                    body.host_config.binds
                );
            }
        }

        #[test]
        fn every_create_limit_is_enforced() {
            let good = create_body(&runtime(), "/models/x.gguf", vec![]);
            let refused = |f: &dyn Fn(&mut CreateBody)| {
                let mut b = good.clone();
                f(&mut b);
                check_create(&b, &runtime()).is_err()
            };
            assert!(refused(&|b| {
                b.labels.insert("other".into(), "1".into());
            }));
            assert!(refused(&|b| {
                b.labels.insert(SWEEP_LABEL.into(), "true".into());
            }));
            assert!(refused(&|b| b.image = "busybox".into()));
            assert!(refused(&|b| b.image = String::new()));
            assert!(refused(&|b| b.entrypoint = vec!["/bin/sh".into()]));
            assert!(refused(&|b| b.host_config.network_mode = "host".into()));
            assert!(refused(&|b| b.host_config.network_mode = "bridge".into()));
            assert!(refused(
                &|b| b.host_config.binds = vec!["/mnt/models:/models:rw".into()]
            ));
            assert!(refused(
                &|b| b.host_config.binds = vec!["/mnt/models:/models".into()]
            ));
            assert!(refused(&|b| b.host_config.binds = vec!["/:/host:ro".into()]));
            assert!(refused(
                &|b| b.host_config.binds = vec!["/mnt/cache:/root/.cache:ro".into()]
            ));
            assert!(refused(&|b| {
                b.host_config.binds = vec!["/var/run/docker.sock:/var/run/docker.sock:ro".into()];
            }));
            assert!(refused(&|b| b.host_config.device_requests = vec![]));
            assert!(refused(
                &|b| b.host_config.device_requests[0].device_ids = vec!["0".into()]
            ));
            let mut empty = runtime();
            empty.image = String::new();
            assert!(check_create(&create_body(&empty, "/models/x", vec![]), &empty).is_err());
        }

        #[test]
        fn the_sweep_allowlist_names_every_request() {
            let ours = "a".repeat(64);
            let stranger = "b".repeat(64);
            let known: HashSet<String> = [ours.clone()].into();
            let logs = "timestamps=false&follow=false&tail=all";
            for (method, path) in [
                ("POST", "/containers/create".to_owned()),
                ("GET", LIST_PATH.to_owned()),
                ("POST", format!("/containers/{ours}/start")),
                ("POST", format!("/containers/{ours}/kill")),
                ("GET", format!("/containers/{ours}/json")),
                (
                    "GET",
                    format!("/containers/{ours}/logs?stdout=true&stderr=false&{logs}"),
                ),
                (
                    "GET",
                    format!("/containers/{ours}/logs?stdout=false&stderr=true&{logs}"),
                ),
                ("DELETE", format!("/containers/{ours}?force=true")),
            ] {
                assert!(allowed_sweep(method, &path, &known), "{method} {path}");
                // The same verb on a container we neither created nor listed by label.
                let other = path.replace(&ours, &stranger);
                if other != path {
                    assert!(!allowed_sweep(method, &other, &known), "{method} {other}");
                }
            }
            let short = &ours[..12];
            for (method, path) in [
                ("GET", "/containers/json?all=true".to_owned()),
                ("GET", "/containers/json?all=true&filters=%7B%7D".to_owned()),
                ("POST", "/containers/create?name=x".to_owned()),
                ("POST", format!("/containers/{short}/start")),
                ("POST", "/containers/noevia-llama/kill".to_owned()),
                ("POST", format!("/containers/{ours}/restart")),
                ("POST", format!("/containers/{ours}/exec")),
                ("POST", format!("/containers/{ours}/stop")),
                ("DELETE", format!("/containers/{ours}")),
                ("DELETE", format!("/containers/{ours}?force=true&v=true")),
                (
                    "GET",
                    format!("/containers/{ours}/logs?stdout=true&stderr=true&{logs}"),
                ),
                (
                    "GET",
                    format!("/containers/{ours}/logs?stdout=true&stderr=false&follow=true"),
                ),
                ("POST", format!("/containers/{ours}/json")),
                ("GET", "/images/json".to_owned()),
                ("POST", "/images/create".to_owned()),
                ("POST", "/volumes/create".to_owned()),
                (
                    "DELETE",
                    format!("/containers/{}?force=true", ours.to_uppercase()),
                ),
            ] {
                assert!(!allowed_sweep(method, &path, &known), "{method} {path}");
            }
        }
    }
}
