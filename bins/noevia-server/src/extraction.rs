//! The extraction engine of the front (sbstndalton/noevia#2191 round 5, `rust-document-extraction`):
//! core docling.cjs and ocr.cjs's clients over HTTP, and the worker that drains each user's durable
//! extraction queue (crates/server-projects `extract_queue`).
//!
//! Where the workers are (document-workers.cjs `resolve`): the machine Settings → Inference machines
//! assigned the service to (cowork.db `settings` key `inference:machines`, read at call time), else
//! the environment (`DOCLING_BASE_URL`, `OCR_BASE_URL`). A machine address is connected to only when
//! every address its name resolves to is on this network (loopback, private, link-local, Tailscale's
//! 100.64/10, ULA), as service-endpoint.cjs `serviceFetch` requires; no redirect is ever followed and
//! no credential is sent. The requests carry what core's carry: the bytes and their content type,
//! `X-Document-Name` (the last path segment, Latin-1 or percent-encoded), `X-Document-Figures`,
//! `X-Document-Pages`, `X-OCR-Pages`, `X-OCR-DPI`; the deadlines (an hour and five minutes for a
//! whole document, two minutes plus a minute a page for a range, 610 s for OCR, 10 s for `/health`);
//! the reply caps (32 MiB Docling, 16 MiB OCR, 64 KiB health); the `/health` answers kept five minutes.
use js_json::{wtf16, JValue};
use server_projects::extract::{Engine, EngineError, Limits, OcrCaps};
use server_projects::Ctx;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DOCLING_REPLY_CAP: usize = 32 * 1024 * 1024;
const OCR_REPLY_CAP: usize = 16 * 1024 * 1024;
const HEALTH_CAP: usize = 64 * 1024;
const CAPABILITY_TTL: Duration = Duration::from_secs(5 * 60);
/// How often the worker looks for due jobs.
pub const TICK: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct Cached<T> {
    at: Instant,
    url: String,
    value: T,
}

/// docling.cjs `rangesHealth`: `(ranges, rangedMaxBytes)`, `None` when it could not be asked.
type Ranges = Option<(bool, Option<usize>)>;

/// The HTTP engine. Blocking: every call runs on the blocking pool, through `rt`.
pub struct HttpEngine {
    rt: tokio::runtime::Handle,
    client: reqwest::Client,
    data_dir: Option<PathBuf>,
    ranges: Mutex<Option<Cached<Ranges>>>,
    caps: Mutex<Option<Cached<Option<OcrCaps>>>>,
    waits: Vec<Duration>,
    /// How often a request that is waiting for its answer asks `/health` whether the service is
    /// still there, and how many asks in a row may go unanswered before the request is given up.
    liveness: (Duration, u32),
}

/// A request to the document service has no answer until the whole document is read, and a large
/// scan can take many minutes with the service healthy (its own limit is an hour), so no total
/// deadline of minutes can tell slow from stuck. While such a request waits, `/health` is asked every
/// [`LIVENESS_EVERY`]; [`LIVENESS_MISSES`] asks in a row without a 2xx answer (5 minutes of silence)
/// end the wait. That catches a dead host, container, network or HTTP front (#2240: a service that
/// accepted the request and never answered held the account's upload slots for the 30 minutes of the
/// job bound; Node gave up after 5 minutes). It does NOT catch a worker that hangs while the HTTP front
/// still answers `/health`: that needs a heartbeat from the worker, and such a request is bounded only by
/// its own deadline and the job bound. The window is generous on purpose: the service stops its worker
/// when the connection goes, so giving up on a healthy long document restarts it from zero.
pub const LIVENESS_EVERY: Duration = Duration::from_secs(30);
pub const LIVENESS_MISSES: u32 = 10;
/// One `/health` ask must answer within this.
const LIVENESS_ASK: Duration = Duration::from_secs(10);

/// `DOCLING_LIVENESS_SECONDS` / `DOCLING_LIVENESS_MISSES`: positive whole numbers, else the defaults.
fn liveness_from_env() -> (Duration, u32) {
    let number = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n > 0 && *n <= 3600)
    };
    (
        number("DOCLING_LIVENESS_SECONDS").map_or(LIVENESS_EVERY, Duration::from_secs),
        number("DOCLING_LIVENESS_MISSES").map_or(LIVENESS_MISSES, |n| n.min(100) as u32),
    )
}

