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
pub struct Context {
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
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| Error::Refused)?;
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
