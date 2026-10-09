//! noevia model-manager's autoconfig (sbstndalton/noevia, MODEL_AUTOCONFIG), ported in slices
//! from noevia-services `model-manager/app/autoconfig_core.py`:
//!
//! * the size core ([`size_plan`], `size_plan` there): from a model's KV shape, its size and the
//!   GPU backends, which backend and context to recommend, the offload presets, and the prompt
//!   cache;
//! * input prep ([`prep`], `prepare_all`): the GGUF summary's fields read with Python's `int()`
//!   and `==`, the KV shape, the early refusals, the main-GPU reservation, the sized backends;
//! * values assembly ([`values`], `assemble_values`): the settings a recommendation writes;
//! * speculative-decoding profiles ([`spec`], `resolve_spec`), the companion-file name rules
//!   ([`files`], `pick_file`: which projector and draft head belong to a model, from a listing
//!   Python read), the baseline parser ([`baseline`], `parse_baseline`) and the report beside
//!   the values ([`present`], `present`: baseline redundancy, quirks, the saved preset, the diff,
//!   the displaced keys, the quality warnings).
//!
//! Each matches Python to the bit on every input inside the caps below (floats included: the
//! arithmetic is done in the same order, and Python's `round` and `int` are reproduced exactly;
//! see [`pyfloat`], [`pyval`] and [`pystr`]). `model-autoconfig check` answers them all in one
//! process.
//!
//! Python stays authoritative: the service uses its own plan, and only when this one agrees
//! exactly (or Python's is the smaller one). This crate's job is to agree, or to refuse.
//!
//! Pure: no network, filesystem or clock. Bounded: [`MAX_INPUT_BYTES`], the shape caps
//! (block count, period, backends, GPUs, integer magnitude) and a work budget ([`MAX_WORK`])
//! that refuses a request before it can run long. Inputs past a cap are refused with a typed
//! [`Error`]; a refusal is never a smaller answer, the caller treats it as "cannot confirm".

#![forbid(unsafe_code)]

pub mod baseline;
pub mod core;
pub mod files;
pub mod kv;
pub mod prep;
pub mod present;
pub mod pyfloat;
pub mod pystr;
pub mod pyval;
pub mod spec;
pub mod values;

use crate::core::{Offload, Plan, PresetOption};
use crate::kv::Shape;
use model_files::json::{self, Value};
use std::fmt;
use std::fmt::Write as _;