impl HttpEngine {
    pub fn new(rt: tokio::runtime::Handle, data_dir: Option<PathBuf>) -> Option<Arc<Self>> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .ok()?;
        Some(Arc::new(HttpEngine {
            rt,
            client,
            data_dir,
            ranges: Mutex::new(None),
            caps: Mutex::new(None),
            waits: vec![Duration::from_secs(5), Duration::from_secs(15)],
            liveness: liveness_from_env(),
        }))
    }

    /// A test's engine that asks `/health` every `every` and gives up after `misses` misses.
    pub fn with_liveness(mut self: Arc<Self>, every: Duration, misses: u32) -> Arc<Self> {
        if let Some(e) = Arc::get_mut(&mut self) {
            e.liveness = (every, misses);
        }
        self
    }

    /// A test's engine with no busy waits.
    pub fn without_waits(mut self: Arc<Self>) -> Arc<Self> {
        if let Some(e) = Arc::get_mut(&mut self) {
            e.waits = vec![Duration::ZERO, Duration::ZERO];
        }
        self
    }

    /// document-workers.cjs `resolve(kind)`: (url, from a machine).
    fn resolve(&self, kind: &str) -> Option<(String, bool)> {
        if let Some(url) = self.data_dir.as_deref().and_then(|d| machine_url(d, kind)) {
            return Some((url, true));
        }
        let env = if kind == "ocr" {
            "OCR_BASE_URL"
        } else {
            "DOCLING_BASE_URL"
        };
        std::env::var(env)
            .ok()
            .filter(|u| !u.is_empty())
            .map(|u| (u, false))
    }

    fn send(
        &self,
        url: &str,
        machine: bool,
        req: reqwest::RequestBuilder,
        cap: usize,
    ) -> Result<(u16, Option<Vec<u8>>), EngineError> {
        if machine && !on_this_network(url) {
            return Err(EngineError::new("fetch failed"));
        }
        self.rt.block_on(fetch(req, cap))
    }

    /// [`Self::send`] for a request that may legitimately wait for minutes: while it waits, the
    /// service is asked for `/health`, and a service that stops answering ends the wait.
    fn send_watched(
        &self,
        url: &str,
        machine: bool,
        req: reqwest::RequestBuilder,
        cap: usize,
    ) -> Result<(u16, Option<Vec<u8>>), EngineError> {
        if machine && !on_this_network(url) {
            return Err(EngineError::new("fetch failed"));
        }
        let (every, limit) = self.liveness;
        let client = self.client.clone();
        let health = format!("{}/health", url.trim_end_matches('/'));
        self.rt.block_on(async move {
            let work = fetch(req, cap);
            tokio::pin!(work);
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            let mut missed = 0u32;
            loop {
                tokio::select! {
                    done = &mut work => return done,
                    _ = ticks.tick() => {
                        // The ask races the request: a reply that arrives meanwhile is kept, not thrown away.
                        let ask = client.get(&health).timeout(LIVENESS_ASK.min(every)).send();
                        let alive = tokio::select! {
                            done = &mut work => return done,
                            asked = ask => asked.is_ok_and(|r| r.status().is_success()),
                        };
                        missed = if alive { 0 } else { missed + 1 };
                        if missed >= limit {
                            return Err(EngineError {
                                message: "Document extraction unavailable: the service stopped responding; it will be retried.".into(),
                                timed_out: true,
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        })
    }

    fn health(&self, kind: &str) -> Option<JValue> {
        let (url, machine) = self.resolve(kind)?;
        let req = self
            .client
            .get(format!("{}/health", url.trim_end_matches('/')))
            .timeout(Duration::from_secs(10));
        match self.send(&url, machine, req, HEALTH_CAP) {
            Ok((_, Some(body))) => wtf16::parse(&String::from_utf8_lossy(&body)).ok(),
            _ => None,
        }
    }
}

/// The request, its status and its body up to `cap` bytes (`None` past it or for a non-2xx status).
async fn fetch(
    req: reqwest::RequestBuilder,
    cap: usize,
) -> Result<(u16, Option<Vec<u8>>), EngineError> {
    let mut resp = req.send().await.map_err(transport)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok((status, None));
    }
    if resp.content_length().is_some_and(|n| n as usize > cap) {
        return Ok((status, None));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport)? {
        body.extend_from_slice(&chunk);
        if body.len() > cap {
            return Ok((status, None));
        }
    }
    Ok((status, Some(body)))
}

fn transport(e: reqwest::Error) -> EngineError {
    if e.is_timeout() {
        EngineError {
            message: "The operation was aborted due to timeout".into(),
            timed_out: true,
            ..Default::default()
        }
    } else {
        // undici's TypeError for a connection that failed.
        EngineError::new("fetch failed")
    }
}

/// inference-machines.cjs `urlFor(service)` from the saved state.
fn machine_url(data_dir: &Path, kind: &str) -> Option<String> {
    let store = server_store::Store::open(data_dir).ok()?;
    let raw: Option<String> = store
        .read(|r| {
            r.row(
                "SELECT value FROM settings WHERE key='inference:machines'",
                [],
                |row| row.get(0),
            )
        })
        .ok()
        .flatten();
    let state = wtf16::parse(&raw?).ok()?;
    if state.get("version").as_num() != Some(1.0) {
        return None;
    }
    let a = state.get("assignments").get(kind);
    let machine = a.get("machine").as_str()?;
    let port = a.get("port").as_num().filter(|p| p.fract() == 0.0)?;
    let JValue::Arr(machines) = state.get("machines") else {
        return None;
    };
    let m = machines.iter().find(|m| {
        m.get("id").as_str() == Some(machine)
            && machine != "local"
            && m.get("name").as_str().is_some()
    })?;
    let address = m.get("address").as_str()?;
    Some(format!("{address}:{}", js_json::number_to_string(port)))
}

fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            v.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// service-endpoint.cjs: every answer for the host is on this network.
fn on_this_network(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = u.host_str() else {
        return false;
    };
    let port = u.port_or_known_default().unwrap_or(80);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let all: Vec<_> = addrs.collect();
            !all.is_empty() && all.iter().all(|a| private(a.ip()))
        }
        Err(_) => false,
    }
}

/// docling.cjs `documentName(name)`: the last path segment, its last 200 UTF-16 units.
fn document_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let last = if last.is_empty() { "document" } else { last };
    tail16(last, 200)
}

fn tail16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= n {
        return s.to_string();
    }
    String::from_utf16_lossy(units.get(units.len() - n..).unwrap_or_default())
}

