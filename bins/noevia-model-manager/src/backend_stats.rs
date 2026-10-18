//! noevia#1318 slice 13: the native backend sampler and `GET /api/v1/backends`.
//!
//! Python (`hw._sample_loop`, `hw._collect`, `services.snapshot_llama_backends`, `api.backends`):
//! every 2 s the sampler lists the effective backends (LLAMA_CONTAINERS, then every container whose
//! first image tag contains `llama.cpp`, never `model-loader`), reads each one's Docker state, GPU
//! and container figures into a cache, and keeps 450 points of history per backend.
//! `GET /api/v1/backends` reads each backend's Docker state, probes its llama-server for the loaded
//! model, and adds the cached figures and the last 120 history points.
//!
//! What runs here: the Docker reads (`/version`, the container list, inspect, image inspect and
//! one-shot container stats, all read-only and allowlisted in `docker.rs`), the sysfs reads of AMD
//! cards mapped into a backend (`hw._read_drm`), the `GPU_VRAM` declaration, the history, the
//! llama-server probes and the answer. What stays in the Python child: the GPU reads that need
//! `docker exec` (`nvidia-smi`, `rocm-smi`), asked through its private
//! `GET /api/v1/internal/gpu-exec-probe`; this side never runs `exec`. After every tick the cache
//! is pushed to the child (`PUT /api/v1/internal/backend-stats`), whose other readers (fit chips,
//! autoconfig, the benchmark observer) keep reading it through `hw.stats_for`; the child no longer
//! samples the backends itself.
//!
//! Python arithmetic is reproduced exactly where it shapes the answer: `round()` (ties to even on
//! the exact value), `round(x)` giving an int, `sum()` of floats with Python 3.12's Neumaier
//! compensation, `max()` keeping the first maximum (and its int or float type), int/int division.
//! Deliberate differences (see `docs/native-backends.md`): Docker answers of a shape Python would
//! crash on (a stats body that is not JSON, a container without `Image`, a non-RFC 3339
//! `StartedAt`) give a generic sampler error, or the native 503 for the GET, instead of Python's
//! exception text or 500.
use crate::Job;
use model_files::json::Value as J;

pub mod fit;
pub mod live;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// The child's private calls (never forwarded from outside: `is_internal_path`).
pub const PUSH_PATH: &str = "/api/v1/internal/backend-stats";
pub const PROBE_PATH: &str = "/api/v1/internal/gpu-exec-probe";
/// `hw._DRM_ROOT`.
pub const DRM_ROOT: &str = "/sys/class/drm";
/// The native 503 body when this side cannot answer exactly.
pub const REFUSAL: &str = "Native backend read is unavailable";
/// `hw._SAMPLE_INTERVAL_S`.
pub const INTERVAL: Duration = Duration::from_secs(2);
/// `hw._HISTORY_MAXLEN` and the `points[-120:]` the answer carries.
const HISTORY_MAX: usize = 450;
const HISTORY_SHOWN: usize = 120;
/// Largest Docker answer parsed here (inspect data can carry long environments).
const DOCKER_JSON_CAP: usize = 1024 * 1024;
/// A bound on one tick's reads (Python has none; a stuck daemon ends only that tick).
const TICK_BUDGET: Duration = Duration::from_secs(60);
/// One sysfs file is a few bytes; anything past this is not a counter.
const SYSFS_CAP: u64 = 4096;
const GIB: i128 = 1024 * 1024 * 1024;

/// One raw Docker answer: status, the reason phrase of its status line, body.
#[derive(Debug, Clone)]
pub struct Raw {
    pub status: u16,
    pub reason: String,
    pub body: Vec<u8>,
}

/// The read-only Docker GETs this module makes (allowlisted by the production transport).
pub trait DockerApi: Send + Sync {
    /// `Err`: the daemon could not be reached or the exchange failed.
    fn get<'a>(&'a self, path: &'a str) -> Job<'a, Raw>;
}

pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

fn wall() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

// ------------------------------------------------------------------ Python numbers

/// A number as Python holds it: an int or a float (they render differently).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Num {
    Int(i128),
    Float(f64),
}

impl Num {
    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
    /// Python's `a > b` across int and float.
    fn gt(self, other: Num) -> bool {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => a > b,
            _ => self.as_f64() > other.as_f64(),
        }
    }
    fn render(self, out: &mut String) -> Result<(), Fail> {
        match self {
            Num::Int(i) => out.push_str(&i.to_string()),
            Num::Float(f) if f.is_finite() => out.push_str(&gguf::py_float_repr(f)),
            // FastAPI's `allow_nan=False` raises: the plain 500.
            Num::Float(_) => return Err(Fail::Raised),
        }
        Ok(())
    }
}

/// `round(x, nd)`.
fn round_nd(x: f64, nd: usize) -> f64 {
    model_autoconfig::pyfloat::round_nd(x, nd)
}

/// `int / int`, Python's true division (exact operands below 2^53, as every counter here is).
fn div(a: i128, b: i128) -> f64 {
    a as f64 / b as f64
}

/// Python 3.12 `sum()` of floats (start 0): the first float as is, then Neumaier compensation.
fn py_sum(values: &[f64]) -> f64 {
    let Some((first, rest)) = values.split_first() else {
        return 0.0;
    };
    // `0 + x` (an int plus a float) is `x`, except that -0.0 becomes 0.0.
    let mut total = 0.0 + *first;
    let mut c = 0.0;
    for &x in rest {
        let t = total + x;
        if total.abs() >= x.abs() {
            c += (total - t) + x;
        } else {
            c += (x - t) + total;
        }
        total = t;
    }
    if c != 0.0 && c.is_finite() {
        total += c;
    }
    total
}

/// `max(values)`: the first maximal element, with its own type.
fn py_max(values: &[Num]) -> Option<Num> {
    let mut best = *values.first()?;
    for v in values.iter().skip(1) {
        if v.gt(best) {
            best = *v;
        }
    }
    Some(best)
}

// ------------------------------------------------------------------ the dataclasses

/// `hw.GpuCard`.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub index: i128,
    pub name: String,
    pub util_pct: Num,
    pub vram_used_gb: Num,
    pub vram_total_gb: Num,
    pub temp_c: Num,
    pub power_w: Num,
    pub shared_used_gb: Num,
    pub shared_total_gb: Num,
    pub clock_mhz: Num,
    pub device: String,
}

/// `hw.GpuStats`.
#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub vendor: String,
    pub name: String,
    pub util_pct: Num,
    pub vram_used_gb: Num,
    pub vram_total_gb: Num,
    pub temp_c: Num,
    pub power_w: Num,
    pub gpu_count: i128,
    pub cards: Vec<Card>,
    pub memory_kind: String,
    pub shared_used_gb: Num,
    pub shared_total_gb: Num,
    pub clock_mhz: Num,
    pub source: String,
    pub measured: bool,
}

/// `hw.ContainerRuntimeStats`.
#[derive(Debug, Clone, PartialEq)]
pub struct Runtime {
    pub cpu_pct: Num,
    pub mem_used_gb: Num,
    pub mem_limit_gb: Num,
    pub mem_anon_gb: Option<Num>,
}

/// `hw.BackendStats`.
#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub ok: bool,
    pub error: Option<String>,
    pub gpu: Option<Gpu>,
    pub container: Option<Runtime>,
}

impl Stats {
    fn failed(error: impl Into<String>) -> Self {
        Stats {
            ok: false,
            error: Some(error.into()),
            gpu: None,
            container: None,
        }
    }
}

/// `hw.HistoryPoint`.
#[derive(Debug, Clone, PartialEq)]
struct Point {
    ts: f64,
    gpu_util: Num,
    vram_used_gb: Num,
    cpu_pct: Num,
    mem_used_gb: Num,
    per_gpu_util: Vec<Num>,
    per_gpu_vram_used_gb: Vec<Num>,
    shared_used_gb: Num,
    temp_c: Num,
    power_w: Num,
}

fn zero() -> Num {
    Num::Float(0.0)
}

/// `hw._record_history(name, s)` for an `ok` sample.
fn point(ts: f64, s: &Stats) -> Point {
    let g = s.gpu.as_ref();
    let c = s.container.as_ref();
    Point {
        ts,
        gpu_util: g.map_or(zero(), |g| g.util_pct),
        vram_used_gb: g.map_or(zero(), |g| g.vram_used_gb),
        cpu_pct: c.map_or(zero(), |c| c.cpu_pct),
        mem_used_gb: c.map_or(zero(), |c| c.mem_used_gb),
        per_gpu_util: g.map_or(Vec::new(), |g| g.cards.iter().map(|c| c.util_pct).collect()),
        per_gpu_vram_used_gb: g.map_or(Vec::new(), |g| {
            g.cards.iter().map(|c| c.vram_used_gb).collect()
        }),
        shared_used_gb: g.map_or(zero(), |g| g.shared_used_gb),
        temp_c: g.map_or(zero(), |g| g.temp_c),
        power_w: g.map_or(zero(), |g| g.power_w),
    }
}