/// Largest request accepted, in bytes of JSON.
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
/// autoconfig.py's MAX_BLOCK_COUNT: analyze() refuses larger block counts before sizing.
pub const MAX_BLOCK_COUNT: i128 = 4096;
/// Longest repeating sliding-window period accepted.
pub const MAX_PERIOD: usize = 65_536;
/// Most backends, GPUs per backend, and per-card entries accepted.
pub const MAX_BACKENDS: usize = 64;
pub const MAX_GPUS: i128 = 64;
pub const MAX_CARDS: usize = 256;
/// Steps of work (fit-search iterations, KV evaluations, per-card passes) one request may use.
pub const MAX_WORK: u64 = 200_000_000;
/// Steps (comparisons, iterated items) input prep or values assembly may use. Real per-layer
/// samples hold at most 8 items (gguf_meta keeps no more), so this is generous; it only stops a
/// hostile request from running long.
pub const MAX_PREP_WORK: u64 = 2_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input is larger than [`MAX_INPUT_BYTES`].
    InputTooLarge,
    /// The input is not UTF-8.
    NotUtf8,
    /// The input is not JSON.
    Json(String),
    /// A field is missing, of the wrong type, or inconsistent.
    Schema(&'static str),
    /// A value is outside the range this port handles (see the caps above).
    OutOfRange(&'static str),
    /// The request would take more than [`MAX_WORK`] steps.
    WorkLimit,
    /// Python's answer here would need semantics this port does not reproduce (a non-ASCII
    /// digit string, a NaN inside a compared value, ...): refused rather than guessed.
    Unsupported(&'static str),
    /// Python raises here (the exception's type). The service never asks Rust about a request
    /// on which Python raised, but the differential fixtures record these cases too.
    Python(&'static str),
}

impl Error {
    /// A short stable code for the CLI's error line.
    pub fn code(&self) -> String {
        match self {
            Error::InputTooLarge => "input_too_large".to_owned(),
            Error::NotUtf8 => "not_utf8".to_owned(),
            Error::Json(_) => "json".to_owned(),
            Error::Schema(_) => "schema".to_owned(),
            Error::OutOfRange(_) => "out_of_range".to_owned(),
            Error::WorkLimit => "work_limit".to_owned(),
            Error::Unsupported(_) => "unsupported".to_owned(),
            Error::Python(kind) => format!("python:{kind}"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InputTooLarge => write!(f, "input larger than {MAX_INPUT_BYTES} bytes"),
            Error::NotUtf8 => write!(f, "input is not UTF-8"),
            Error::Json(m) => write!(f, "not JSON: {m}"),
            Error::Schema(m) => write!(f, "bad request: {m}"),
            Error::OutOfRange(m) => write!(f, "out of range: {m}"),
            Error::WorkLimit => write!(f, "more than {MAX_WORK} steps of work"),
            Error::Unsupported(m) => write!(f, "not reproduced exactly: {m}"),
            Error::Python(kind) => write!(f, "the Python reference raises {kind} here"),
        }
    }
}

impl std::error::Error for Error {}

/// A countdown of the steps one request may take.
#[derive(Debug)]
pub struct Work {
    left: u64,
}

impl Work {
    pub fn new(limit: u64) -> Self {
        Work { left: limit }
    }

    pub fn charge(&mut self, n: usize) -> Result<(), Error> {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        self.left = self.left.checked_sub(n).ok_or(Error::WorkLimit)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    pub vram_gb: f64,
    pub gpu_count: i128,
    pub cards: Vec<f64>,
    pub host_ram_gb: f64,
    pub same_as: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub shape: Shape,
    pub layers: i128,
    pub native_ctx: i128,
    pub model_gb_raw: f64,
    pub moe_ratio: f64,
    pub is_moe: bool,
    pub mmproj_vram_gb: f64,
    pub n_sessions: i128,
    pub backends: Vec<Backend>,
    pub preset: String,
    pub prompt_tps: f64,
    pub prompt_budget_s: f64,
    pub verified_ctx: i128,
    pub cache_ram_cap_mib: i128,
}

const REQUEST_KEYS: [&str; 14] = [
    "shape",
    "layers",
    "native_ctx",
    "model_gb_raw",
    "moe_ratio",
    "is_moe",
    "mmproj_vram_gb",
    "n_sessions",
    "backends",
    "preset",
    "prompt_tps",
    "prompt_budget_s",
    "verified_ctx",
    "cache_ram_cap_mib",
];
const SHAPE_KEYS: [&str; 12] = [
    "gemma",
    "layers",
    "kv_heads",
    "k_dim",
    "v_dim",
    "hybrid_interval",
    "window",
    "k_swa",
    "v_swa",
    "shared",
    "period",
    "recurrent_bytes",
];
const BACKEND_KEYS: [&str; 5] = ["vram_gb", "gpu_count", "cards", "host_ram_gb", "same_as"];
const PRESET_KEYS: [&str; 3] = ["fast", "balanced", "long-ctx"];

fn obj<'a>(
    v: &'a Value,
    keys: &[&str],
    what: &'static str,
) -> Result<&'a [(String, Value)], Error> {
    let Value::Obj(pairs) = v else {
        return Err(Error::Schema(what));
    };
    if pairs.iter().any(|(k, _)| !keys.contains(&k.as_str())) {
        return Err(Error::Schema(what));
    }
    Ok(pairs)
}

fn field<'a>(v: &'a Value, key: &str, what: &'static str) -> Result<&'a Value, Error> {
    v.get(key).ok_or(Error::Schema(what))
}

/// A Python int (a JSON integer, or a bool, which Python ints include), within [`INT_LIMIT`].
///
/// [`INT_LIMIT`]: pyfloat::INT_LIMIT
fn int(v: &Value, what: &'static str) -> Result<i128, Error> {
    match v {
        Value::Bool(b) => Ok(i128::from(*b)),
        Value::Int(i) => {
            let n: i128 = i.to_string().parse().map_err(|_| Error::OutOfRange(what))?;
            if n.abs() >= pyfloat::INT_LIMIT {
                return Err(Error::OutOfRange(what));
            }
            Ok(n)
        }
        _ => Err(Error::Schema(what)),
    }
}

fn opt_int(v: &Value, what: &'static str) -> Result<Option<i128>, Error> {
    match v {
        Value::Null => Ok(None),
        other => int(other, what).map(Some),
    }
}

/// A Python float. Integers are refused: Python's int arithmetic differs from float arithmetic
/// past 2^53, and the service always sends floats here.
fn float(v: &Value, what: &'static str) -> Result<f64, Error> {
    match v {
        Value::Float(x) => Ok(*x),
        _ => Err(Error::Schema(what)),
    }
}

fn boolean(v: &Value, what: &'static str) -> Result<bool, Error> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(Error::Schema(what)),
    }
}

fn parse_shape(v: &Value) -> Result<Shape, Error> {
    obj(v, &SHAPE_KEYS, "shape")?;
    let g = |k: &str| field(v, k, "shape: missing field");
    let period = match g("period")? {
        Value::Null => None,
        Value::Arr(items) => {
            if items.len() > MAX_PERIOD {
                return Err(Error::OutOfRange("shape.period"));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Arr(pair) => match pair.as_slice() {
                        [local, heads] => out.push((
                            boolean(local, "shape.period")?,
                            float(heads, "shape.period")?,
                        )),
                        _ => return Err(Error::Schema("shape.period")),
                    },
                    _ => return Err(Error::Schema("shape.period")),
                }
            }
            Some(out)
        }
        _ => return Err(Error::Schema("shape.period")),
    };
    let shape = Shape {
        gemma: boolean(g("gemma")?, "shape.gemma")?,
        layers: int(g("layers")?, "shape.layers")?,
        kv_heads: int(g("kv_heads")?, "shape.kv_heads")?,
        k_dim: int(g("k_dim")?, "shape.k_dim")?,
        v_dim: int(g("v_dim")?, "shape.v_dim")?,
        hybrid_interval: opt_int(g("hybrid_interval")?, "shape.hybrid_interval")?,
        window: opt_int(g("window")?, "shape.window")?,
        k_swa: opt_int(g("k_swa")?, "shape.k_swa")?,
        v_swa: opt_int(g("v_swa")?, "shape.v_swa")?,
        shared: opt_int(g("shared")?, "shape.shared")?,
        period,
        // Optional: only a hybrid model's attention-only shape carries it (#1159).
        recurrent_bytes: match v.get("recurrent_bytes") {
            None => None,
            Some(x) => opt_int(x, "shape.recurrent_bytes")?,
        },
    };
    if shape.recurrent_bytes.is_some_and(|r| r < 0) {
        return Err(Error::Schema("shape.recurrent_bytes"));
    }
    // kv_shape() returns None (and analyze() refuses) unless these hold.
    if !(shape.layers > 0 && shape.kv_heads > 0 && shape.k_dim > 0 && shape.v_dim > 0) {
        return Err(Error::Schema("shape: not sizeable"));
    }
    if shape.layers > MAX_BLOCK_COUNT {
        return Err(Error::OutOfRange("shape.layers"));
    }
    if let Some(shared) = shape.shared {
        if shape.window.is_some() && !(0..=shape.layers).contains(&shared) {
            return Err(Error::Schema("shape.shared"));
        }
    }
    Ok(shape)
}

fn parse_backend(v: &Value, index: usize) -> Result<Backend, Error> {
    obj(v, &BACKEND_KEYS, "backend")?;
    let g = |k: &str| field(v, k, "backend: missing field");
    let cards = match g("cards")? {
        Value::Arr(items) if items.len() <= MAX_CARDS => items
            .iter()
            .map(|c| float(c, "backend.cards"))
            .collect::<Result<Vec<_>, _>>()?,
        Value::Arr(_) => return Err(Error::OutOfRange("backend.cards")),
        _ => return Err(Error::Schema("backend.cards")),
    };
    let gpu_count = int(g("gpu_count")?, "backend.gpu_count")?;
    if !(1..=MAX_GPUS).contains(&gpu_count) {
        return Err(Error::OutOfRange("backend.gpu_count"));
    }
    let vram_gb = float(g("vram_gb")?, "backend.vram_gb")?;
    // analyze() only sizes backends whose VRAM is above zero (so never NaN).
    if vram_gb.is_nan() || vram_gb <= 0.0 {
        return Err(Error::Schema("backend.vram_gb"));
    }
    let same_as = usize::try_from(int(g("same_as")?, "backend.same_as")?)
        .map_err(|_| Error::Schema("backend.same_as"))?;
    if same_as > index {
        return Err(Error::Schema("backend.same_as"));
    }
    Ok(Backend {
        vram_gb,
        gpu_count,
        cards,
        host_ram_gb: float(g("host_ram_gb")?, "backend.host_ram_gb")?,
        same_as,
    })
}

/// Read a request (the JSON autoconfig_core.py's analyze() hands to `size_plan`).
pub fn parse_request(input: &[u8]) -> Result<Request, Error> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| Error::NotUtf8)?;
    let v = json::parse(text).map_err(|e| Error::Json(e.to_string()))?;
    parse_value(&v)
}

