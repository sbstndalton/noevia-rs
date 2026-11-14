//! core machine-health.cjs (#1638, #1851, #1856, #1861): is an assigned service answering? One GET of its health
//! endpoint, bounded and cached, so a busy chat (or a Settings refresh) never turns into a probe storm:
//! - one probe per address at a time (later callers share the one in flight);
//! - a result is reused for the TTL (30 s), a failure as well;
//! - each probe has a short deadline and reads no body;
//! - the cache holds at most [`MAX_ENTRIES`] keys, oldest dropped first; [`HealthCache::remember`]'s facts share it.
//!
//! Every request goes through a [`Fetch`]: [`NetFetch`] is the on-this-network client (model_stream::net), so a
//! name that now resolves to a public address is refused at connect time and reads "unreachable".
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::OnceCell;

pub const TTL_MS: u64 = 30_000;
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);
pub const MAX_ENTRIES: usize = 32;

/// Why a request to a machine got no answer (Node's error codes, as machine-health.cjs and chat-machines.cjs read them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// `EPUBLICADDR`: the on-this-network rule refused the address (literal or resolved).
    NotOnThisNetwork,
    /// `ECONNREFUSED`.
    Refused,
    /// `TimeoutError` / `AbortError` / `ETIMEDOUT` / `UND_ERR_CONNECT_TIMEOUT`.
    Timeout,
    /// `ENOTFOUND` / `EAI_AGAIN`.
    NotFound,
    /// A redirect status (`redirect: 'error'`): never followed.
    Redirect,
    /// Anything else (a bad URL, a reset, TLS).
    Network,
}

/// The body could not be read within its cap (`tooLarge`), or the read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    TooLarge,
    Failed,
}

/// An answer: its status and, when asked for and the status is 2xx, its body as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Option<Result<String, BodyError>>,
}

impl Reply {
    /// `response.ok`.
    pub fn ok(&self) -> bool {
        (200..=299).contains(&self.status)
    }
}

/// A body read whatever the status (http.cjs `readCappedText`): at most `cap` bytes, and whether more were on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capped {
    pub bytes: Vec<u8>,
    /// More than `cap` bytes were on offer (the rest is not read).
    pub capped: bool,
}

/// The answer to [`Fetch::send`]: its status and its body, read for every status. `Err(Failed)`: the read broke
/// (a reset, or the deadline passed while it ran).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub status: u16,
    pub body: Result<Capped, BodyError>,
}

impl Sent {
    /// `response.ok`.
    pub fn ok(&self) -> bool {
        (200..=299).contains(&self.status)
    }
}

/// One GET to a machine. `body_cap: None` reads no body (a probe); `Some(cap)` reads a 2xx body up to `cap` bytes.
/// Implementations never follow a redirect and never send this machine's keys; `headers` are the caller's own.
pub trait Fetch: Send + Sync {
    fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        body_cap: Option<usize>,
    ) -> impl Future<Output = Result<Reply, FetchError>> + Send;

    /// One request with any method and an optional body (the router classifier's JSON, the image probe), under the
    /// same rules as [`Fetch::get`]; the answer's body is read for every status, up to `cap` bytes. A fetch that only
    /// reads (a test script) keeps this default: no answer.
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<String>,
        timeout: Duration,
        cap: usize,
    ) -> impl Future<Output = Result<Sent, FetchError>> + Send {
        let _ = (method, url, headers, body, timeout, cap);
        async { Err(FetchError::Network) }
    }
}

impl<F: Fetch> Fetch for Arc<F> {
    fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        body_cap: Option<usize>,
    ) -> impl Future<Output = Result<Reply, FetchError>> + Send {
        (**self).get(url, headers, timeout, body_cap)
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<String>,
        timeout: Duration,
        cap: usize,
    ) -> impl Future<Output = Result<Sent, FetchError>> + Send {
        (**self).send(method, url, headers, body, timeout, cap)
    }
}

/// The probe states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Up,
    Down,
    Unreachable,
    /// No health URL could be made from the address.
    Unknown,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Up => "up",
            State::Down => "down",
            State::Unreachable => "unreachable",
            State::Unknown => "unknown",
        }
    }
}

/// A probe's result: `{ state, reason?, checkedAt? }` (`checked_at_ms`: when it was taken, epoch ms; none for
/// `unknown`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub state: State,
    pub reason: Option<String>,
    pub checked_at_ms: Option<u64>,
}