// ------------------------------------------------------------------ rendering (FastAPI's JSON)

/// Python raised while answering: FastAPI's plain 500.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    Raised,
    /// This side cannot answer exactly: the native 503.
    Refused,
}

fn s(out: &mut String, text: &str) {
    out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()));
}

fn opt_s(out: &mut String, text: Option<&str>) {
    match text {
        Some(t) => s(out, t),
        None => out.push_str("null"),
    }
}

/// Writes `"key":` then the value through `f`.
fn field(
    out: &mut String,
    first: &mut bool,
    key: &str,
    f: impl FnOnce(&mut String) -> Result<(), Fail>,
) -> Result<(), Fail> {
    if !*first {
        out.push(',');
    }
    *first = false;
    s(out, key);
    out.push(':');
    f(out)
}

fn nums(out: &mut String, values: &[Num]) -> Result<(), Fail> {
    out.push('[');
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        v.render(out)?;
    }
    out.push(']');
    Ok(())
}

fn render_card(out: &mut String, c: &Card) -> Result<(), Fail> {
    let mut first = true;
    out.push('{');
    field(out, &mut first, "index", |o| Num::Int(c.index).render(o))?;
    field(out, &mut first, "name", |o| {
        s(o, &c.name);
        Ok(())
    })?;
    field(out, &mut first, "util_pct", |o| c.util_pct.render(o))?;
    field(out, &mut first, "vram_used_gb", |o| {
        c.vram_used_gb.render(o)
    })?;
    field(out, &mut first, "vram_total_gb", |o| {
        c.vram_total_gb.render(o)
    })?;
    field(out, &mut first, "temp_c", |o| c.temp_c.render(o))?;
    field(out, &mut first, "power_w", |o| c.power_w.render(o))?;
    field(out, &mut first, "shared_used_gb", |o| {
        c.shared_used_gb.render(o)
    })?;
    field(out, &mut first, "shared_total_gb", |o| {
        c.shared_total_gb.render(o)
    })?;
    field(out, &mut first, "clock_mhz", |o| c.clock_mhz.render(o))?;
    field(out, &mut first, "device", |o| {
        s(o, &c.device);
        Ok(())
    })?;
    out.push('}');
    Ok(())
}

fn render_gpu(out: &mut String, g: &Gpu) -> Result<(), Fail> {
    let mut first = true;
    out.push('{');
    field(out, &mut first, "vendor", |o| {
        s(o, &g.vendor);
        Ok(())
    })?;
    field(out, &mut first, "name", |o| {
        s(o, &g.name);
        Ok(())
    })?;
    field(out, &mut first, "util_pct", |o| g.util_pct.render(o))?;
    field(out, &mut first, "vram_used_gb", |o| {
        g.vram_used_gb.render(o)
    })?;
    field(out, &mut first, "vram_total_gb", |o| {
        g.vram_total_gb.render(o)
    })?;
    field(out, &mut first, "temp_c", |o| g.temp_c.render(o))?;
    field(out, &mut first, "power_w", |o| g.power_w.render(o))?;
    field(out, &mut first, "gpu_count", |o| {
        Num::Int(g.gpu_count).render(o)
    })?;
    field(out, &mut first, "cards", |o| {
        o.push('[');
        for (i, c) in g.cards.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            render_card(o, c)?;
        }
        o.push(']');
        Ok(())
    })?;
    field(out, &mut first, "memory_kind", |o| {
        s(o, &g.memory_kind);
        Ok(())
    })?;
    field(out, &mut first, "shared_used_gb", |o| {
        g.shared_used_gb.render(o)
    })?;
    field(out, &mut first, "shared_total_gb", |o| {
        g.shared_total_gb.render(o)
    })?;
    field(out, &mut first, "clock_mhz", |o| g.clock_mhz.render(o))?;
    field(out, &mut first, "source", |o| {
        s(o, &g.source);
        Ok(())
    })?;
    field(out, &mut first, "measured", |o| {
        o.push_str(if g.measured { "true" } else { "false" });
        Ok(())
    })?;
    out.push('}');
    Ok(())
}

/// `_plain(BackendStats)` as JSON.
pub fn render_stats(out: &mut String, st: &Stats) -> Result<(), Fail> {
    let mut first = true;
    out.push('{');
    field(out, &mut first, "ok", |o| {
        o.push_str(if st.ok { "true" } else { "false" });
        Ok(())
    })?;
    field(out, &mut first, "error", |o| {
        opt_s(o, st.error.as_deref());
        Ok(())
    })?;
    field(out, &mut first, "gpu", |o| match &st.gpu {
        Some(g) => render_gpu(o, g),
        None => {
            o.push_str("null");
            Ok(())
        }
    })?;
    field(out, &mut first, "container", |o| match &st.container {
        Some(c) => {
            let mut first = true;
            o.push('{');
            field(o, &mut first, "cpu_pct", |o| c.cpu_pct.render(o))?;
            field(o, &mut first, "mem_used_gb", |o| c.mem_used_gb.render(o))?;
            field(o, &mut first, "mem_limit_gb", |o| c.mem_limit_gb.render(o))?;
            field(o, &mut first, "mem_anon_gb", |o| match c.mem_anon_gb {
                Some(n) => n.render(o),
                None => {
                    o.push_str("null");
                    Ok(())
                }
            })?;
            o.push('}');
            Ok(())
        }
        None => {
            o.push_str("null");
            Ok(())
        }
    })?;
    out.push('}');
    Ok(())
}

fn render_point(out: &mut String, p: &Point) -> Result<(), Fail> {
    let mut first = true;
    out.push('{');
    field(out, &mut first, "ts", |o| Num::Float(p.ts).render(o))?;
    field(out, &mut first, "gpu_util", |o| p.gpu_util.render(o))?;
    field(out, &mut first, "vram_used_gb", |o| {
        p.vram_used_gb.render(o)
    })?;
    field(out, &mut first, "cpu_pct", |o| p.cpu_pct.render(o))?;
    field(out, &mut first, "mem_used_gb", |o| p.mem_used_gb.render(o))?;
    field(out, &mut first, "per_gpu_util", |o| {
        nums(o, &p.per_gpu_util)
    })?;
    field(out, &mut first, "per_gpu_vram_used_gb", |o| {
        nums(o, &p.per_gpu_vram_used_gb)
    })?;
    field(out, &mut first, "shared_used_gb", |o| {
        p.shared_used_gb.render(o)
    })?;
    field(out, &mut first, "temp_c", |o| p.temp_c.render(o))?;
    field(out, &mut first, "power_w", |o| p.power_w.render(o))?;
    out.push('}');
    Ok(())
}

// ------------------------------------------------------------------ reading pushed/probed stats

fn num_of(v: Option<&J>) -> Option<Num> {
    match v? {
        J::Int(i) => i.to_string().parse().ok().map(Num::Int),
        J::Float(f) => Some(Num::Float(*f)),
        _ => None,
    }
}