/// Read a request already parsed as JSON.
pub fn parse_value(v: &Value) -> Result<Request, Error> {
    obj(v, &REQUEST_KEYS, "request")?;
    let g = |k: &str| field(v, k, "request: missing field");
    let backends = match g("backends")? {
        Value::Arr(items) if !items.is_empty() && items.len() <= MAX_BACKENDS => items
            .iter()
            .enumerate()
            .map(|(i, b)| parse_backend(b, i))
            .collect::<Result<Vec<_>, _>>()?,
        Value::Arr(_) => return Err(Error::OutOfRange("backends")),
        _ => return Err(Error::Schema("backends")),
    };
    // same_as points at the first backend of that name, which is its own first.
    for b in &backends {
        if backends.get(b.same_as).map(|o| o.same_as) != Some(b.same_as) {
            return Err(Error::Schema("backend.same_as"));
        }
    }
    let preset = match g("preset")? {
        Value::Str(s) if s.is_empty() || PRESET_KEYS.contains(&s.as_str()) => s.clone(),
        _ => return Err(Error::Schema("preset")),
    };
    let req = Request {
        shape: parse_shape(g("shape")?)?,
        layers: int(g("layers")?, "layers")?,
        native_ctx: int(g("native_ctx")?, "native_ctx")?,
        model_gb_raw: float(g("model_gb_raw")?, "model_gb_raw")?,
        moe_ratio: float(g("moe_ratio")?, "moe_ratio")?,
        is_moe: boolean(g("is_moe")?, "is_moe")?,
        mmproj_vram_gb: float(g("mmproj_vram_gb")?, "mmproj_vram_gb")?,
        n_sessions: int(g("n_sessions")?, "n_sessions")?,
        backends,
        preset,
        prompt_tps: float(g("prompt_tps")?, "prompt_tps")?,
        prompt_budget_s: float(g("prompt_budget_s")?, "prompt_budget_s")?,
        verified_ctx: int(g("verified_ctx")?, "verified_ctx")?,
        cache_ram_cap_mib: int(g("cache_ram_cap_mib")?, "cache_ram_cap_mib")?,
    };
    // The shape covers the layers that hold KV: all of them, or a hybrid model's attention
    // layers only (#1159), never more than the stack.
    if !(0..=MAX_BLOCK_COUNT).contains(&req.layers) || req.shape.layers > req.layers {
        return Err(Error::Schema("layers"));
    }
    if !(1..=8).contains(&req.n_sessions) {
        return Err(Error::OutOfRange("n_sessions"));
    }
    Ok(req)
}

