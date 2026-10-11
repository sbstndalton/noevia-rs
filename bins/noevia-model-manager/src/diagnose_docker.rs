//! Closed current-run read capability and retained connection-driver ownership.
use crate::{diagnose, docker::UnixDocker, Container};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{client::conn::http1, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio::{net::UnixStream, sync::Notify, task::JoinHandle};

pub const METADATA_CAP: usize = 1024 * 1024;
pub const INVENTORY_CAP: usize = 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Ordinary,
    Refused,
    Unknown,
}
pub type ReadJob<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, Error>> + Send + 'a>>;
pub trait DiagnosisRead: Send + Sync {
    fn inventory<'a>(&'a self, context: &'a Context) -> ReadJob<'a, Vec<Container>>;
    fn current_logs<'a>(&'a self, name: &'a str, context: &'a Context) -> ReadJob<'a, Vec<u8>>;
}
/// All spawned drivers enter this owner synchronously, before the next await.
/// The retained controller calls cleanup and observes every join before releasing leases.
#[cfg(all(test, target_os = "linux"))]
#[derive(Default)]
struct WireWitness {
    read_kind: std::sync::atomic::AtomicU8,
    read_errno: std::sync::atomic::AtomicI32,
    write_kind: std::sync::atomic::AtomicU8,
    write_errno: std::sync::atomic::AtomicI32,
    response_bytes: AtomicBool,
    hyper_flags: std::sync::atomic::AtomicU8,
}
#[cfg(all(test, target_os = "linux"))]
impl WireWitness {
    fn io(&self, error: &std::io::Error, write: bool) {
        let (kind, errno) = if write {
            (&self.write_kind, &self.write_errno)
        } else {
            (&self.read_kind, &self.read_errno)
        };
        let value = match error.kind() {
            std::io::ErrorKind::ConnectionReset => 1,
            std::io::ErrorKind::BrokenPipe => 2,
            _ => 3,
        };
        if kind
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            errno.store(error.raw_os_error().unwrap_or(0), Ordering::Release);
        }
    }
    fn known_reset(&self) -> bool {
        [
            (
                self.read_kind.load(Ordering::Acquire),
                self.read_errno.load(Ordering::Acquire),
            ),
            (
                self.write_kind.load(Ordering::Acquire),
                self.write_errno.load(Ordering::Acquire),
            ),
        ]
        .iter()
        .any(|pair| matches!(pair, (1, 104) | (2, 32)))
    }
}
pub struct Context {
    #[cfg(all(test, target_os = "linux"))]
    witness: Arc<WireWitness>,
    pub deadline: Instant,
    pub cancelled: AtomicBool,
    wake: Notify,
    drivers: Mutex<Vec<Arc<DriverSlot>>>,
}
/// The handle stays in Context even if an injected reader cancels its cleanup future.
struct DriverSlot {
    handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}
