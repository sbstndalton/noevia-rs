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
    // cancellation), the driver is only aborted; that drop path observes no join.
    let driver = AbortDriver::spawn(connection);
    let result = exchange(&mut sender, method, path).await;
    driver.finish().await;
    result
}

type Sender = http1::SendRequest<Empty<Bytes>>;

async fn exchange(
    sender: &mut Sender,
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

/// Owns one connection task. `finish` aborts it and observes its join. Dropping it without
/// `finish` (the request future was cancelled) only requests the abort.
struct AbortDriver<T>(Option<tokio::task::JoinHandle<T>>);
impl<T: Send + 'static> AbortDriver<T> {
    fn spawn<C>(connection: C) -> Self
    where
        C: std::future::Future<Output = T> + Send + 'static,
    {
        Self(Some(spawn_driver(connection)))
    }
    async fn finish(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
impl<T> Drop for AbortDriver<T> {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
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
}