/// The size plan of a request, or why it cannot be given.
pub fn size_plan(req: &Request) -> Result<Plan, Error> {
    let mut work = Work::new(MAX_WORK);
    core::size_plan(req, &mut work)
}

pub(crate) fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || !c.is_ascii() => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_opt_int(out: &mut String, v: Option<i128>) {
    match v {
        Some(n) => {
            let _ = write!(out, "{n}");
        }
        None => out.push_str("null"),
    }
}

fn json_preset(out: &mut String, p: &PresetOption) {
    out.push_str("{\"key\":");
    json_str(out, &p.key);
    out.push_str(",\"label\":");
    json_str(out, &p.label);
    out.push_str(",\"icon\":");
    json_str(out, p.icon);
    let _ = write!(
        out,
        ",\"ctx\":{},\"n_cpu_moe\":{},\"offload_kind\":\"{}\",\"gpu_layers\":{},\"total_layers\":{},\"gpu_gb\":{},\"kv_gb\":{},\"speed_score\":{},\"ngl\":{}}}",
        p.ctx,
        p.n_cpu_moe,
        p.offload_kind.as_str(),
        p.gpu_layers,
        p.total_layers,
        pyfloat::json(p.gpu_gb),
        pyfloat::json(p.kv_gb),
        pyfloat::json(p.speed_score),
        p.ngl
    );
}