impl Context {
    pub fn new(deadline: Instant) -> Self {
        Self {
            #[cfg(all(test, target_os = "linux"))]
            witness: Arc::new(WireWitness::default()),
            deadline,
            cancelled: AtomicBool::new(false),
            wake: Notify::new(),
            drivers: Mutex::new(Vec::new()),
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_one();
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.wake.notified();
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    pub fn check(&self) -> Result<(), Error> {
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            Err(Error::Ordinary)
        } else {
            Ok(())
        }
    }
    pub fn own_driver(
        &self,
        driver: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), Error> {
        let mut drivers = self.drivers.lock().map_err(|_| Error::Refused)?;
        if drivers.len() == 3075 {
            return Err(Error::Refused);
        }
        drivers.try_reserve(1).map_err(|_| Error::Refused)?;
        drivers.push(Arc::new(DriverSlot {
            handle: tokio::sync::Mutex::new(Some(tokio::spawn(driver))),
        }));
        Ok(())
    }
    pub async fn cleanup(&self) {
        loop {
            let slot = self
                .drivers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .first()
                .cloned();
            let Some(slot) = slot else { return };
            let mut handle = slot.handle.lock().await;
            if let Some(driver) = handle.as_mut() {
                driver.abort();
                let _ = driver.await;
                // No await separates observed join from removal of the retained handle.
                *handle = None;
            }
            self.drivers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|registered| !Arc::ptr_eq(registered, &slot));
        }
    }
}
/// No caller supplied URL/query/method is representable.
pub(crate) enum Read<'a> {
    Inventory,
    Inspect(&'a str),
    Image(&'a str),
    Logs { id: &'a str, since: i64 },
}
impl Read<'_> {
    fn path(&self) -> Result<String, Error> {
        let path = match self {
            Self::Inventory => "/containers/json?all=true&limit=1025&size=false".to_owned(),
            Self::Inspect(id) => format!("/containers/{}/json", identifier(id)?),
            Self::Image(id) => format!("/images/{}/json", identifier(id)?),
            Self::Logs { id, since } => format!("/containers/{}/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000&since={since}",identifier(id)?),
        };
        if path.len() > 8192 {
            return Err(Error::Refused);
        }
        Ok(path)
    }
    fn cap(&self) -> usize {
        if matches!(self, Self::Logs { .. }) {
            diagnose::LOG_CAP
        } else {
            METADATA_CAP
        }
    }
}
pub fn identifier(value: &str) -> Result<&str, Error> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        Err(Error::Refused)
    } else {
        Ok(value)
    }
}
/// Observes bytes read from the owned peer without interpreting or reflecting them.
struct ObservedUnix {
    #[cfg(all(test, target_os = "linux"))]
    witness: Arc<WireWitness>,
    socket: UnixStream,
    response_bytes: Arc<AtomicBool>,
}
fn observe_response_bytes(observed: &AtomicBool, before: usize, after: usize) {
    if after > before {
        observed.store(true, Ordering::Release);
    }
}
impl tokio::io::AsyncRead for ObservedUnix {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        let result = std::pin::Pin::new(&mut this.socket).poll_read(cx, buffer);
        observe_response_bytes(&this.response_bytes, before, buffer.filled().len());
        #[cfg(all(test, target_os = "linux"))]
        {
            if buffer.filled().len() > before {
                this.witness.response_bytes.store(true, Ordering::Release);
            }
            if let std::task::Poll::Ready(Err(error)) = &result {
                this.witness.io(error, false);
            }
        }
        result
    }
}
impl tokio::io::AsyncWrite for ObservedUnix {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        #[cfg(not(all(test, target_os = "linux")))]
        {
            std::pin::Pin::new(&mut self.get_mut().socket).poll_write(cx, bytes)
        }
        #[cfg(all(test, target_os = "linux"))]
        {
            let this = self.get_mut();
            let result = std::pin::Pin::new(&mut this.socket).poll_write(cx, bytes);
            if let std::task::Poll::Ready(Err(error)) = &result {
                this.witness.io(error, true);
            }
            result
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().socket).poll_shutdown(cx)
    }
}
/// Only typed, known I/O causes before any response byte can be ordinary.
/// Unknown causes, bounded-chain exhaustion and protocol errors fail closed.
fn pre_response_error(
    error: &(dyn std::error::Error + 'static),
    observed: bool,
    protocol_error: bool,
) -> Error {
    if observed || protocol_error {
        return Error::Refused;
    }
    let mut source = Some(error);
    for _ in 0..8 {
        let Some(error) = source else {
            return Error::Refused;
        };
        if error
            .downcast_ref::<hyper::Error>()
            .is_some_and(|error| error.is_parse() || error.is_incomplete_message())
        {
            return Error::Refused;
        }
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            return if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::TimedOut
            ) {
                Error::Ordinary
            } else {
                Error::Refused
            };
        }
        source = error.source();
    }
    Error::Refused
}
pub(crate) async fn read(
    socket: &Path,
    operation: Read<'_>,
    context: &Context,
) -> Result<(StatusCode, Vec<u8>), Error> {
    context.check()?;
    let path = operation.path()?;
    let cap = operation.cap();
    let socket = UnixStream::connect(socket)
        .await
        .map_err(|_| Error::Ordinary)?;
    let response_bytes = Arc::new(AtomicBool::new(false));
    let socket = ObservedUnix {
        #[cfg(all(test, target_os = "linux"))]
        witness: Arc::clone(&context.witness),
        socket,
        response_bytes: Arc::clone(&response_bytes),
    };
    let (mut sender, connection) = http1::Builder::new()
        .max_buf_size(8192)
        .handshake(TokioIo::new(socket))
        .await
        .map_err(|_| Error::Refused)?;
    context.own_driver(async move {
        let _ = connection.await;
    })?;
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "localhost")
        .header("connection", "close")
        .body(Empty::<Bytes>::new())
        .map_err(|_| Error::Refused)?;
    let response = sender.send_request(request).await.map_err(|error| {
        #[cfg(all(test, target_os = "linux"))]
        context.witness.hyper_flags.store(
            4 | u8::from(error.is_parse()) | (u8::from(error.is_incomplete_message()) << 1),
            Ordering::Release,
        );
        pre_response_error(
            &error,
            response_bytes.load(Ordering::Acquire),
            error.is_parse() || error.is_incomplete_message(),
        )
    })?;
    if response.headers().len() > 32
        || response
            .headers()
            .iter()
            .map(|(key, value)| key.as_str().len().saturating_add(value.as_bytes().len()))
            .sum::<usize>()
            > 8192
    {
        return Err(Error::Refused);
    }
    let headers = response.headers();
    if headers.get_all("content-length").iter().count() > 1
        || headers.get_all("transfer-encoding").iter().count() > 1
        || (headers.contains_key("content-length") && headers.contains_key("transfer-encoding"))
    {
        return Err(Error::Refused);
    }
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.as_bytes() != b"chunked")
    {
        return Err(Error::Refused);
    }
    let length = if let Some(length) = headers.get("content-length") {
        let text = length.to_str().map_err(|_| Error::Refused)?;
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Refused);
        }
        let length = text.parse::<usize>().map_err(|_| Error::Refused)?;
        if length > cap {
            return Err(Error::Refused);
        }
        Some(length)
    } else {
        None
    };
    let status = response.status();
    if status.is_redirection() {
        return Err(Error::Refused);
    }
    let mut body = response.into_body();
    let mut raw = Vec::new();
    while let Some(frame) = body.frame().await {
        context.check()?;
        let frame = frame.map_err(|_| Error::Refused)?;
        let data = frame.into_data().map_err(|_| Error::Refused)?;
        if raw.len().saturating_add(data.len()) > cap {
            return Err(Error::Refused);
        }
        raw.try_reserve(data.len()).map_err(|_| Error::Refused)?;
        raw.extend_from_slice(&data);
    }
    if length.is_some_and(|length| length != raw.len()) {
        return Err(Error::Refused);
    }
    context.check()?;
    Ok((status, raw))
}
#[derive(Deserialize)]
struct Inspect<'a> {
    #[serde(rename = "Id", borrow)]
    id: Option<&'a str>,
    #[serde(rename = "Name", borrow)]
    name: Option<&'a str>,
    #[serde(rename = "Image", borrow)]
    image: Option<&'a str>,
    #[serde(rename = "State", borrow)]
    state: Option<Started<'a>>,
    #[serde(rename = "Config")]
    config: Option<Configuration>,
}
#[derive(Deserialize)]
struct Started<'a> {
    #[serde(rename = "StartedAt", borrow)]
    started: Option<&'a str>,
}
#[derive(Deserialize)]
struct Configuration {
    #[serde(rename = "Tty")]
    tty: Option<bool>,
}
fn inspect(raw: &[u8]) -> Result<Inspect<'_>, Error> {
    let value: Inspect<'_> = serde_json::from_slice(raw).map_err(|_| Error::Refused)?;
    for text in [value.id, value.name, value.image].into_iter().flatten() {
        if text.len() > 256 {
            return Err(Error::Refused);
        }
    }
    if value
        .state
        .as_ref()
        .and_then(|state| state.started)
        .is_some_and(|text| text.len() > 128)
    {
        return Err(Error::Refused);
    }
    Ok(value)
}
#[derive(Deserialize)]
struct Summary<'a> {
    #[serde(rename = "Id", borrow)]
    id: &'a str,
}
fn inventory(raw: &[u8]) -> Result<Vec<Summary<'_>>, Error> {
    use serde::de::{SeqAccess, Visitor};
    struct Bounded;
    impl<'de> Visitor<'de> for Bounded {
        type Value = Vec<Summary<'de>>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("bounded container inventory")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut rows = Vec::new();
            while let Some(row) = sequence.next_element::<Summary<'de>>()? {
                if rows.len() == INVENTORY_CAP || row.id.len() > 256 {
                    return Err(serde::de::Error::custom("inventory refused"));
                }
                rows.try_reserve(1)
                    .map_err(|_| serde::de::Error::custom("allocation refused"))?;
                rows.push(row);
            }
            Ok(rows)
        }
    }
    let mut parser = serde_json::Deserializer::from_slice(raw);
    let rows =
        serde::Deserializer::deserialize_seq(&mut parser, Bounded).map_err(|_| Error::Refused)?;
    parser.end().map_err(|_| Error::Refused)?;
    Ok(rows)
}
/// Canonical ASCII Docker UTC only, whole-second source granularity, explicit UTC process gate.
pub fn started_seconds(value: &str) -> Result<i64, Error> {
    let Some(value) = value.strip_suffix('Z') else {
        return Err(Error::Ordinary);
    };
    let (whole, fraction) = value
        .split_once('.')
        .map_or((value, None), |(a, b)| (a, Some(b)));
    if whole.len() != 19
        || !whole.is_ascii()
        || fraction.is_some_and(|fraction| {
            fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(Error::Ordinary);
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(whole, "%Y-%m-%dT%H:%M:%S")
        .map_err(|_| Error::Ordinary)?;
    if parsed.format("%Y-%m-%dT%H:%M:%S").to_string() != whole
        || whole.starts_with("0000-")
        || whole.starts_with("0001-")
    {
        return Err(Error::Ordinary);
    }
    Ok(parsed.and_utc().timestamp())
}
pub fn decode_frames(raw: &[u8], tty: bool) -> Result<Vec<u8>, Error> {
    if raw.len() > diagnose::LOG_CAP {
        return Err(Error::Refused);
    }
    let mut decoded = Vec::new();
    decoded.try_reserve(raw.len()).map_err(|_| Error::Refused)?;
    if tty {
        decoded.extend_from_slice(raw);
        return Ok(decoded);
    }
    let mut remaining = raw;
    while !remaining.is_empty() {
        let header = remaining.get(..8).ok_or(Error::Refused)?;
        if !header.first().is_some_and(|kind| matches!(kind, 1 | 2))
            || header.get(1..4) != Some(&[0, 0, 0][..])
        {
            return Err(Error::Refused);
        }
        let bytes: [u8; 4] = header
            .get(4..8)
            .ok_or(Error::Refused)?
            .try_into()
            .map_err(|_| Error::Refused)?;
        let length = usize::try_from(u32::from_be_bytes(bytes)).map_err(|_| Error::Refused)?;
        if decoded.len().saturating_add(length) > diagnose::LOG_CAP {
            return Err(Error::Refused);
        }
        let end = 8usize.checked_add(length).ok_or(Error::Refused)?;
        decoded.extend_from_slice(remaining.get(8..end).ok_or(Error::Refused)?);
        remaining = remaining.get(end..).ok_or(Error::Refused)?;
    }
    Ok(decoded)
}
impl DiagnosisRead for UnixDocker {
    fn inventory<'a>(&'a self, context: &'a Context) -> ReadJob<'a, Vec<Container>> {
        Box::pin(async move {
            let (status, raw) = self.diagnosis_read(Read::Inventory, context).await?;
            if status != StatusCode::OK {
                return Err(Error::Ordinary);
            }
            let rows = inventory(&raw)?;
            let mut containers = Vec::new();
            containers
                .try_reserve(rows.len())
                .map_err(|_| Error::Refused)?;
            for row in rows {
                context.check()?;
                let (status, raw) = match self.diagnosis_read(Read::Inspect(row.id), context).await
                {
                    Ok(response) => response,
                    Err(Error::Ordinary) => continue,
                    Err(error) => return Err(error),
                };
                if status != StatusCode::OK {
                    continue;
                }
                let metadata = inspect(&raw)?;
                let name = metadata.name.unwrap_or_default().trim_start_matches('/');
                if name == "model-loader" {
                    continue;
                }
                let Some(image) = metadata.image else {
                    continue;
                };
                let (status, raw) = match self.diagnosis_read(Read::Image(image), context).await {
                    Ok(response) => response,
                    Err(Error::Ordinary) => continue,
                    Err(error) => return Err(error),
                };
                if status != StatusCode::OK {
                    continue;
                }
                // Read only the first tag using a bounded raw metadata body. Unknown fields
                // remain bounded by ingress; JSON parser allocations are not hard RSS bounded.
                let image: serde_json::Value =
                    serde_json::from_slice(&raw).map_err(|_| Error::Refused)?;
                let tags = image.get("RepoTags").and_then(serde_json::Value::as_array);
                if tags.is_some_and(|tags| tags.len() > 1024) {
                    return Err(Error::Refused);
                }
                let tag = tags
                    .and_then(|tags| tags.first())
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if tag.len() > 4096 {
                    return Err(Error::Refused);
                }
                if tag.to_lowercase().contains("llama.cpp") {
                    containers.push(Container {
                        name: name.to_owned(),
                        first_image_tag: tag.to_owned(),
                    });
                }
            }
            Ok(containers)
        })
    }
    fn current_logs<'a>(&'a self, name: &'a str, context: &'a Context) -> ReadJob<'a, Vec<u8>> {
        Box::pin(async move {
            let (status, before_raw) = self.diagnosis_read(Read::Inspect(name), context).await?;
            if status != StatusCode::OK {
                return Err(Error::Ordinary);
            }
            let before = inspect(&before_raw)?;
            let id = before.id.ok_or(Error::Ordinary)?;
            let started = before
                .state
                .as_ref()
                .and_then(|state| state.started)
                .ok_or(Error::Ordinary)?;
            let tty = before
                .config
                .as_ref()
                .and_then(|config| config.tty)
                .unwrap_or(false);
            let since = started_seconds(started)?;
            let (status, logs) = self
                .diagnosis_read(Read::Logs { id, since }, context)
                .await?;
            if status != StatusCode::OK {
                return Err(Error::Ordinary);
            }
            let (status, after_raw) = self.diagnosis_read(Read::Inspect(id), context).await?;
            if status != StatusCode::OK {
                return Err(Error::Ordinary);
            }
            let after = inspect(&after_raw)?;
            if after.id != Some(id)
                || after.state.as_ref().and_then(|state| state.started) != Some(started)
                || after
                    .config
                    .as_ref()
                    .and_then(|config| config.tty)
                    .unwrap_or(false)
                    != tty
            {
                return Err(Error::Ordinary);
            }
            decode_frames(&logs, tty)
        })
    }
}