impl Probe {
    fn of(state: State, reason: Option<&str>) -> Probe {
        Probe {
            state,
            reason: reason.map(str::to_owned),
            checked_at_ms: None,
        }
    }
}

/// core `healthUrl(base, path)`: `path` at the base URL's origin; `None` for anything not http(s).
pub fn health_url(base: &str, path: &str) -> Option<String> {
    let base = crate::machine_turn::js_trim(base);
    let u = reqwest::Url::parse(base).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?;
    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}{path}", u.scheme()))
}

/// The answer a probe's GET gives, as machine-health.cjs `probeUrl` reads it.
pub fn probe_result(answer: Result<u16, FetchError>, sent_headers: bool) -> Probe {
    match answer {
        Ok(status) if (200..=299).contains(&status) => Probe::of(State::Up, None),
        // It answered and wants a login: it is there. Only a key we sent and it refused is a fault.
        Ok(401 | 403) if !sent_headers => Probe::of(State::Up, Some("needs-auth")),
        Ok(status) => Probe::of(State::Down, Some(format!("http-{status}").as_str())),
        Err(e) => Probe::of(
            State::Unreachable,
            Some(match e {
                FetchError::NotOnThisNetwork => "not-on-this-network",
                FetchError::Refused => "refused",
                FetchError::Timeout => "timeout",
                FetchError::NotFound => "not-found",
                FetchError::Redirect | FetchError::Network => "network",
            }),
        ),
    }
}

/// core `machineState(states)`: the worst one (`idle` when nothing on it is probed).
pub fn machine_state(states: &[Probe]) -> &'static str {
    let list: Vec<&Probe> = states
        .iter()
        .filter(|s| s.state != State::Unknown)
        .collect();
    if list.is_empty() {
        "idle"
    } else if list.iter().any(|s| s.state == State::Unreachable) {
        "unreachable"
    } else if list.iter().any(|s| s.state == State::Down) {
        "down"
    } else {
        "up"
    }
}

fn worst<'a, T>(list: &'a [(T, &'a Probe)]) -> Option<&'a (T, &'a Probe)> {
    list.iter()
        .find(|(_, p)| p.state == State::Unreachable)
        .or_else(|| list.iter().find(|(_, p)| p.state == State::Down))
}

/// core `machineReason(states)`: the reason of the worst probed service (unreachable, then down), or ''.
pub fn machine_reason(states: &[Probe]) -> String {
    let list: Vec<((), &Probe)> = states
        .iter()
        .filter(|s| s.state != State::Unknown)
        .map(|s| ((), s))
        .collect();
    worst(&list)
        .and_then(|(_, p)| p.reason.clone())
        .unwrap_or_default()
}

/// core `machineFailing(entries)`: the service behind [`machine_reason`] (same pick), or ''.
pub fn machine_failing(entries: &[(String, Probe)]) -> String {
    let list: Vec<(&str, &Probe)> = entries
        .iter()
        .filter(|(_, p)| p.state != State::Unknown)
        .map(|(s, p)| (s.as_str(), p))
        .collect();
    worst(&list)
        .map(|(s, _)| (*s).to_owned())
        .unwrap_or_default()
}

/// What [`HealthCache::remember`] keeps beside the probes (#1851): a machine's facts.
pub type Facts = crate::machine_turn::MachineFacts;

#[derive(Clone)]
enum Value {
    Probe(Probe),
    Facts(Option<Facts>),
}

enum Slot {
    /// In flight: later callers wait on the same cell. `last` is the value it replaces (remember's `prev`).
    Pending(Arc<OnceCell<Value>>, Option<Value>),
    Done {
        at: u64,
        value: Value,
    },
}

/// Epoch milliseconds (`Date.now`); tests inject their own.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
}

/// Options of [`HealthCache::probe`] (`{ headers, path, fallbackPath }`; the decision-service fetch is not here).
#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    pub headers: Vec<(String, String)>,
    /// Default `/health`.
    pub path: Option<String>,
    /// #1861: asked when the health path answers 404 (an OpenAI-style server has no /health).
    pub fallback_path: Option<String>,
}

/// core `createMachineHealth`: the shared probe and facts cache.
pub struct HealthCache<F> {
    fetch: F,
    ttl_ms: u64,
    timeout: Duration,
    max_entries: usize,
    now: Clock,
    // Insertion order is Map order: a key set again moves to the end; the oldest is dropped first.
    slots: Mutex<Vec<(String, Slot)>>,
}