fn json_list<T>(out: &mut String, items: &[T], each: impl Fn(&mut String, &T)) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        each(out, item);
    }
    out.push(']');
}

/// The plan as the JSON object autoconfig_core.py's `size_plan` returns (key order aside).
pub fn plan_json(plan: &Plan) -> String {
    let mut out = String::from("{\"plans\":");
    json_list(&mut out, &plan.plans, |out, p| {
        out.push_str("{\"rows\":");
        json_list(out, &p.rows, |out, r| {
            let _ = write!(
                out,
                "{{\"ctx\":{},\"total_ctx\":{},\"model_gb\":{},\"kv_gb\":{},\"total_gb\":{},\"fits\":{},\"free_gb\":{},\"offload_kind\":\"{}\",\"n_cpu_moe\":{},\"gpu_pct\":{}}}",
                r.ctx,
                r.total_ctx,
                pyfloat::json(r.model_gb),
                pyfloat::json(r.kv_gb),
                pyfloat::json(r.total_gb),
                r.fits,
                pyfloat::json(r.free_gb),
                r.offload_kind.as_str(),
                r.n_cpu_moe,
                r.gpu_pct
            );
        });
        let _ = write!(out, ",\"max_ctx\":{}}}", p.max_ctx);
    });
    out.push_str(",\"recommended\":");
    json_opt_int(&mut out, plan.recommended.map(|r| r as i128));
    let (kind, n): (Offload, i128) = plan.offload;
    let _ = write!(
        out,
        ",\"offload\":[\"{}\",{}],\"estimated_ctx\":{},\"initial_ctx\":{},\"capped_ctx\":{},\"cap\":\"{}\",\"sized\":{},\"ctx\":{}",
        kind.as_str(),
        n,
        plan.estimated_ctx,
        plan.initial_ctx,
        plan.capped_ctx,
        plan.cap,
        plan.sized,
        plan.ctx
    );
    out.push_str(",\"presets\":");
    json_list(&mut out, &plan.presets, json_preset);
    out.push_str(",\"frontier\":");
    json_list(&mut out, &plan.frontier, json_preset);
    out.push_str(",\"active_preset\":");
    json_str(&mut out, &plan.active_preset);
    let _ = write!(out, ",\"fits_full_gpu\":{},\"ngl\":", plan.fits_full_gpu);
    json_opt_int(&mut out, plan.ngl);
    let _ = write!(out, ",\"fit\":{},\"cache_ram\":", plan.fit);
    json_opt_int(&mut out, plan.cache_ram);
    out.push('}');
    out
}