fn str_of(v: Option<&J>) -> Option<String> {
    match v? {
        J::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn int_of(v: Option<&J>) -> Option<i128> {
    match num_of(v)? {
        Num::Int(i) => Some(i),
        Num::Float(_) => None,
    }
}

/// A `GpuStats` as the child's `_plain` wrote it (every field, nothing else).
fn gpu_from(v: &J) -> Option<Gpu> {
    let J::Obj(pairs) = v else { return None };
    if pairs.len() != 15 {
        return None;
    }
    let cards = match v.get("cards")? {
        J::Arr(items) => items.iter().map(card_from).collect::<Option<Vec<_>>>()?,
        _ => return None,
    };
    Some(Gpu {
        vendor: str_of(v.get("vendor"))?,
        name: str_of(v.get("name"))?,
        util_pct: num_of(v.get("util_pct"))?,
        vram_used_gb: num_of(v.get("vram_used_gb"))?,
        vram_total_gb: num_of(v.get("vram_total_gb"))?,
        temp_c: num_of(v.get("temp_c"))?,
        power_w: num_of(v.get("power_w"))?,
        gpu_count: int_of(v.get("gpu_count"))?,
        cards,
        memory_kind: str_of(v.get("memory_kind"))?,
        shared_used_gb: num_of(v.get("shared_used_gb"))?,
        shared_total_gb: num_of(v.get("shared_total_gb"))?,
        clock_mhz: num_of(v.get("clock_mhz"))?,
        source: str_of(v.get("source"))?,
        measured: match v.get("measured")? {
            J::Bool(b) => *b,
            _ => return None,
        },
    })
}

fn card_from(v: &J) -> Option<Card> {
    let J::Obj(pairs) = v else { return None };
    if pairs.len() != 11 {
        return None;
    }
    Some(Card {
        index: int_of(v.get("index"))?,
        name: str_of(v.get("name"))?,
        util_pct: num_of(v.get("util_pct"))?,
        vram_used_gb: num_of(v.get("vram_used_gb"))?,
        vram_total_gb: num_of(v.get("vram_total_gb"))?,
        temp_c: num_of(v.get("temp_c"))?,
        power_w: num_of(v.get("power_w"))?,
        shared_used_gb: num_of(v.get("shared_used_gb"))?,
        shared_total_gb: num_of(v.get("shared_total_gb"))?,
        clock_mhz: num_of(v.get("clock_mhz"))?,
        device: str_of(v.get("device"))?,
    })
}

// ------------------------------------------------------------------ Docker as docker-py sees it

/// How a container or image read ended, as docker-py raises it.
#[derive(Debug, Clone, PartialEq)]
enum ReadErr {
    /// `docker.errors.NotFound` (or `ImageNotFound`): a 404.
    NotFound,
    /// `APIError` and its `str()`.
    Api(String),
    /// Something Python does not catch here (a transport failure, an odd error body).
    Odd,
}

fn parse_json(body: &[u8]) -> Option<J> {
    if body.len() > DOCKER_JSON_CAP {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    model_files::json::parse_with_limits(
        text,
        model_files::json::Limits {
            bytes: DOCKER_JSON_CAP,
            depth: 64,
            nodes: 200_000,
            string_chars: DOCKER_JSON_CAP,
            string_bytes: DOCKER_JSON_CAP,
            number_bytes: 64,
        },
    )
    .ok()
}

/// `urllib.parse.quote(arg, safe="/:")`, as docker-py's `_url` quotes path arguments.
fn quote(arg: &str) -> String {
    let mut out = String::new();
    for b in arg.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~/:".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `docker.errors.create_api_error_from_http_exception` then `str(APIError)`.
fn api_error(raw: &Raw, api: &str, path: &str) -> ReadErr {
    // `response.json()['message']`, else `(response.text or '').strip()`.
    let explanation = match std::str::from_utf8(&raw.body)
        .ok()
        .and_then(|t| (!t.trim().is_empty()).then_some(t))
        .and_then(|t| model_files::json::parse(t).ok())
    {
        Some(J::Obj(pairs)) => match pairs.iter().rev().find(|(k, _)| k == "message") {
            Some((_, J::Str(m))) => m.clone(),
            // A message that is not a string, or none at all (KeyError): not reproduced.
            _ => return ReadErr::Odd,
        },
        // `response.json()` gave a list, number, ...: `['message']` raises TypeError.
        Some(_) => return ReadErr::Odd,
        None => model_autoconfig::pyval::py_strip(&String::from_utf8_lossy(&raw.body)).to_owned(),
    };
    if raw.status == 404 {
        return ReadErr::NotFound;
    }
    let kind = match raw.status {
        400..=499 => "Client Error",
        500..=599 => "Server Error",
        _ => return ReadErr::Odd,
    };
    let mut message = format!(
        "{} {kind} for http+docker://localhost/v{api}{path}: {}",
        raw.status, raw.reason
    );
    if !explanation.is_empty() {
        message.push_str(&format!(" (\"{explanation}\")"));
    }
    ReadErr::Api(message)
}

/// A JSON GET as docker-py makes it: the decoded object, or how it failed.
async fn docker_json(docker: &dyn DockerApi, api: &str, path: &str) -> Result<J, ReadErr> {
    let raw = docker.get(path).await.map_err(|_| ReadErr::Odd)?;
    if !(200..300).contains(&raw.status) {
        return Err(api_error(&raw, api, path));
    }
    parse_json(&raw.body).ok_or(ReadErr::Odd)
}

/// `docker.from_env()`: the negotiated API version, or `None` ("docker unreachable").
async fn client(docker: &dyn DockerApi) -> Option<String> {
    let raw = docker.get("/version").await.ok()?;
    if raw.status != 200 {
        return None;
    }
    let J::Str(v) = parse_json(&raw.body)?.get("ApiVersion")?.clone() else {
        return None;
    };
    // docker-py refuses versions below its minimum (1.24) with a DockerException.
    let (major, minor) = v.split_once('.')?;
    let (major, minor): (u32, u32) = (major.parse().ok()?, minor.parse().ok()?);
    ((major, minor) >= (1, 24)).then_some(v)
}

/// A name docker-py can put in a path that this transport also sends (others answer 404 there).
fn sendable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
}

/// `client.containers.get(name)`: the inspect data.
async fn inspect(docker: &dyn DockerApi, api: &str, name: &str) -> Result<J, ReadErr> {
    if !sendable(name) {
        // Docker has no container by a name it would not accept: a 404 for docker-py.
        return Err(ReadErr::NotFound);
    }
    let path = format!("/containers/{}/json", quote(name));
    let value = docker_json(docker, api, &path).await?;
    matches!(value, J::Obj(_))
        .then_some(value)
        .ok_or(ReadErr::Odd)
}

/// `container.image`: `images.get(attrs.get('ImageID', attrs['Image']).split(':')[1])`.
async fn image(docker: &dyn DockerApi, api: &str, attrs: &J) -> Result<J, ReadErr> {
    let id = match attrs.get("ImageID").or_else(|| attrs.get("Image")) {
        Some(J::Str(s)) => s.clone(),
        _ => return Err(ReadErr::Odd),
    };
    let Some(hex) = id.split(':').nth(1) else {
        return Err(ReadErr::Odd);
    };
    if !sendable(hex) {
        return Err(ReadErr::NotFound);
    }
    let path = format!("/images/{}/json", quote(hex));
    let value = docker_json(docker, api, &path).await?;
    matches!(value, J::Obj(_))
        .then_some(value)
        .ok_or(ReadErr::Odd)
}

/// `(image.tags or [""])[0]`: `None` when a tag is not text (`.lower()` would raise).
fn first_tag_or_empty(img: &J) -> Option<String> {
    match img.get("RepoTags") {
        Some(J::Arr(tags)) => {
            for t in tags {
                match t {
                    J::Str(t) if t == "<none>:<none>" => {}
                    J::Str(t) => return Some(t.clone()),
                    _ => return None,
                }
            }
            Some(String::new())
        }
        None | Some(J::Null) => Some(String::new()),
        Some(_) => None,
    }
}

/// `(image.tags or [image.short_id])[0]`.
fn first_tag(img: &J) -> Option<String> {
    if let Some(J::Arr(tags)) = img.get("RepoTags") {
        for t in tags {
            match t {
                J::Str(t) if t == "<none>:<none>" => {}
                J::Str(t) => return Some(t.clone()),
                _ => return None,
            }
        }
    } else if !matches!(img.get("RepoTags"), None | Some(J::Null)) {
        return None;
    }
    // `Image.short_id`: `id[:19]` for a `sha256:` id, else `id[:12]`.
    let J::Str(id) = img.get("Id")? else {
        return None;
    };
    let keep = if id.starts_with("sha256:") { 19 } else { 12 };
    Some(id.chars().take(keep).collect())
}

/// `Container.status`.
fn status_of(attrs: &J) -> Option<String> {
    match attrs.get("State")? {
        st @ J::Obj(_) => str_of(st.get("Status")),
        J::Str(s) => Some(s.clone()),
        _ => None,
    }
}

/// `Container.short_id`: `attrs['Id'][:12]`.
fn short_id(attrs: &J) -> Option<String> {
    str_of(attrs.get("Id")).map(|id| id.chars().take(12).collect())
}

/// `services.discover_llama_containers()`: each name with its lower-cased first image tag, `[]` on
/// any Docker failure.
async fn discover(docker: &dyn DockerApi) -> Vec<(String, String)> {
    let Some(api) = client(docker).await else {
        return Vec::new();
    };
    let Ok(J::Arr(list)) = docker_json(
        docker,
        &api,
        "/containers/json?all=true&limit=1025&size=false",
    )
    .await
    else {
        return Vec::new();
    };
    if list.len() > crate::INVENTORY_CAP {
        return Vec::new();
    }
    let mut out = Vec::new();
    for summary in &list {
        let Some(J::Str(id)) = summary.get("Id") else {
            return Vec::new();
        };
        // `containers.list()` inspects every container; any failure fails the whole list.
        let Ok(attrs) = inspect(docker, &api, id).await else {
            return Vec::new();
        };
        let tag = match image(docker, &api, &attrs).await {
            Ok(img) => match first_tag(&img) {
                Some(t) => t.to_lowercase(),
                None => return Vec::new(),
            },
            Err(ReadErr::Odd) => return Vec::new(),
            // `except DockerException: img = ""`.
            Err(_) => String::new(),
        };
        if !tag.contains("llama.cpp") {
            continue;
        }
        let name = match attrs.get("Name") {
            Some(J::Str(n)) => n.trim_start_matches('/').to_owned(),
            _ => continue,
        };
        if name == "model-loader" {
            continue;
        }
        out.push((name, tag));
    }
    out
}

/// `_effective_container_names()`: configured first, then discovered, without repeats.
fn effective(configured: &[String], discovered: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in configured.iter().chain(discovered.iter().map(|(n, _)| n)) {
        if !n.is_empty() && !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

// ------------------------------------------------------------------ sysfs (`hw._read_drm`)

/// `_read_text(path)`: stripped UTF-8 text, "" when it cannot be read.
fn read_text(path: &Path) -> Result<String, ()> {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(String::new());
    };
    let mut bytes = Vec::new();
    if file.take(SYSFS_CAP + 1).read_to_end(&mut bytes).is_err() {
        return Ok(String::new());
    }
    if bytes.len() as u64 > SYSFS_CAP {
        return Err(());
    }
    // A decode error is not an OSError in Python: it ends the sample.
    let text = String::from_utf8(bytes).map_err(|_| ())?;
    Ok(model_autoconfig::pyval::py_strip(&text).to_owned())
}

/// `_read_int(path)`: `int(text or 0)`, 0 where `int()` refuses.
fn read_int(path: &Path) -> Result<i128, ()> {
    let text = read_text(path)?;
    if text.is_empty() {
        return Ok(0);
    }
    match model_autoconfig::pyval::int_str(&text, "sysfs") {
        Ok(n) => Ok(n),
        Err(model_autoconfig::Error::Python(_)) => Ok(0),
        // Non-ASCII digits or a huge value: not reproduced.
        Err(_) => Err(()),
    }
}

/// `_container_drm_cards(container)`: `card<N>` names from `HostConfig.Devices[*].PathOnHost`.
fn drm_cards(attrs: &J) -> Result<Vec<String>, ()> {
    let host = match attrs.get("HostConfig") {
        Some(h) if h.truthy() => h,
        _ => return Ok(Vec::new()),
    };
    let J::Obj(_) = host else { return Err(()) };
    let devices = match host.get("Devices") {
        Some(J::Arr(d)) => d.clone(),
        Some(d) if !d.truthy() => Vec::new(),
        None => Vec::new(),
        Some(_) => return Err(()),
    };
    let mut out = Vec::new();
    for d in &devices {
        let J::Obj(_) = d else { return Err(()) };
        // `str(d.get("PathOnHost") or "")`: another type's text never matches the pattern.
        let Some(J::Str(path)) = d.get("PathOnHost").cloned() else {
            continue;
        };
        // `re.match(r"^/dev/dri/(card\d+)$", ...)`: `$` also matches before one final newline.
        let body = path.strip_suffix('\n').unwrap_or(&path);
        if let Some(card) = body.strip_prefix("/dev/dri/") {
            if let Some(digits) = card.strip_prefix("card") {
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    out.push(card.to_owned());
                } else if !digits.is_empty() && !digits.is_ascii() {
                    // `\d` takes every Unicode digit; not reproduced.
                    return Err(());
                }
            }
        }
    }
    Ok(out)
}

fn gpu_name(vendor: &str, device: &str) -> String {
    match (vendor, device) {
        ("0x1002", "0x150e") => "AMD Radeon 880M/890M".into(),
        ("0x1002", "0x1900") | ("0x1002", "0x15bf") => "AMD Radeon 780M".into(),
        ("0x1002", "0x164e") => "AMD Radeon (Raphael iGPU)".into(),
        _ => {
            let v = match vendor {
                "0x1002" => "AMD",
                "0x10de" => "NVIDIA",
                "0x8086" => "Intel",
                other => other,
            };
            format!("{v} GPU ({device})")
        }
    }
}

/// `hw._read_drm(container)`. `Err` where Python would raise out of the sample.
fn read_drm(root: &Path, attrs: &J) -> Result<Option<Gpu>, ()> {
    let mut cards: Vec<Card> = Vec::new();
    for (i, card) in drm_cards(attrs)?.iter().enumerate() {
        let dev = root.join(card).join("device");
        let vendor = read_text(&dev.join("vendor"))?;
        let device = read_text(&dev.join("device"))?;
        if vendor != "0x1002" || !dev.join("mem_info_vram_total").exists() {
            continue;
        }
        let hwmon = match std::fs::read_dir(dev.join("hwmon")) {
            Ok(entries) => {
                let mut names: Vec<String> = Vec::new();
                for e in entries.take(256) {
                    let Ok(e) = e else { continue };
                    if let Ok(n) = e.file_name().into_string() {
                        names.push(n);
                    } else {
                        return Err(());
                    }
                }
                names.sort();
                names.first().map(|n| dev.join("hwmon").join(n))
            }
            Err(_) => None,
        };
        let int = |p: PathBuf| read_int(&p);
        let power = match &hwmon {
            Some(h) => {
                let avg = int(h.join("power1_average"))?;
                if avg != 0 {
                    avg
                } else {
                    int(h.join("power1_input"))?
                }
            }
            None => 0,
        };
        let gb = |p: &str| -> Result<Num, ()> {
            Ok(Num::Float(round_nd(div(int(dev.join(p))?, GIB), 2)))
        };
        cards.push(Card {
            index: i as i128,
            name: gpu_name(&vendor, &device),
            util_pct: Num::Float(int(dev.join("gpu_busy_percent"))? as f64),
            vram_used_gb: gb("mem_info_vram_used")?,
            vram_total_gb: gb("mem_info_vram_total")?,
            temp_c: match &hwmon {
                Some(h) => Num::Float(round_nd(int(h.join("temp1_input"))? as f64 / 1000.0, 1)),
                None => zero(),
            },
            power_w: Num::Float(round_nd(power as f64 / 1_000_000.0, 1)),
            shared_used_gb: gb("mem_info_gtt_used")?,
            shared_total_gb: gb("mem_info_gtt_total")?,
            clock_mhz: match &hwmon {
                Some(h) => {
                    let mhz = int(h.join("freq1_input"))? as f64 / 1_000_000.0;
                    Num::Int(model_autoconfig::pyfloat::round_int(mhz).map_err(|_| ())?)
                }
                None => zero(),
            },
            device: card.clone(),
        });
    }
    if cards.is_empty() {
        return Ok(None);
    }
    let f =
        |pick: fn(&Card) -> Num| -> Vec<f64> { cards.iter().map(|c| pick(c).as_f64()).collect() };
    let unified = cards.iter().all(|c| {
        c.vram_total_gb.as_f64() <= 4.0
            && c.shared_total_gb.as_f64() > c.vram_total_gb.as_f64() * 2.0
    });
    let mut names: Vec<String> = Vec::new();
    for c in &cards {
        if !names.contains(&c.name) {
            names.push(c.name.clone());
        }
    }
    let n = cards.len() as f64;
    let temps: Vec<Num> = cards.iter().map(|c| c.temp_c).collect();
    let clocks: Vec<Num> = cards.iter().map(|c| c.clock_mhz).collect();
    Ok(Some(Gpu {
        vendor: "amd".into(),
        name: names.join(", "),
        util_pct: Num::Float(round_nd(py_sum(&f(|c| c.util_pct)) / n, 1)),
        vram_used_gb: Num::Float(round_nd(py_sum(&f(|c| c.vram_used_gb)), 2)),
        vram_total_gb: Num::Float(round_nd(py_sum(&f(|c| c.vram_total_gb)), 2)),
        temp_c: py_max(&temps).unwrap_or(zero()),
        power_w: Num::Float(round_nd(py_sum(&f(|c| c.power_w)), 1)),
        gpu_count: cards.len() as i128,
        memory_kind: if unified { "unified" } else { "dedicated" }.into(),
        shared_used_gb: Num::Float(round_nd(py_sum(&f(|c| c.shared_used_gb)), 2)),
        shared_total_gb: Num::Float(round_nd(py_sum(&f(|c| c.shared_total_gb)), 2)),
        clock_mhz: py_max(&clocks).unwrap_or(zero()),
        source: "sysfs".into(),
        measured: true,
        cards,
    }))
}

// ------------------------------------------------------------------ container stats

/// A number as the stats code uses it (`int`, `float`, or a bool as an int); `None` for a type
/// Python's arithmetic would refuse (TypeError).
fn arith(v: &J) -> Option<Num> {
    match v {
        J::Bool(b) => Some(Num::Int(i128::from(*b))),
        J::Int(i) => i.to_string().parse().ok().map(Num::Int),
        J::Float(f) => Some(Num::Float(*f)),
        _ => None,
    }
}

fn sub(a: Num, b: Num) -> Num {
    match (a, b) {
        (Num::Int(a), Num::Int(b)) => Num::Int(a - b),
        _ => Num::Float(a.as_f64() - b.as_f64()),
    }
}

/// `x or {}` for a dict lookup: falsy is `{}`; a truthy non-dict is `Err` (its `.get` raises an
/// AttributeError Python does not catch here).
fn dict_or_empty(v: Option<&J>) -> Result<J, ()> {
    match v {
        None => Ok(J::Obj(Vec::new())),
        Some(v) if !v.truthy() => Ok(J::Obj(Vec::new())),
        Some(v @ J::Obj(_)) => Ok(v.clone()),
        Some(_) => Err(()),
    }
}

/// `int(x)` as `_read_container_runtime` applies it; `None` where Python raises one of the
/// errors it catches (TypeError, ValueError), `Err` where it would not be reproduced.
fn py_int(v: &J) -> Result<Option<i128>, ()> {
    match model_autoconfig::pyval::int_of(v, "stats") {
        Ok(n) => Ok(Some(n)),
        Err(model_autoconfig::Error::Python("TypeError" | "ValueError")) => Ok(None),
        Err(_) => Err(()),
    }
}

/// `hw._read_container_runtime` over the stats JSON: `Ok(None)` where it returns None, `Err` where
/// an exception it does not catch would end the sample.
fn runtime(stats: &J) -> Result<Option<Runtime>, ()> {
    let J::Obj(_) = stats else { return Err(()) };
    let cpu = dict_or_empty(stats.get("cpu_stats"))?;
    let pre = dict_or_empty(stats.get("precpu_stats"))?;
    let usage = dict_or_empty(cpu.get("cpu_usage"))?;
    let pre_usage = dict_or_empty(pre.get("cpu_usage"))?;
    let num_or_zero = |v: Option<&J>| -> Option<Num> {
        match v {
            None => Some(Num::Int(0)),
            Some(v) => arith(v),
        }
    };
    let (Some(total), Some(pre_total)) = (
        num_or_zero(usage.get("total_usage")),
        num_or_zero(pre_usage.get("total_usage")),
    ) else {
        return Ok(None);
    };
    let cpu_delta = sub(total, pre_total);
    let (Some(system), Some(pre_system)) = (
        num_or_zero(cpu.get("system_cpu_usage")),
        num_or_zero(pre.get("system_cpu_usage")),
    ) else {
        return Ok(None);
    };
    let sys_delta = sub(system, pre_system);
    // `online = cpu.get("online_cpus") or len(percpu) or 1`, `percpu = (...).get(...) or []`.
    let online = match cpu.get("online_cpus") {
        Some(v) if v.truthy() => arith(v),
        _ => {
            let len = match usage.get("percpu_usage") {
                None => 0,
                Some(v) if !v.truthy() => 0,
                Some(J::Arr(a)) => a.len() as i128,
                Some(J::Str(t)) => t.chars().count() as i128,
                Some(J::Obj(pairs)) => {
                    let mut keys: Vec<&String> = Vec::new();
                    for (k, _) in pairs {
                        if !keys.contains(&k) {
                            keys.push(k);
                        }
                    }
                    keys.len() as i128
                }
                // `len()` of a number: TypeError, caught.
                Some(_) => return Ok(None),
            };
            Some(Num::Int(if len != 0 { len } else { 1 }))
        }
    };
    let cpu_pct = if sys_delta.gt(Num::Int(0)) {
        let ratio = match (cpu_delta, sys_delta) {
            (Num::Int(a), Num::Int(b)) => div(a, b),
            (a, b) => a.as_f64() / b.as_f64(),
        };
        // `ratio * online`: a non-number there is a TypeError, caught.
        let Some(online) = online else {
            return Ok(None);
        };
        ratio * online.as_f64() * 100.0
    } else {
        0.0
    };
    let mem = dict_or_empty(stats.get("memory_stats"))?;
    let sub_stats = dict_or_empty(mem.get("stats"))?;
    // `int(stats_sub.get("cache") or stats_sub.get("inactive_file") or 0)`.
    let cache_source = [sub_stats.get("cache"), sub_stats.get("inactive_file")]
        .into_iter()
        .flatten()
        .find(|v| v.truthy());
    let cache = match cache_source {
        Some(v) => match py_int(v)? {
            Some(n) => n,
            None => return Ok(None),
        },
        None => 0,
    };
    let usage_bytes = match mem.get("usage") {
        None => 0,
        Some(v) => match py_int(v)? {
            Some(n) => n,
            None => return Ok(None),
        },
    };
    let mem_used = (usage_bytes - cache).max(0);
    let limit = match mem.get("limit") {
        Some(v) if v.truthy() => match py_int(v)? {
            Some(n) => n,
            None => return Ok(None),
        },
        _ => 0,
    };
    // `stats_sub.get("anon", stats_sub.get("rss"))`.
    let anon = match sub_stats.get("anon").or_else(|| sub_stats.get("rss")) {
        None | Some(J::Null) => None,
        Some(v) => match py_int(v)? {
            Some(n) => Some(Num::Float(round_nd(div(n, GIB), 2))),
            None => return Ok(None),
        },
    };
    Ok(Some(Runtime {
        cpu_pct: Num::Float(round_nd(cpu_pct, 1)),
        mem_used_gb: Num::Float(round_nd(div(mem_used, GIB), 2)),
        mem_limit_gb: if limit != 0 {
            Num::Float(round_nd(div(limit, GIB), 1))
        } else {
            zero()
        },
        mem_anon_gb: anon,
    }))
}

// ------------------------------------------------------------------ `_parse_started_at`

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// What `_parse_started_at` gives: `(started_at, uptime, uptime_s)`.
#[derive(Debug, PartialEq, Eq)]
struct Started {
    local: String,
    up: String,
    secs: Option<i128>,
}

/// `datetime.fromtimestamp(now, timezone.utc)` in microseconds (ROUND_HALF_EVEN of the fraction).
fn now_micros(now: f64) -> i128 {
    let int = now.trunc();
    let mut frac = ((now - int) * 1e6).round_ties_even();
    let mut secs = int as i128;
    if frac >= 1e6 {
        frac -= 1e6;
        secs += 1;
    } else if frac < 0.0 {
        frac += 1e6;
        secs -= 1;
    }
    secs * 1_000_000 + frac as i128
}

/// `services._parse_started_at(iso)` with the process in UTC. `Err` for a shape this side does
/// not parse exactly (anything but Docker's RFC 3339 `...Z`): the native 503.
fn parse_started(iso: &str, now: f64) -> Result<Started, Fail> {
    let empty = || Started {
        local: String::new(),
        up: String::new(),
        secs: None,
    };
    if iso.is_empty() || iso.starts_with("0001") {
        return Ok(empty());
    }
    let b = iso.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = iso.get(r)?;
        (part.bytes().all(|c| c.is_ascii_digit())).then(|| part.parse().ok())?
    };
    // YYYY-MM-DDTHH:MM:SS(.fraction)?Z
    let shape = b.len() >= 20
        && b.get(4) == Some(&b'-')
        && b.get(7) == Some(&b'-')
        && b.get(10) == Some(&b'T')
        && b.get(13) == Some(&b':')
        && b.get(16) == Some(&b':')
        && iso.ends_with('Z');
    if !shape {
        return Err(Fail::Refused);
    }
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) = (
        digits(0..4),
        digits(5..7),
        digits(8..10),
        digits(11..13),
        digits(14..16),
        digits(17..19),
    ) else {
        return Err(Fail::Refused);
    };
    let rest = iso.get(19..iso.len() - 1).ok_or(Fail::Refused)?;
    let micros: i64 = if rest.is_empty() {
        0
    } else {
        let frac = rest.strip_prefix('.').ok_or(Fail::Refused)?;
        if frac.is_empty() || !frac.bytes().all(|c| c.is_ascii_digit()) {
            return Err(Fail::Refused);
        }
        // `frac.rstrip("Z")[:6]`, then fromisoformat reads 1..6 digits as a fraction.
        let six: String = frac.chars().take(6).collect();
        let padded = format!("{six:0<6}");
        padded.parse().map_err(|_| Fail::Refused)?
    };
    // fromisoformat's own range checks raise ValueError: `(iso, "", None)`.
    if y < 1
        || !(1..=12).contains(&mo)
        || d < 1
        || d > days_in_month(y, mo)
        || h > 23
        || mi > 59
        || se > 59
    {
        return Ok(Started {
            local: iso.to_owned(),
            up: String::new(),
            secs: None,
        });
    }
    let started = i128::from(days_from_civil(y, mo, d)) * 86_400 * 1_000_000
        + i128::from(h * 3600 + mi * 60 + se) * 1_000_000
        + i128::from(micros);
    let delta = now_micros(now) - started;
    // `int(delta.total_seconds())`: truncation toward zero.
    let secs = delta / 1_000_000;
    let up = if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    };
    Ok(Started {
        local: format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}"),
        up,
        secs: Some(secs.max(0)),
    })
}