pub enum Outcome {
    Json(Vec<u8>),
    Busy,
    Unknown,
    Refused,
}
fn empty() -> Outcome {
    Outcome::Json(b"{\"failures\":[]}".to_vec())
}
struct Controller {
    context: std::sync::Arc<Context>,
    handle: JoinHandle<()>,
}
/// Two retained controllers, no queue. Finished controller joins are observed before reuse.
pub struct Diagnosis {
    reader: std::sync::Arc<dyn DiagnosisRead>,
    analyzer: diagnose::Analyzer,
    controllers: tokio::sync::Mutex<Vec<Controller>>,
    stopping: AtomicBool,
}
impl Diagnosis {
    pub fn new(reader: std::sync::Arc<dyn DiagnosisRead>) -> Result<Self, regex::Error> {
        Ok(Self {
            reader,
            analyzer: diagnose::Analyzer::new()?,
            controllers: tokio::sync::Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
        })
    }
    pub async fn request(
        self: &std::sync::Arc<Self>,
        state: std::sync::Arc<crate::StateData>,
        name: String,
        deadline: Instant,
    ) -> Outcome {
        let mut controllers = match self.controllers.try_lock() {
            Ok(controllers) => controllers,
            Err(_) => return Outcome::Busy,
        };
        if self.stopping.load(Ordering::Acquire) {
            return Outcome::Busy;
        }
        let mut position = 0;
        while position < controllers.len() {
            if controllers
                .get(position)
                .is_some_and(|controller| controller.handle.is_finished())
            {
                let controller = controllers.swap_remove(position);
                let _ = controller.handle.await;
                controller.context.cleanup().await;
            } else {
                position += 1;
            }
        }
        if controllers.len() == 2 || controllers.try_reserve(1).is_err() {
            return Outcome::Busy;
        }
        let context = std::sync::Arc::new(Context::new(deadline));
        let task_context = context.clone();
        let owner = self.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            // The task owns StateData while borrowing the existing HTTP semaphore lease.
            // Its result channel never owns cleanup or admission.
            let permit = match state.permits.try_acquire() {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = sender.send(Outcome::Busy);
                    return;
                }
            };
            // Reader polling and destructor panics stay inside this retained inner
            // task. Its actual join is observed by the outer admission owner.
            let job_state = state.clone();
            let job_context = task_context.clone();
            let mut inner = tokio::spawn(async move {
                owner
                    .analyse(&job_state.configured, &name, &job_context)
                    .await
            });
            let (mut outcome, joined) = tokio::select! {
                result = &mut inner=>(result.unwrap_or(Outcome::Refused),true),
                _ = task_context.cancelled()=>(empty(),false),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))=>(empty(),false),
            };
            if !joined {
                inner.abort();
                if let Err(error) = inner.await {
                    if error.is_panic() {
                        outcome = Outcome::Refused;
                    }
                }
            }
            // Socket/sender/body futures have dropped before cleanup. Every connection
            // driver's actual join is observed while the HTTP admission remains held.
            task_context.cleanup().await;
            drop(permit);
            let _ = sender.send(outcome);
        });
        controllers.push(Controller {
            context: context.clone(),
            handle,
        });
        drop(controllers);
        struct CancelOnDrop(std::sync::Arc<Context>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _cancel_on_drop = CancelOnDrop(context);
        tokio::select! {
            result = receiver=>result.unwrap_or(Outcome::Refused),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))=>empty(),
        }
    }
    async fn analyse(&self, configured: &[String], name: &str, context: &Context) -> Outcome {
        let inventory = match self.reader.inventory(context).await {
            Ok(inventory) => inventory,
            Err(Error::Ordinary) => Vec::new(),
            Err(_) => return Outcome::Refused,
        };
        let names = match crate::effective_names(configured, &inventory) {
            Ok(names) => names,
            Err(_) => return Outcome::Refused,
        };
        if context.check().is_err() {
            return empty();
        }
        if !names.iter().any(|candidate| candidate == name) {
            return Outcome::Unknown;
        }
        let raw = match self.reader.current_logs(name, context).await {
            Ok(raw) => raw,
            Err(Error::Ordinary) => return empty(),
            Err(_) => return Outcome::Refused,
        };
        let text = match lossy(&raw) {
            Ok(text) => text,
            Err(_) => return Outcome::Refused,
        };
        match self.analyzer.analyse_json(
            &text,
            &diagnose::Work {
                deadline: context.deadline,
                cancelled: &context.cancelled,
            },
        ) {
            Ok(json) => Outcome::Json(json),
            Err(diagnose::Refusal::Cancelled | diagnose::Refusal::Deadline) => empty(),
            Err(_) => Outcome::Refused,
        }
    }
    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let mut controllers = self.controllers.lock().await;
        for controller in controllers.iter() {
            controller.context.cancel();
        }
        for controller in std::mem::take(&mut *controllers) {
            let _ = controller.handle.await;
            controller.context.cleanup().await;
        }
    }
}
/// Replacement expansion is admitted before each retained append.
pub fn lossy(raw: &[u8]) -> Result<String, Error> {
    if raw.len() > diagnose::LOG_CAP {
        return Err(Error::Refused);
    }
    fn append(output: &mut String, text: &str) -> Result<(), Error> {
        if output.len().saturating_add(text.len()) > diagnose::LOG_CAP {
            return Err(Error::Refused);
        }
        output.try_reserve(text.len()).map_err(|_| Error::Refused)?;
        output.push_str(text);
        Ok(())
    }
    let mut output = String::new();
    let mut remaining = raw;
    loop {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                append(&mut output, text)?;
                break;
            }
            Err(error) => {
                let valid = remaining.get(..error.valid_up_to()).ok_or(Error::Refused)?;
                append(
                    &mut output,
                    std::str::from_utf8(valid).map_err(|_| Error::Refused)?,
                )?;
                append(&mut output, "\u{fffd}")?;
                let skip = error.valid_up_to().saturating_add(
                    error
                        .error_len()
                        .unwrap_or_else(|| remaining.len().saturating_sub(error.valid_up_to())),
                );
                remaining = remaining.get(skip..).ok_or(Error::Refused)?;
            }
        }
    }
    Ok(output)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    NotOwned,
    NotFound,
    Redirect(String),
    Item(String),
}
pub fn classify(raw: &str) -> Route {
    // ASGI decodes the complete path once using UTF-8 replacement. Malformed percent
    // triplets remain literal, then the admitted name policy handles them on GET.
    let mut bytes = Vec::new();
    if raw.len() > 8192 || bytes.try_reserve(raw.len()).is_err() {
        return Route::NotFound;
    }
    let mut remaining = raw.as_bytes();
    while let Some((&byte, rest)) = remaining.split_first() {
        if byte == b'%' {
            let hex = remaining.get(1..3).and_then(|pair| {
                let a = char::from(*pair.first()?).to_digit(16)?;
                let b = char::from(*pair.get(1)?).to_digit(16)?;
                u8::try_from(a * 16 + b).ok()
            });
            if let Some(decoded) = hex {
                bytes.push(decoded);
                remaining = remaining.get(3..).unwrap_or_default();
                continue;
            }
        }
        bytes.push(byte);
        remaining = rest;
    }
    let decoded = String::from_utf8_lossy(&bytes);
    let Some(rest) = decoded.strip_prefix("/api/v1/backends/") else {
        return Route::NotOwned;
    };
    let (item, redirect) = if let Some(item) = rest.strip_suffix("/diagnose/") {
        (item, true)
    } else if let Some(item) = rest.strip_suffix("/diagnose") {
        (item, false)
    } else {
        return Route::NotOwned;
    };
    if item.is_empty() || item.contains('/') {
        return Route::NotFound;
    }
    if redirect {
        Route::Redirect(format!("/api/v1/backends/{item}/diagnose"))
    } else {
        Route::Item(item.to_owned())
    }
}