/// Request JSON in, plan JSON out: what `model-autoconfig size` does.
pub fn size_plan_json(input: &[u8]) -> Result<String, Error> {
    let req = parse_request(input)?;
    size_plan(&req).map(|p| plan_json(&p))
}

const CHECK_KEYS: [&str; 7] = [
    "prep", "size", "values", "spec", "files", "present", "baseline",
];

/// A JSON value written back the way Python's json.dumps writes it (floats as [`pyfloat::json`]).
pub(crate) fn json_value(out: &mut String, v: &Value) -> Result<(), Error> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(x) => out.push_str(&pyfloat::json(*x)),
        Value::Str(s) => json_str(out, s),
        // Only scalars are ever echoed back.
        Value::Arr(_) | Value::Obj(_) => {
            return Err(Error::Schema("an echoed value is not a scalar"))
        }
    }
    Ok(())
}

fn sep(out: &mut String) {
    if out.len() > 1 {
        out.push(',');
    }
}

/// `model-autoconfig check`: a request `{"prep"?, "size"?, "values"?, "spec"?, "files"?,
/// "present"?, "baseline"?}` (each part absent or null to skip it) in; an object with the parts
/// asked for out, as autoconfig_core.py's `check_reference` answers. Any part's refusal refuses
/// the whole.
pub fn check_json(input: &[u8]) -> Result<String, Error> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| Error::NotUtf8)?;
    let v = json::parse(text).map_err(|e| Error::Json(e.to_string()))?;
    let pairs = obj(&v, &CHECK_KEYS, "check")?;
    let mut seen: Vec<&str> = Vec::new();
    for (k, _) in pairs {
        if seen.contains(&k.as_str()) {
            return Err(Error::Schema("check: a duplicated part"));
        }
        seen.push(k);
    }
    let part = |k: &str| v.get(k).filter(|p| **p != Value::Null);
    let mut out = String::from("{");
    if let Some(p) = part("prep") {
        let mut work = Work::new(MAX_PREP_WORK);
        out.push_str("\"prep\":");
        out.push_str(&prep::prep_json(&prep::prepare(p, &mut work)?));
    }
    if let Some(p) = part("size") {
        if out.len() > 1 {
            out.push(',');
        }
        // The size part is the very request `model-autoconfig size` reads.
        let req = parse_value(p)?;
        out.push_str("\"size\":");
        out.push_str(&plan_json(&size_plan(&req)?));
    }
    if let Some(p) = part("values") {
        if out.len() > 1 {
            out.push(',');
        }
        out.push_str("\"values\":");
        out.push_str(&values::values_json(&values::assemble(p)?));
    }
    if let Some(p) = part("spec") {
        let mut work = Work::new(MAX_PREP_WORK);
        let r = spec::resolve(p, &mut work)?;
        sep(&mut out);
        out.push_str("\"spec\":");
        out.push_str(&spec::resolved_json(&r));
    }
    if let Some(p) = part("files") {
        let mut work = Work::new(MAX_PREP_WORK);
        let answers = files::pick_all(p, &mut work)?;
        sep(&mut out);
        out.push_str("\"files\":");
        out.push_str(&files::answers_json(&answers)?);
    }
    if let Some(p) = part("present") {
        let mut work = Work::new(MAX_PREP_WORK);
        let r = present::present(p, &mut work)?;
        sep(&mut out);
        out.push_str("\"present\":");
        out.push_str(&present::report_json(&r));
    }
    if let Some(p) = part("baseline") {
        let mut work = Work::new(MAX_PREP_WORK);
        let parsed = baseline::parse_all(p, &mut work)?;
        sep(&mut out);
        out.push_str("\"baseline\":[");
        for (i, v) in parsed.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&values::values_json(v));
        }
        out.push(']');
    }
    out.push('}');
    Ok(out)
}