/// `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// docling.cjs `headerSafeName(name)`: Latin-1 (no controls) as it is, anything else percent-encoded.
fn header_safe_name(name: &str) -> reqwest::header::HeaderValue {
    let latin = name
        .chars()
        .all(|c| c == '\t' || (' '..='~').contains(&c) || ('\u{80}'..='\u{ff}').contains(&c));
    let safe = if latin {
        name.to_string()
    } else {
        encode_uri_component(name)
    };
    let safe = tail16(&safe, 200);
    let bytes: Vec<u8> = safe.chars().map(|c| c as u32 as u8).collect();
    reqwest::header::HeaderValue::from_bytes(&bytes)
        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("document"))
}

impl Engine for HttpEngine {
    fn docling_configured(&self) -> bool {
        self.resolve("documents").is_some()
    }

    fn docling(
        &self,
        name: &str,
        bytes: &[u8],
        figures: bool,
        pages: Option<(u32, u32)>,
    ) -> Result<JValue, EngineError> {
        let Some((url, machine)) = self.resolve("documents") else {
            return Err(EngineError::new(server_projects::extract::NOT_CONFIGURED));
        };
        let range = pages.filter(|(a, b)| *a >= 1 && b >= a && b - a < 200);
        let timeout = match range {
            Some((a, b)) => (120_000 + u64::from(b - a + 1) * 60_000).min(3_900_000),
            None => 3_900_000,
        };
        let mut req = self
            .client
            .post(format!("{}/extract", url.trim_end_matches('/')))
            .header("Content-Type", "application/octet-stream")
            .header("X-Document-Name", header_safe_name(&document_name(name)))
            .timeout(Duration::from_millis(timeout))
            .body(bytes.to_vec());
        if figures {
            req = req.header("X-Document-Figures", "1");
        }
        if let Some((a, b)) = range {
            req = req.header("X-Document-Pages", format!("{a}-{b}"));
        }
        let invalid = || EngineError::new("Invalid extraction response; refresh to retry.");
        let (status, body) = self.send_watched(&url, machine, req, DOCLING_REPLY_CAP)?;
        match status {
            200..=299 => {}
            503 => {
                return Err(EngineError {
                    message: "Document extraction is busy; refresh to retry.".into(),
                    status: Some(503),
                    ..Default::default()
                })
            }
            400 => return Err(EngineError::new("noevia sent this document in a form the extractor refused; it will be read again after the next update.")),
            415 => {
                return Err(EngineError {
                    message: "This file type cannot be read yet; the original is stored.".into(),
                    permanent: true,
                    ..Default::default()
                })
            }
            422 => {
                return Err(EngineError {
                    message: "This document could not be read; the original is stored.".into(),
                    permanent: true,
                    ..Default::default()
                })
            }
            s => {
                return Err(EngineError::new(&format!(
                    "Document extraction unavailable (HTTP {s}); refresh to retry."
                )))
            }
        }
        let body = body.ok_or_else(invalid)?;
        let parsed = wtf16::parse(&String::from_utf8_lossy(&body)).map_err(|_| invalid())?;
        let JValue::Arr(list) = parsed.get("pages") else {
            return Err(invalid());
        };
        if let Some((a, b)) = range {
            let outside = list.iter().any(|p| {
                let n = p.get("number").as_num().unwrap_or(f64::NAN);
                !(n >= f64::from(a) && n <= f64::from(b))
            });
            if outside {
                return Err(EngineError {
                    message: server_projects::extract::NO_RANGES.into(),
                    no_ranges: true,
                    ..Default::default()
                });
            }
        }
        Ok(parsed)
    }

