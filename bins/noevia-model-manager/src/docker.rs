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
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| "docker unreachable")?;
        let (mut sender, connection) = http1::Builder::new()
            .max_buf_size(8192)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(|_| "Docker protocol refused")?;
        // Driver belongs to this request and is aborted on EVERY exit, including timeout.
        let driver = AbortDriver(tokio::spawn(async move {
            let _ = connection.await;
        }));
        let request = Request::builder()
            .method("GET")
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
        if status.is_redirection() {
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
        drop(driver);
        Ok((status, raw))
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
struct AbortDriver(tokio::task::JoinHandle<()>);
impl Drop for AbortDriver {
    fn drop(&mut self) {
        self.0.abort();
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