// ------------------------------------------------------------------ the backend snapshot

/// `services.LlamaBackend`.
#[derive(Debug, Clone, PartialEq)]
struct Backend {
    name: String,
    found: bool,
    status: String,
    image: String,
    short_id: String,
    started_at: String,
    uptime: Option<String>,
    uptime_s: Option<i128>,
    host_ports: Vec<String>,
    internal_port: Option<u32>,
    loaded_model: Option<String>,
    probe_error: Option<String>,
    unreachable_but_healthy: bool,
    last_restart_error: Option<String>,
}

impl Backend {
    fn new(name: &str, status: &str) -> Self {
        Backend {
            name: name.to_owned(),
            found: false,
            status: status.to_owned(),
            image: String::new(),
            short_id: String::new(),
            started_at: String::new(),
            uptime: None,
            uptime_s: None,
            host_ports: Vec::new(),
            internal_port: None,
            loaded_model: None,
            probe_error: None,
            unreachable_but_healthy: false,
            last_restart_error: None,
        }
    }
}

/// `_extract_ports` host side: `["<host>-><port/proto>", ...]` in the map's order.
fn host_ports(attrs: &J) -> Result<Vec<String>, ()> {
    let ports = match attrs.get("NetworkSettings") {
        Some(ns) if ns.truthy() => match ns {
            J::Obj(_) => ns.get("Ports").cloned(),
            _ => return Err(()),
        },
        _ => None,
    };
    let pairs = match ports {
        Some(J::Obj(pairs)) => pairs,
        Some(v) if !v.truthy() => Vec::new(),
        None => Vec::new(),
        Some(_) => return Err(()),
    };
    // A dict keeps the first position of a repeated key and its last value.
    let mut keys: Vec<&String> = Vec::new();
    for (k, _) in &pairs {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    let mut out = Vec::new();
    for key in keys {
        let bindings = pairs
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or(J::Null);
        let list = match bindings {
            J::Arr(list) => list,
            v if !v.truthy() => continue,
            _ => return Err(()),
        };
        for b in &list {
            let J::Obj(_) = b else { return Err(()) };
            match b.get("HostPort") {
                Some(J::Str(hp)) if !hp.is_empty() => out.push(format!("{hp}->{key}")),
                Some(J::Int(n)) if n.to_string() != "0" => out.push(format!("{n}->{key}")),
                Some(v) if v.truthy() => return Err(()),
                _ => {}
            }
        }
    }
    Ok(out)
}

/// The inspect fields the port is resolved from, as `BackendSpec`.
fn spec_of(attrs: &J) -> Result<crate::BackendSpec, ()> {
    let strings = |v: Option<&J>| -> Result<Vec<String>, ()> {
        match v {
            Some(J::Arr(items)) => items
                .iter()
                .map(|i| match i {
                    J::Str(s) => Ok(s.clone()),
                    // `str(p)` of an int.
                    J::Int(n) => Ok(n.to_string()),
                    _ => Err(()),
                })
                .collect(),
            _ => Ok(Vec::new()),
        }
    };
    let config = match attrs.get("Config") {
        Some(c @ J::Obj(_)) => c.clone(),
        Some(c) if !c.truthy() => J::Obj(Vec::new()),
        None => J::Obj(Vec::new()),
        Some(_) => return Err(()),
    };
    let env = match config.get("Env") {
        Some(J::Arr(items)) => items
            .iter()
            .filter_map(|i| match i {
                J::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut ports: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(J::Obj(_)) = attrs.get("NetworkSettings") {
        if let Some(J::Obj(pairs)) = attrs.get("NetworkSettings").and_then(|n| n.get("Ports")) {
            for (k, _) in pairs {
                if !ports.iter().any(|(p, _)| p == k) {
                    ports.push((k.clone(), Vec::new()));
                }
            }
        }
    }
    Ok(crate::BackendSpec {
        ports,
        cmd: strings(config.get("Cmd"))?,
        entrypoint: strings(config.get("Entrypoint"))?,
        env,
    })
}

const UNREACHABLE_NOTE: &str = "not reachable from the model loader (Docker reports it healthy)";

/// `_SOFT_PROBE_NOTE_RE`: `^(no models configured|\d+ configured, none loaded)$`.
fn soft_note(note: &str) -> bool {
    if note == "no models configured" {
        return true;
    }
    note.strip_suffix(" configured, none loaded")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// What the native read needs to know about the child and the environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// LLAMA_CONTAINERS, split and stripped.
    pub configured: Vec<String>,
    /// `settings.gpu_vram_map` as Python builds it (a later name replaces an earlier one).
    pub gpu_vram: Vec<(String, f64)>,
    /// LLAMA_DEFAULT_PORT (0 is none).
    pub default_port: Option<u16>,
    pub drm_root: PathBuf,
    pub legacy_base: String,
    pub token: String,
    /// What `hw.host_ram_gb` reads (`fit::MEMINFO`).
    pub meminfo: PathBuf,
    /// HOST_RAM_RESERVE_GB (`fit::reserve_gb`); `None` when it is not a value this side reads,
    /// and the fit chips are then refused rather than guessed.
    pub ram_reserve_gb: Option<f64>,
}

/// `settings.gpu_vram_map`: `name:int` pairs (whitespace stripped; others skipped).
pub fn gpu_vram_map(raw: &str) -> Result<Vec<(String, f64)>, &'static str> {
    let mut out: Vec<(String, f64)> = Vec::new();
    for pair in raw.split(',') {
        let pair = model_autoconfig::pyval::py_strip(pair);
        let Some((name, val)) = pair.split_once(':') else {
            continue;
        };
        let name = model_autoconfig::pyval::py_strip(name).to_owned();
        let val = model_autoconfig::pyval::py_strip(val);
        let n = match model_autoconfig::pyval::int_str(val, "GPU_VRAM") {
            Ok(n) => n,
            Err(model_autoconfig::Error::Python(_)) => continue,
            Err(_) => return Err("GPU_VRAM holds a value the native sampler cannot read exactly"),
        };
        out.retain(|(k, _)| *k != name);
        out.push((name, n as f64));
    }
    Ok(out)
}

#[derive(Default)]
struct Sampled {
    /// `hw._CACHE`, in insertion order.
    cache: Vec<(String, Stats)>,
    history: HashMap<String, VecDeque<Point>>,
    vendors: HashMap<String, &'static str>,
    restart_errors: HashMap<String, String>,
}

/// The native sampler and `GET /api/v1/backends`.
pub struct BackendStats {
    config: Config,
    docker: Arc<dyn DockerApi>,
    probe: reqwest::Client,
    child: reqwest::Client,
    clock: Clock,
    state: Mutex<Sampled>,
    /// One log line per run of failures, not one per tick.
    push_failing: std::sync::atomic::AtomicBool,
    probe_failing: std::sync::atomic::AtomicBool,
}

/// Logs `message` when `flag` turns on; clears it on success.
fn note(flag: &std::sync::atomic::AtomicBool, failed: bool, message: &str) {
    let was = flag.swap(failed, std::sync::atomic::Ordering::Relaxed);
    if failed && !was {
        eprintln!("{message}");
    }
}

fn lock(m: &Mutex<Sampled>) -> std::sync::MutexGuard<'_, Sampled> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How a GET ends.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(String),
    Failed,
    Refused,
}

impl BackendStats {
    pub fn new(config: Config, docker: Arc<dyn DockerApi>) -> Result<Self, reqwest::Error> {
        Self::with_clock(config, docker, Arc::new(wall))
    }

    pub fn with_clock(
        config: Config,
        docker: Arc<dyn DockerApi>,
        clock: Clock,
    ) -> Result<Self, reqwest::Error> {
        let base = || {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
        };
        Ok(Self {
            config,
            docker,
            // httpx.Timeout(2.0, read=3.0).
            probe: base()
                .connect_timeout(Duration::from_secs(2))
                .read_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()?,
            child: base().timeout(Duration::from_secs(30)).build()?,
            clock,
            state: Mutex::new(Sampled::default()),
            push_failing: std::sync::atomic::AtomicBool::new(false),
            probe_failing: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The child's `_restart_errors` as this side keeps it (set or cleared by the native restart).
    pub fn set_restart_error(&self, name: &str, error: Option<&str>) {
        let mut st = lock(&self.state);
        match error {
            Some(e) => {
                st.restart_errors.insert(name.to_owned(), e.to_owned());
            }
            None => {
                st.restart_errors.remove(name);
            }
        }
    }

    /// Lib and integration tests seed the restart errors as the child's store held them.
    #[doc(hidden)]
    pub fn seed_restart_errors(&self, errors: &[(String, String)]) {
        let mut st = lock(&self.state);
        st.restart_errors = errors.iter().cloned().collect();
    }

    /// Lib and integration tests seed the cache as the child's `hw._CACHE` held it
    /// (`{name: BackendStats}` as `_plain` writes it, `container` null).
    #[doc(hidden)]
    pub fn seed_cache_json(&self, cache: &str) -> Option<()> {
        let J::Obj(pairs) = parse_json(cache.as_bytes())? else {
            return None;
        };
        let mut seeded = Vec::new();
        for (name, v) in pairs {
            let ok = match v.get("ok")? {
                J::Bool(b) => *b,
                _ => return None,
            };
            let error = match v.get("error")? {
                J::Null => None,
                J::Str(s) => Some(s.clone()),
                _ => return None,
            };
            let gpu = match v.get("gpu")? {
                J::Null => None,
                g => Some(gpu_from(g)?),
            };
            if !matches!(v.get("container")?, J::Null) {
                return None;
            }
            seeded.push((
                name,
                Stats {
                    ok,
                    error,
                    gpu,
                    container: None,
                },
            ));
        }
        lock(&self.state).cache = seeded;
        Some(())
    }

    fn declared(&self, name: &str) -> f64 {
        self.config
            .gpu_vram
            .iter()
            .find(|(k, _)| k == name)
            .map_or(0.0, |(_, v)| *v)
    }

    /// The child's `hw.exec_probe(name, probe)`; `None` when it gives none or cannot be asked.
    async fn exec_probe(&self, name: &str, probe: &str) -> Option<Gpu> {
        if !(name.len() <= 256
            && name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)))
        {
            return None;
        }
        let url = format!(
            "{}{PROBE_PATH}?name={name}&probe={probe}",
            self.config.legacy_base
        );
        let mut request = self.child.get(url);
        if !self.config.token.is_empty() {
            request = request.header("x-model-loader-token", &self.config.token);
        }
        let answered = match request.send().await {
            Ok(r) if r.status() == reqwest::StatusCode::OK => r.bytes().await.ok(),
            _ => None,
        };
        let value = answered.as_deref().and_then(parse_json);
        note(
            &self.probe_failing,
            value.is_none(),
            "model-manager backend sampler: the child did not answer a GPU read",
        );
        let value = value?;
        match value.get("gpu")? {
            J::Null => None,
            g => gpu_from(g),
        }
    }

    /// `hw._collect(name)`.
    async fn collect(&self, name: &str) -> Stats {
        let Some(api) = client(self.docker.as_ref()).await else {
            return Stats::failed("docker unreachable");
        };
        let attrs = match inspect(self.docker.as_ref(), &api, name).await {
            Ok(a) => a,
            Err(ReadErr::NotFound) => return Stats::failed("container not found"),
            Err(ReadErr::Api(m)) => return Stats::failed(format!("APIError: {m}")),
            Err(ReadErr::Odd) => return Stats::failed(SAMPLER_ERROR),
        };
        let Some(status) = status_of(&attrs) else {
            return Stats::failed(SAMPLER_ERROR);
        };
        if status != "running" {
            return Stats::failed(format!("container is {status}"));
        }
        let vendor = match self.vendor(&api, name, &attrs).await {
            Ok(v) => v,
            Err(()) => return Stats::failed(SAMPLER_ERROR),
        };
        let root = self.config.drm_root.clone();
        let drm = || {
            let (root, attrs) = (root.clone(), attrs.clone());
            async move {
                tokio::task::spawn_blocking(move || read_drm(&root, &attrs))
                    .await
                    .unwrap_or(Err(()))
            }
        };
        let gpu = match vendor {
            "nvidia" => self.exec_probe(name, "nvidia").await,
            "amd" => match self.exec_probe(name, "amd").await {
                Some(g) => Some(g),
                None => match drm().await {
                    Ok(g) => g,
                    Err(()) => return Stats::failed(SAMPLER_ERROR),
                },
            },
            "vulkan" => match drm().await {
                Err(()) => return Stats::failed(SAMPLER_ERROR),
                Ok(Some(g)) => Some(Gpu {
                    vendor: "vulkan".into(),
                    ..g
                }),
                Ok(None) => match self.exec_probe(name, "amd-nvidia").await {
                    Some(g) => Some(Gpu {
                        vendor: "vulkan".into(),
                        ..g
                    }),
                    None => {
                        let declared = self.declared(name);
                        (declared > 0.0).then(|| Gpu {
                            vendor: "vulkan".into(),
                            name: "Vulkan device (no SMI tool in image — VRAM from GPU_VRAM)"
                                .into(),
                            util_pct: zero(),
                            vram_used_gb: zero(),
                            vram_total_gb: Num::Float(round_nd(declared, 1)),
                            temp_c: zero(),
                            power_w: zero(),
                            gpu_count: 1,
                            cards: Vec::new(),
                            memory_kind: "dedicated".into(),
                            shared_used_gb: zero(),
                            shared_total_gb: zero(),
                            clock_mhz: zero(),
                            source: "declared".into(),
                            measured: false,
                        })
                    }
                },
            },
            _ => None,
        };
        let container = match str_of(attrs.get("Id")) {
            Some(id) if sendable(&id) => {
                match self
                    .docker
                    .get(&format!("/containers/{}/stats?stream=false", quote(&id)))
                    .await
                {
                    Ok(raw) if (200..300).contains(&raw.status) => match parse_json(&raw.body) {
                        Some(stats) => match runtime(&stats) {
                            Ok(r) => r,
                            Err(()) => return Stats::failed(SAMPLER_ERROR),
                        },
                        // requests' JSONDecodeError is not a DockerException.
                        None => return Stats::failed(SAMPLER_ERROR),
                    },
                    Ok(_) => None,
                    Err(_) => return Stats::failed(SAMPLER_ERROR),
                }
            }
            _ => return Stats::failed(SAMPLER_ERROR),
        };
        Stats {
            ok: true,
            error: None,
            gpu,
            container,
        }
    }

    /// `hw._detect_vendor(container)`: from the image tag, kept per name for the process life.
    async fn vendor(&self, api: &str, name: &str, attrs: &J) -> Result<&'static str, ()> {
        if let Some(v) = lock(&self.state).vendors.get(name) {
            return Ok(v);
        }
        let image = match image(self.docker.as_ref(), api, attrs).await {
            Ok(img) => first_tag(&img).ok_or(())?.to_lowercase(),
            Err(ReadErr::Odd) => return Err(()),
            Err(_) => String::new(),
        };
        let vendor = if image.contains("rocm") || image.contains("hip") {
            "amd"
        } else if image.contains("cuda") || image.contains("nvidia") {
            "nvidia"
        } else if image.contains("vulkan") {
            "vulkan"
        } else {
            "unknown"
        };
        lock(&self.state).vendors.insert(name.to_owned(), vendor);
        Ok(vendor)
    }

    /// One pass of `hw._sample_loop` (without the host part, which has its own native owner or
    /// stays with the child), then the push of the cache to the child.
    pub async fn tick(&self) {
        let discovered = discover(self.docker.as_ref()).await;
        let names = effective(&self.config.configured, &discovered);
        for name in &names {
            let stats = self.collect(name).await;
            let ts = (self.clock)();
            let mut st = lock(&self.state);
            match st.cache.iter_mut().find(|(k, _)| k == name) {
                Some((_, slot)) => *slot = stats.clone(),
                None => st.cache.push((name.clone(), stats.clone())),
            }
            if stats.ok {
                let buf = st.history.entry(name.clone()).or_default();
                if buf.len() == HISTORY_MAX {
                    buf.pop_front();
                }
                buf.push_back(point(ts, &stats));
            }
        }
        // Python prunes only names with history (and their cache entries); a name that never
        // sampled ok keeps its cache entry.
        if !names.is_empty() {
            let mut st = lock(&self.state);
            let gone: Vec<String> = st
                .history
                .keys()
                .filter(|k| !names.contains(k))
                .cloned()
                .collect();
            for k in gone {
                st.history.remove(&k);
                st.cache.retain(|(c, _)| *c != k);
            }
        }
        self.push().await;
    }

    /// The cache as the child's `PUT /api/v1/internal/backend-stats` body.
    pub fn push_body(&self) -> Option<String> {
        let st = lock(&self.state);
        let mut out = String::from("{\"backends\":{");
        for (i, (name, stats)) in st.cache.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            s(&mut out, name);
            out.push(':');
            render_stats(&mut out, stats).ok()?;
        }
        out.push_str("}}");
        Some(out)
    }

    async fn push(&self) {
        let Some(body) = self.push_body() else {
            eprintln!("model-manager backend sampler: a sample could not be written for the child");
            return;
        };
        let mut request = self
            .child
            .put(format!("{}{PUSH_PATH}", self.config.legacy_base))
            .header("content-type", "application/json")
            .body(body);
        if !self.config.token.is_empty() {
            request = request.header("x-model-loader-token", &self.config.token);
        }
        let taken = request
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        note(
            &self.push_failing,
            !taken,
            "model-manager backend sampler: the child did not take the backend samples",
        );
    }

    /// Runs the sampler every [`INTERVAL`] until the returned handle is dropped.
    pub fn start(self: &Arc<Self>) -> Sampler {
        let me = self.clone();
        Sampler(tokio::spawn(async move {
            loop {
                if tokio::time::timeout(TICK_BUDGET, me.tick()).await.is_err() {
                    eprintln!("model-manager backend sampler: a tick did not finish in time");
                }
                tokio::time::sleep(INTERVAL).await;
            }
        }))
    }

    /// `GET /api/v1/backends`.
    pub async fn get(&self) -> Outcome {
        match self.answer().await {
            Ok(body) => Outcome::Json(body),
            Err(Fail::Raised) => Outcome::Failed,
            Err(Fail::Refused) => Outcome::Refused,
        }
    }

    /// `services.snapshot_llama_backends()`: the Docker state and the llama-server probes.
    async fn snapshot(&self) -> Result<Vec<Backend>, Fail> {
        let docker = self.docker.as_ref();
        let api = client(docker).await;
        let names = effective(&self.config.configured, &discover(docker).await);
        let mut backends: Vec<Backend> = Vec::new();
        let mut targets: Vec<(usize, Option<u32>)> = Vec::new();
        let mut health: HashMap<usize, String> = HashMap::new();
        match &api {
            None => {
                for n in &names {
                    backends.push(Backend::new(n, "docker unreachable"));
                }
            }
            Some(api) => {
                let errors = lock(&self.state).restart_errors.clone();
                for (i, name) in names.iter().enumerate() {
                    let mut b = Backend::new(name, "not_found");
                    b.last_restart_error = errors.get(name).cloned();
                    if let Some(h) = self.read_backend(api, &mut b).await? {
                        health.insert(i, h);
                    }
                    if b.found && b.status == "running" {
                        targets.push((i, b.internal_port));
                    }
                    backends.push(b);
                }
            }
        }
        let probes = futures_util::future::join_all(targets.iter().map(|(i, port)| {
            let name = names.get(*i).cloned().unwrap_or_default();
            async move {
                match port {
                    None => Err(crate::backends::NoModel::Note("port unknown".into())),
                    Some(p) => crate::backends::probe(&self.probe, &name, *p).await,
                }
            }
        }))
        .await;
        for ((i, _), result) in targets.iter().zip(probes) {
            let Some(b) = backends.get_mut(*i) else {
                continue;
            };
            let (loaded, err) = match result {
                Ok(ids) => (Some(ids), None),
                Err(crate::backends::NoModel::Note(n)) => (None, Some(n)),
                Err(crate::backends::NoModel::Raised) => return Err(Fail::Raised),
            };
            let soft = err.as_deref().is_some_and(soft_note);
            b.probe_error = if loaded.is_some() || soft { None } else { err };
            b.loaded_model = loaded;
            if matches!(
                b.probe_error.as_deref(),
                Some("connection refused" | "timeout")
            ) && health.get(i).map(String::as_str) == Some("healthy")
            {
                b.probe_error = Some(UNREACHABLE_NOTE.into());
                b.unreachable_but_healthy = true;
            }
        }
        Ok(backends)
    }

    async fn answer(&self) -> Result<String, Fail> {
        let backends = self.snapshot().await?;
        let st = lock(&self.state);
        let mut out = String::from("{\"backends\":[");
        for (i, b) in backends.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            render_backend(&mut out, b)?;
            out.push_str(",\"stats\":");
            let warming = Stats::failed("warming up…");
            let stats = st
                .cache
                .iter()
                .find(|(k, _)| *k == b.name)
                .map_or(&warming, |(_, v)| v);
            render_stats(&mut out, stats)?;
            out.push_str(",\"history\":[");
            if let Some(buf) = st.history.get(&b.name) {
                let skip = buf.len().saturating_sub(HISTORY_SHOWN);
                for (j, p) in buf.iter().skip(skip).enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    render_point(&mut out, p)?;
                }
            }
            out.push_str("]}");
        }
        out.push_str("]}");
        Ok(out)
    }

    /// The Docker part of one `LlamaBackend`; the Docker healthcheck status when inspected.
    async fn read_backend(&self, api: &str, b: &mut Backend) -> Result<Option<String>, Fail> {
        let docker = self.docker.as_ref();
        let attrs = match inspect(docker, api, &b.name).await {
            Ok(a) => a,
            Err(ReadErr::NotFound) => return Ok(None),
            Err(ReadErr::Api(m)) => {
                b.status = format!("error: {m}");
                return Ok(None);
            }
            Err(ReadErr::Odd) => return Err(Fail::Refused),
        };
        b.found = true;
        b.status = status_of(&attrs).ok_or(Fail::Refused)?;
        let tag = match image(docker, api, &attrs).await {
            Ok(img) => first_tag(&img).ok_or(Fail::Refused)?,
            // `except NotFound: pass` also catches the image's 404.
            Err(ReadErr::NotFound) => return Ok(None),
            Err(ReadErr::Api(m)) => {
                b.status = format!("error: {m}");
                return Ok(None);
            }
            Err(ReadErr::Odd) => return Err(Fail::Refused),
        };
        b.image = tag;
        b.short_id = short_id(&attrs).ok_or(Fail::Refused)?;
        let state = match attrs.get("State") {
            Some(st @ J::Obj(_)) => st.clone(),
            Some(st) if !st.truthy() => J::Obj(Vec::new()),
            None => J::Obj(Vec::new()),
            Some(_) => return Err(Fail::Refused),
        };
        let started = match state.get("StartedAt") {
            Some(J::Str(s)) => s.clone(),
            // `if not iso`: any falsy value is "no start time".
            Some(v) if !v.truthy() => String::new(),
            None => String::new(),
            Some(_) => return Err(Fail::Refused),
        };
        let parsed = parse_started(&started, (self.clock)())?;
        b.started_at = parsed.local;
        if b.status == "running" {
            b.uptime = (!parsed.up.is_empty()).then_some(parsed.up);
            b.uptime_s = parsed.secs;
        }
        b.host_ports = host_ports(&attrs).map_err(|()| Fail::Refused)?;
        let spec = spec_of(&attrs).map_err(|()| Fail::Refused)?;
        b.internal_port = crate::backends::internal_port(&spec, self.config.default_port);
        let health = match state.get("Health") {
            // `str(... or "")`: only a string can equal "healthy".
            Some(h @ J::Obj(_)) => match h.get("Status") {
                Some(J::Str(s)) => s.clone(),
                _ => String::new(),
            },
            Some(v) if !v.truthy() => String::new(),
            None => String::new(),
            Some(_) => return Err(Fail::Refused),
        };
        Ok(Some(health))
    }
}