    fn docling_ranges(&self, bytes: usize) -> Option<bool> {
        let url = self.resolve("documents")?.0;
        let cached = self
            .ranges
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .filter(|c| c.url == url && c.at.elapsed() < CAPABILITY_TTL);
        let value = match cached {
            Some(c) => c.value,
            None => {
                let value = self.health("documents").map(|b| {
                    let max = b
                        .get("rangedMaxBytes")
                        .as_num()
                        .filter(|n| *n > 0.0 && n.fract() == 0.0)
                        .map(|n| n as usize);
                    (b.get("ranges").as_bool() == Some(true), max)
                });
                if let Ok(mut c) = self.ranges.lock() {
                    *c = Some(Cached {
                        at: Instant::now(),
                        url,
                        value,
                    });
                }
                value
            }
        };
        let (ranges, max) = value?;
        Some(ranges && !max.is_some_and(|m| bytes > m))
    }

    fn ocr_configured(&self) -> bool {
        self.resolve("ocr").is_some()
    }

    /// docx.cjs `extract(bytes)`: the OCR worker's `/extract-docx` (60 s, a 2 MiB reply).
    fn extract_docx(&self, bytes: &[u8]) -> Result<JValue, EngineError> {
        let Some((url, machine)) = self.resolve("ocr") else {
            return Err(EngineError::new(
                "DOCX reader is unavailable. Re-upload after the document worker is configured.",
            ));
        };
        let req = self
            .client
            .post(format!("{}/extract-docx", url.trim_end_matches('/')))
            .header(
                "Content-Type",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            )
            .timeout(Duration::from_secs(60))
            .body(bytes.to_vec());
        let (status, body) = self.send(&url, machine, req, 2 * 1024 * 1024)?;
        if !(200..300).contains(&status) {
            return Err(EngineError::new(if status == 503 {
                "Document reader is busy. Re-upload or refresh connected storage to retry."
            } else {
                "DOCX could not be read; it may be malformed, encrypted, oversized, or the reader is unavailable."
            }));
        }
        let invalid = || EngineError::new("Invalid DOCX reader response.");
        let body = body.ok_or_else(invalid)?;
        wtf16::parse(&String::from_utf8_lossy(&body)).map_err(|_| invalid())
    }