#[cfg(test)]
mod transport_error_tests {
    use super::*;
    #[derive(Debug)]
    struct Chain(Box<dyn std::error::Error>);
    impl std::fmt::Display for Chain {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("fixed synthetic cause")
        }
    }
    impl std::error::Error for Chain {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }
    #[test]
    fn typed_pre_response_boundary_and_observation_fail_closed() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
        ] {
            let io = std::io::Error::from(kind);
            assert_eq!(pre_response_error(&io, false, false), Error::Ordinary);
            assert_eq!(pre_response_error(&io, true, false), Error::Refused);
            assert_eq!(pre_response_error(&io, false, true), Error::Refused);
            let mut cause: Box<dyn std::error::Error> = Box::new(io);
            for _ in 0..8 {
                cause = Box::new(Chain(cause));
            }
            assert_eq!(
                pre_response_error(cause.as_ref(), false, false),
                Error::Refused
            );
        }
        for kind in [std::io::ErrorKind::UnexpectedEof, std::io::ErrorKind::Other] {
            assert_eq!(
                pre_response_error(&std::io::Error::from(kind), false, false),
                Error::Refused
            );
        }
        assert_eq!(
            pre_response_error(&std::fmt::Error, false, false),
            Error::Refused
        );
        let observed = AtomicBool::new(false);
        observe_response_bytes(&observed, 0, 0);
        assert!(!observed.load(Ordering::Acquire));
        observe_response_bytes(&observed, 0, 1);
        assert!(observed.load(Ordering::Acquire));
        observe_response_bytes(&observed, 1, 1);
        assert!(observed.load(Ordering::Acquire));
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn connected_unix_unread_request_reset_is_ordinary_after_actual_driver_join(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;
        let root = tempfile::Builder::new()
            .prefix("s1-unix-reset-")
            .tempdir_in("/tmp")?;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = root.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let context = Context::new(Instant::now() + Duration::from_secs(2));
        let deadline = tokio::time::Instant::from_std(context.deadline);
        let (queued_send, queued_receive) = tokio::sync::oneshot::channel();
        let (close_send, close_receive) = tokio::sync::oneshot::channel();
        let mut daemon = tokio::spawn(async move {
            let operation = async {
                let (peer, _) = listener.accept().await.map_err(|_| "peer accept")?;
                peer.readable().await.map_err(|_| "peer readable")?;
                let mut bytes = [0u8; 64];
                let (_, count) = rustix::net::recv(
                    &peer,
                    &mut bytes[..],
                    rustix::net::RecvFlags::PEEK | rustix::net::RecvFlags::DONTWAIT,
                )
                .map_err(|_| "peer peek")?;
                let prefix = bytes.get(..count).ok_or("peer peek count")?;
                if !prefix.starts_with(b"GET /containers/synthetic-backend/json ") {
                    return Err("peer queued GET prerequisite");
                }
                queued_send
                    .send(count)
                    .map_err(|_| "peer queue publication")?;
                close_receive.await.map_err(|_| "peer close permission")?;
                // No response write/request consumption; actual kernel error is mandatory.
                drop(peer);
                Ok::<usize, &'static str>(count)
            };
            tokio::time::timeout_at(deadline, operation)
                .await
                .map_err(|_| "peer deadline")?
        });
        let docker = UnixDocker { socket };
        let operation =
            tokio::time::timeout_at(deadline, docker.current_logs("synthetic-backend", &context));
        let release = async {
            let queued = tokio::time::timeout_at(deadline, queued_receive).await;
            let _ = close_send.send(());
            queued
        };
        let (result, queued) = tokio::join!(operation, release);
        let joined = match tokio::time::timeout_at(deadline, &mut daemon).await {
            Ok(joined) => joined,
            Err(_) => {
                daemon.abort();
                daemon.await
            }
        };
        context.cleanup().await;
        // Every assertion follows actual same-handle joins, also on error/timeout.
        let count = queued
            .map_err(|_| "queue deadline")?
            .map_err(|_| "queue absent")?;
        let peer_count = joined.map_err(|_| "peer join failed")??;
        assert_eq!(count, peer_count);
        assert!(count > 0);
        assert!(context
            .drivers
            .lock()
            .map_err(|_| "driver registry poisoned")?
            .is_empty());
        assert!(
            context.witness.known_reset(),
            "actual Linux typed reset/broken-pipe prerequisite"
        );
        assert!(!context.witness.response_bytes.load(Ordering::Acquire));
        assert_eq!(context.witness.hyper_flags.load(Ordering::Acquire), 4);
        assert_eq!(
            result.map_err(|_| "subject deadline")?,
            Err(Error::Ordinary)
        );
        Ok(())
    }
}