impl<F: Fetch> HealthCache<F> {
    pub fn new(fetch: F) -> Self {
        Self::with(fetch, TTL_MS, PROBE_TIMEOUT, MAX_ENTRIES, system_clock())
    }

    pub fn with(fetch: F, ttl_ms: u64, timeout: Duration, max_entries: usize, now: Clock) -> Self {
        HealthCache {
            fetch,
            ttl_ms,
            timeout,
            max_entries,
            now,
            slots: Mutex::new(Vec::new()),
        }
    }

    pub fn fetch(&self) -> &F {
        &self.fetch
    }

    /// `clear()`.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// How many keys the cache holds now (tests).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, Slot)>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The fresh value under `key`, or the cell to wait on, or a new cell this caller fills (with the last value).
    fn enter(&self, key: &str) -> Result<Value, (Arc<OnceCell<Value>>, Option<Value>)> {
        let mut slots = self.lock();
        let now = (self.now)();
        let mut last = None;
        if let Some((_, slot)) = slots.iter().find(|(k, _)| k == key) {
            match slot {
                Slot::Pending(cell, last) => return Err((cell.clone(), last.clone())),
                Slot::Done { at, value } if now.saturating_sub(*at) < self.ttl_ms => {
                    return Ok(value.clone())
                }
                Slot::Done { value, .. } => last = Some(value.clone()),
            }
        }
        // `cache.set(key, { pending })`: an existing key keeps its place, a new one goes last; the cap is applied
        // only when a value settles.
        let cell = Arc::new(OnceCell::new());
        let pending = Slot::Pending(cell.clone(), last.clone());
        match slots.iter_mut().find(|(k, _)| k == key) {
            Some((_, slot)) => *slot = pending,
            None => slots.push((key.to_owned(), pending)),
        }
        Err((cell, last))
    }

    /// `cache.delete(key); cache.set(key, { at, value })`, then the size cap.
    fn settle(&self, key: &str, value: Value) {
        let mut slots = self.lock();
        let at = (self.now)();
        slots.retain(|(k, _)| k != key);
        slots.push((key.to_owned(), Slot::Done { at, value }));
        self.trim(&mut slots);
    }

    fn trim(&self, slots: &mut Vec<(String, Slot)>) {
        while slots.len() > self.max_entries {
            slots.remove(0);
        }
    }

    async fn probe_url(&self, url: &str, headers: &[(String, String)]) -> Probe {
        let answer = self
            .fetch
            .get(url, headers, self.timeout, None)
            .await
            .map(|r| r.status);
        probe_result(answer, !headers.is_empty())
    }

    /// core `probe(base, { headers, path, fallbackPath })`.
    pub async fn probe(&self, base: &str, opts: &ProbeOptions) -> Probe {
        let path = opts.path.as_deref().unwrap_or("/health");
        let Some(url) = health_url(base, path) else {
            return Probe::of(State::Unknown, None);
        };
        let fallback = opts
            .fallback_path
            .as_deref()
            .and_then(|p| health_url(base, p));
        let (cell, _) = match self.enter(&url) {
            Ok(Value::Probe(p)) => return p,
            // A facts key never looks like a URL with a path; treat a clash as a miss.
            Ok(Value::Facts(_)) => (Arc::new(OnceCell::new()), None),
            Err(pending) => pending,
        };
        let value = cell
            .get_or_init(|| async {
                let first = self.probe_url(&url, &opts.headers).await;
                let not_found =
                    first.state == State::Down && first.reason.as_deref() == Some("http-404");
                let result = match &fallback {
                    Some(f) if not_found => self.probe_url(f, &opts.headers).await,
                    _ => first,
                };
                let out = Probe {
                    checked_at_ms: Some((self.now)()),
                    ..result
                };
                self.settle(&url, Value::Probe(out.clone()));
                Value::Probe(out)
            })
            .await;
        match value {
            Value::Probe(p) => p.clone(),
            Value::Facts(_) => Probe::of(State::Unknown, None),
        }
    }