/// The generic text of a sample that Python would have ended with an exception's message.
pub const SAMPLER_ERROR: &str = "sampler error: unexpected Docker or sysfs data";

fn render_backend(out: &mut String, b: &Backend) -> Result<(), Fail> {
    let mut first = true;
    out.push('{');
    field(out, &mut first, "name", |o| {
        s(o, &b.name);
        Ok(())
    })?;
    field(out, &mut first, "found", |o| {
        o.push_str(if b.found { "true" } else { "false" });
        Ok(())
    })?;
    for (key, value) in [
        ("status", &b.status),
        ("image", &b.image),
        ("short_id", &b.short_id),
        ("started_at", &b.started_at),
    ] {
        field(out, &mut first, key, |o| {
            s(o, value);
            Ok(())
        })?;
    }
    field(out, &mut first, "uptime", |o| {
        opt_s(o, b.uptime.as_deref());
        Ok(())
    })?;
    field(out, &mut first, "uptime_s", |o| {
        match b.uptime_s {
            Some(n) => o.push_str(&n.to_string()),
            None => o.push_str("null"),
        }
        Ok(())
    })?;
    field(out, &mut first, "host_ports", |o| {
        o.push('[');
        for (i, p) in b.host_ports.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            s(o, p);
        }
        o.push(']');
        Ok(())
    })?;
    field(out, &mut first, "internal_port", |o| {
        match b.internal_port {
            Some(p) => o.push_str(&p.to_string()),
            None => o.push_str("null"),
        }
        Ok(())
    })?;
    field(out, &mut first, "loaded_model", |o| {
        opt_s(o, b.loaded_model.as_deref());
        Ok(())
    })?;
    field(out, &mut first, "probe_error", |o| {
        opt_s(o, b.probe_error.as_deref());
        Ok(())
    })?;
    field(out, &mut first, "unreachable_but_healthy", |o| {
        o.push_str(if b.unreachable_but_healthy {
            "true"
        } else {
            "false"
        });
        Ok(())
    })?;
    field(out, &mut first, "last_restart_error", |o| {
        opt_s(o, b.last_restart_error.as_deref());
        Ok(())
    })?;
    // The closing brace is written by the caller after `stats` and `history`.
    Ok(())
}

/// The running sampler; dropping it stops sampling.
pub struct Sampler(tokio::task::JoinHandle<()>);

impl Drop for Sampler {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests;