    /// pdf-reduce.cjs: the OCR worker's `/reduce-pdf` (180 s); `None` without an OCR worker.
    fn reduce_pdf(&self, bytes: &[u8]) -> Option<Result<JValue, EngineError>> {
        let (url, machine) = self.resolve("ocr")?;
        // The largest honest answer is a base64 PDF at 25 MiB plus a small JSON envelope.
        let cap = (25 * 1024 * 1024usize).div_ceil(3) * 4 + 64 * 1024;
        let req = self
            .client
            .post(format!("{}/reduce-pdf", url.trim_end_matches('/')))
            .header("Content-Type", "application/pdf")
            .timeout(Duration::from_secs(180))
            .body(bytes.to_vec());
        Some(match self.send_watched(&url, machine, req, cap) {
            Err(e) => Err(e),
            Ok((status, _)) if !(200..300).contains(&status) => Err(EngineError {
                message: format!("storage returned {status}"),
                status: Some(status),
                ..Default::default()
            }),
            Ok((_, None)) => Err(EngineError {
                message: "Document processor response was too large.".into(),
                status: Some(502),
                permanent: true,
                ..Default::default()
            }),
            Ok((_, Some(body))) => wtf16::parse(&String::from_utf8_lossy(&body))
                .map_err(|_| EngineError::new("Invalid PDF reduction response")),
        })
    }

    fn ocr_pages(
        &self,
        bytes: &[u8],
        pages: &[u32],
        dpi: Option<u32>,
        timeout_ms: Option<u64>,
    ) -> Result<Vec<JValue>, EngineError> {
        let Some((url, machine)) = self.resolve("ocr") else {
            return Ok(Vec::new());
        };
        let list: Vec<String> = pages.iter().take(50).map(u32::to_string).collect();
        let timeout = timeout_ms
            .filter(|t| *t > 0)
            .unwrap_or(610_000)
            .min(610_000);
        let mut req = self
            .client
            .post(format!("{}/extract", url.trim_end_matches('/')))
            .header("Content-Type", "application/pdf")
            .header("X-OCR-Pages", format!("[{}]", list.join(",")))
            .timeout(Duration::from_millis(timeout))
            .body(bytes.to_vec());
        if dpi == Some(400) {
            req = req.header("X-OCR-DPI", "400");
        }
        let (status, body) = self.send_watched(&url, machine, req, OCR_REPLY_CAP)?;
        if !(200..300).contains(&status) {
            return Err(EngineError {
                message: if status == 503 {
                    "OCR is busy; refresh to retry.".into()
                } else {
                    format!("OCR unavailable (HTTP {status}); refresh to retry.")
                },
                status: Some(status),
                ..Default::default()
            });
        }
        let invalid = || EngineError::new("Invalid OCR response; refresh to retry.");
        let body = body.ok_or_else(invalid)?;
        let parsed = wtf16::parse(&String::from_utf8_lossy(&body)).map_err(|_| invalid())?;
        match parsed.get("pages") {
            JValue::Arr(list) => Ok(list.clone()),
            _ => Err(invalid()),
        }
    }

    fn ocr_caps(&self) -> Option<OcrCaps> {
        let url = self.resolve("ocr")?.0;
        if let Some(c) = self
            .caps
            .lock()
            .ok()
            .and_then(|c| c.clone())
            .filter(|c| c.url == url && c.at.elapsed() < CAPABILITY_TTL)
        {
            return c.value;
        }
        let value = self.health("ocr").map(|b| {
            if b.get("ranges").as_bool() != Some(true) {
                return OcrCaps::default();
            }
            OcrCaps {
                max_bytes: b
                    .get("maxBytes")
                    .as_num()
                    .filter(|n| *n > 0.0 && n.fract() == 0.0)
                    .map(|n| n as usize),
                dpis: match b.get("dpis") {
                    JValue::Arr(d) => d
                        .iter()
                        .filter_map(|x| x.as_num().filter(|n| n.fract() == 0.0))
                        .map(|n| n as u32)
                        .collect(),
                    _ => Vec::new(),
                },
            }
        });
        if let Ok(mut c) = self.caps.lock() {
            *c = Some(Cached {
                at: Instant::now(),
                url,
                value: value.clone(),
            });
        }
        value
    }