    /// core `remember(key, load)`: facts kept beside the health results (same TTL, one in flight, same cap).
    /// `load` gets the last known value so a failed read can keep it. `None`: never known.
    pub async fn remember<L, Fut>(&self, key: &str, load: L) -> Option<Facts>
    where
        L: FnOnce(Option<Facts>) -> Fut,
        Fut: Future<Output = Option<Facts>>,
    {
        let (cell, last) = match self.enter(key) {
            Ok(Value::Facts(v)) => return v,
            Ok(Value::Probe(_)) => (Arc::new(OnceCell::new()), None),
            Err(pending) => pending,
        };
        let last = match last {
            Some(Value::Facts(v)) => v,
            _ => None,
        };
        let value = cell
            .get_or_init(|| async {
                let value = load(last).await;
                self.settle(key, Value::Facts(value.clone()));
                Value::Facts(value)
            })
            .await;
        match value {
            Value::Facts(v) => v.clone(),
            Value::Probe(_) => None,
        }
    }
}

/// The on-this-network fetch (core service-endpoint.cjs serviceFetch with `redirect: 'error'`):
/// [`model_stream::net::this_network_client`] for names, [`model_stream::net::url_allowed`] for literals.
#[derive(Clone)]
pub struct NetFetch {
    client: reqwest::Client,
}

impl NetFetch {
    pub fn new() -> Result<Self, String> {
        Ok(NetFetch {
            client: model_stream::net::this_network_client()?,
        })
    }
}

fn classify(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        return FetchError::Timeout;
    }
    if model_stream::net::refused_in_chain(e) {
        return FetchError::NotOnThisNetwork;
    }
    let mut at: Option<&(dyn std::error::Error + 'static)> =
        Some(e as &(dyn std::error::Error + 'static));
    while let Some(err) = at {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => return FetchError::Refused,
                std::io::ErrorKind::TimedOut => return FetchError::Timeout,
                _ => {}
            }
        }
        if err.to_string().contains("failed to lookup address") {
            return FetchError::NotFound;
        }
        at = err.source();
    }
    FetchError::Network
}

/// A literal off this network, a login in the URL, or not http(s): Node throws before connecting.
fn url_refusal(url: &str) -> Result<(), FetchError> {
    if model_stream::net::url_allowed(url) {
        return Ok(());
    }
    Err(match reqwest::Url::parse(url) {
        Ok(u)
            if matches!(u.scheme(), "http" | "https")
                && u.username().is_empty()
                && u.password().is_none() =>
        {
            FetchError::NotOnThisNetwork
        }
        _ => FetchError::Network,
    })
}

impl Fetch for NetFetch {
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
        timeout: Duration,
        body_cap: Option<usize>,
    ) -> Result<Reply, FetchError> {
        url_refusal(url)?;
        let mut req = self.client.get(url).timeout(timeout);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let mut resp = req.send().await.map_err(|e| classify(&e))?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(FetchError::Redirect);
        }
        let ok = (200..=299).contains(&status);
        let body = match body_cap {
            Some(cap) if ok => {
                let mut buf: Vec<u8> = Vec::new();
                let mut out = None;
                loop {
                    match resp.chunk().await {
                        Ok(Some(chunk)) => {
                            if buf.len() + chunk.len() > cap {
                                out = Some(Err(BodyError::TooLarge));
                                break;
                            }
                            buf.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(e) if e.is_timeout() => return Err(FetchError::Timeout),
                        Err(_) => {
                            out = Some(Err(BodyError::Failed));
                            break;
                        }
                    }
                }
                Some(out.unwrap_or_else(|| Ok(String::from_utf8_lossy(&buf).into_owned())))
            }
            _ => None,
        };
        Ok(Reply { status, body })
    }

    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<String>,
        timeout: Duration,
        cap: usize,
    ) -> Result<Sent, FetchError> {
        url_refusal(url)?;
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| FetchError::Network)?;
        let mut req = self.client.request(method, url).timeout(timeout);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(body) = body {
            req = req.body(body);
        }
        let mut resp = req.send().await.map_err(|e| classify(&e))?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Err(FetchError::Redirect);
        }
        let mut bytes: Vec<u8> = Vec::new();
        let body = loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let room = cap.saturating_sub(bytes.len());
                    if chunk.len() > room {
                        bytes.extend_from_slice(chunk.get(..room).unwrap_or_default());
                        break Ok(Capped {
                            bytes,
                            capped: true,
                        });
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => {
                    break Ok(Capped {
                        bytes,
                        capped: false,
                    })
                }
                Err(_) => break Err(BodyError::Failed),
            }
        };
        Ok(Sent { status, body })
    }
}