    fn multimodal(&self) -> bool {
        std::env::var("NOEVIA_MULTIMODAL_RAG").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            )
        })
    }

    fn busy_waits(&self) -> Vec<Duration> {
        self.waits.clone()
    }

    /// The host-wide lock core's large-document-lock.cjs also takes (review #2191 r4+5, #697).
    fn large_lock_file(&self) -> Option<PathBuf> {
        self.data_dir
            .as_deref()
            .map(|d| d.join(server_projects::extract::LARGE_LOCK_FILE))
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// One pass over every user's queue: the due jobs run, one user after another (one extraction at a
/// time in this process, as one Docling slot serves them all).
pub fn pass(data_dir: &Path, engine: &dyn Engine, switch: server_store::RustProjects) -> usize {
    let limits = Limits::from_env();
    let mut ran = 0;
    for user in server_projects::extract_queue::users_with_jobs(data_dir) {
        let ctx = Ctx {
            data_dir,
            user_id: &user,
            switch,
            now_ms: now_ms(),
        };
        ran += server_projects::extract_queue::run_due(ctx, engine, &limits).len();
    }
    ran
}

/// At start: every job a previous process left `running` is pending again.
pub fn recover(data_dir: &Path) -> usize {
    server_projects::extract_queue::users_with_jobs(data_dir)
        .iter()
        .filter_map(|u| server_projects::store::user_dir(data_dir, u).ok())
        .map(|dir| server_projects::extract_queue::recover(&dir, now_ms()).unwrap_or(0))
        .sum()
}

/// Starts the worker: recovery once, then a pass every [`TICK`] on the blocking pool.
pub fn spawn(app: &Arc<crate::App>) {
    let (Some(data_dir), Some(engine)) = (app.config.data_dir.clone(), app.extraction.clone())
    else {
        return;
    };
    let switch = app.config.rust_projects;
    tokio::spawn(async move {
        let dir = data_dir.clone();
        let recovered = tokio::task::spawn_blocking(move || recover(&dir))
            .await
            .unwrap_or(0);
        if recovered > 0 {
            println!(
                "noevia-server: extraction queue: {recovered} interrupted job(s) pending again"
            );
        }
        let busy = Arc::new(tokio::sync::Semaphore::new(1));
        loop {
            tokio::time::sleep(TICK).await;
            let Ok(permit) = Arc::clone(&busy).try_acquire_owned() else {
                continue;
            };
            let (dir, engine) = (data_dir.clone(), Arc::clone(&engine));
            tokio::task::spawn_blocking(move || {
                let _held = permit;
                pass(&dir, engine.as_ref(), switch)
            });
        }
    });
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]
mod tests {
    use super::*;

    #[test]
    fn names_go_as_core_sends_them() {
        assert_eq!(document_name("Documents/Tax 2024/W-2.pdf"), "W-2.pdf");
        assert_eq!(document_name("a\\b.pdf"), "b.pdf");
        assert_eq!(document_name("dir/"), "document");
        assert_eq!(header_safe_name("caf\u{e9}.pdf").as_bytes(), b"caf\xe9.pdf");
        assert_eq!(
            header_safe_name("\u{65e5}.pdf").to_str().unwrap(),
            "%E6%97%A5.pdf"
        );
        assert_eq!(header_safe_name("a\nb.pdf").to_str().unwrap(), "a%0Ab.pdf");
    }

    #[test]
    fn only_this_network_is_reached_for_a_machine() {
        assert!(on_this_network("http://127.0.0.1:5001"));
        assert!(on_this_network("http://10.0.0.5:5001"));
        assert!(on_this_network("http://100.100.1.1:5001"));
        assert!(!on_this_network("http://8.8.8.8:5001"));
        assert!(!on_this_network("not a url"));
    }
}
