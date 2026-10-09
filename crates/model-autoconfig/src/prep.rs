//! Input prep: autoconfig_core.py's `prepare_all` - analyze()'s reading of the GGUF summary
//! and the backends (`_kv_first_int`, `kv_shape`, `_moe_ratio`, the hostile `int()` and `==`
//! handling), its four early refusals, the main-GPU reservation and the size core's backends.

use crate::kv::{kv_shape_bytes, Shape, SSM_STATE_BYTES};
use crate::pyfloat::{self, f};
use crate::pyval::{
    big, float_of, float_or_zero, get, int_of, int_or, int_value, is_int, iterate,
    lower_starts_with, py_eq, py_str,
};
use crate::{Error, Work, MAX_BLOCK_COUNT};
use model_files::json::Value;
use std::fmt::Write as _;

const CACHE_BYTES: f64 = 1.0625; // q8_0, autoconfig's KV cache type
const MMPROJ_VRAM_MULT: f64 = 1.0;
const MMPROJ_COMPUTE_GB: f64 = 0.5;
const GIB: f64 = 1_073_741_824.0;
/// Largest file size taken as an exact integer (Python divides big ints exactly).
const MAX_EXACT: i128 = 1 << 53;

/// A Python int that may be a bool (`_kv_first_int` hands a bool head count back unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyInt {
    pub value: i128,
    pub is_bool: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SizedBackend {
    pub vram_gb: f64,
    pub gpu_count: i128,
    pub cards: Vec<f64>,
    pub host_ram_gb: f64,
    pub same_as: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Prep {
    BlockCount(i128),
    NoBackends,
    Ram {
        model_gb: f64,
        vram_gb: f64,
        ram_gb: f64,
    },
    UnsizedVram(Vec<String>),
    Kv(Vec<&'static str>),
    /// A hybrid model's per-layer head count with 0 entries that is not fully known (#1159).
    /// `count` is the declared entry count as written (any size of integer).
    KvLayers {
        known: usize,
        count: String,
        layers: i128,
        shared: i128,
    },
    Ok(Box<Prepared>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub n_sessions: i128,
    pub layers: i128,
    pub native_ctx: i128,
    pub hidden: i128,
    pub is_moe: bool,
    pub moe_ratio: f64,
    pub model_gb_raw: f64,
    pub shape: Shape,
    pub kv_heads_is_bool: bool,
    pub sized: Vec<usize>,
    pub mmproj_vram_gb: f64,
    pub backends: Vec<SizedBackend>,
}

fn kv_first_int(v: &Value, default: i128, work: &mut Work) -> Result<PyInt, Error> {
    if is_int(v) {
        return Ok(PyInt {
            value: int_value(v, "attention_head_count_kv")?,
            is_bool: matches!(v, Value::Bool(_)),
        });
    }
    let plain = |value| PyInt {
        value,
        is_bool: false,
    };
    if matches!(v, Value::Obj(_)) && get(v, "_array").truthy() {
        let sample = get(v, "sample");
        if sample.truthy() {
            // counts in first-seen order; max() keeps the first of equal counts.
            let mut counts: Vec<(i128, u64)> = Vec::new();
            for item in iterate(sample, work)? {
                if item == Value::Null {
                    continue;
                }
                let k = int_of(&item, "attention_head_count_kv.sample")?;
                work.charge(counts.len())?;
                match counts.iter_mut().find(|(key, _)| *key == k) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((k, 1)),
                }
            }
            let mut best: Option<(i128, u64)> = None;
            for &(k, n) in &counts {
                if best.is_none_or(|(_, b)| n > b) {
                    best = Some((k, n));
                }
            }
            if let Some((k, _)) = best {
                return Ok(plain(k));
            }
        }
    }
    if let Value::Arr(items) = v {
        if let Some(first) = items.first() {
            if *first != Value::Null {
                return int_of(first, "attention_head_count_kv[0]").map(plain);
            }
        }
    }
    Ok(plain(default))
}

/// `_attention_layers`'s answer.
enum Attn {
    /// Every entry known, one per layer: the attention layers and their largest head count.
    Full { n_attn: i128, max_heads: i128 },
    /// A 0 entry, but not every entry known (or not one per layer).
    Partial { known: usize, count: String },
}

/// `_attention_layers`: a per-layer head count of plain ints with a 0 entry, where llama.cpp
/// gives the 0 layers no KV cache (#1159). None leaves sizing as it was.
fn attention_layers(v: &Value, layers: i128, work: &mut Work) -> Result<Option<Attn>, Error> {
    let (seq, count) = match v {
        Value::Arr(items) => (items, None),
        Value::Obj(_) if matches!(get(v, "_array"), Value::Bool(true)) => {
            match (get(v, "sample"), get(v, "count")) {
                (Value::Arr(items), c @ Value::Int(_)) => (items, Some(c)),
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    work.charge(seq.len())?;
    let mut has_zero = false;
    for x in seq {
        match x {
            Value::Int(i) => has_zero |= i.is_zero(),
            _ => return Ok(None),
        }
    }
    if !has_zero {
        return Ok(None);
    }
    let len = seq.len();
    let len_i = i128::try_from(len).map_err(|_| Error::OutOfRange("attention_head_count_kv"))?;
    let (count_n, count_s) = match count {
        None => (Some(len_i), len.to_string()),
        Some(c @ Value::Int(i)) => (big(c, "count").ok(), i.to_string()),
        Some(_) => return Ok(None),
    };
    if count_n == Some(len_i) && len_i == layers {
        let mut n_attn: i128 = 0;
        let mut max_heads: Option<i128> = None;
        for x in seq {
            let h = int_value(x, "attention_head_count_kv")?;
            if h != 0 {
                n_attn += 1;
                max_heads = Some(max_heads.map_or(h, |m| m.max(h)));
            }
        }
        return Ok(Some(Attn::Full {
            n_attn,
            max_heads: max_heads.unwrap_or(0),
        }));
    }
    Ok(Some(Attn::Partial {
        known: len,
        count: count_s,
    }))
}

const OVERFLOW: Error = Error::OutOfRange("ssm state bytes");

/// `_ssm_layer_bytes`: one non-attention layer's recurrent state per sequence, never below
/// SSM_STATE_BYTES. Checked arithmetic: a hostile header's sizes refuse rather than wrap.
fn ssm_layer_bytes(
    state: Option<i128>,
    inner: Option<i128>,
    conv: Option<i128>,
    groups: Option<i128>,
    embed: i128,
) -> Result<i128, Error> {
    let ok = |v: Option<i128>| v.filter(|&x| x > 0);
    let mul = |a: i128, b: i128| a.checked_mul(b).ok_or(OVERFLOW);
    let add = |a: i128, b: i128| a.checked_add(b).ok_or(OVERFLOW);
    if ok(state).is_none() && ok(inner).is_none() {
        return Ok(SSM_STATE_BYTES);
    }
    let need = match (ok(state), ok(inner), ok(conv), ok(groups)) {
        (Some(st), Some(inn), Some(cv), Some(g)) => {
            let conv_w = add(inn, mul(mul(2, g)?, st)?)?;
            mul(4, add(mul(st, inn)?, mul(cv - 1, conv_w)?)?)?
        }
        _ => {
            let st = ok(state).unwrap_or(128);
            let inn = match ok(inner) {
                Some(v) => v,
                None => mul(2, embed)?,
            };
            let cv = ok(conv).unwrap_or(4);
            let g = ok(groups).unwrap_or(8);
            let ssm = add(mul(mul(mul(4, st)?, inn)?, 11)?, 9)?.div_euclid(10);
            let conv_w = add(inn, mul(mul(2, g)?, st)?)?;
            add(ssm, mul(mul(4, cv - 1)?, conv_w)?)?
        }
    };
    Ok(need.max(SSM_STATE_BYTES))
}

fn moe_ratio(experts: &Value) -> Result<f64, Error> {
    if !is_int(experts) {
        return Ok(0.0);
    }
    let n = int_value(experts, "expert_count")?;
    Ok(if n < 2 {
        0.0
    } else if n >= 64 {
        0.92
    } else if n >= 32 {
        0.90
    } else if n >= 16 {
        0.85
    } else if n >= 8 {
        0.78
    } else {
        0.65
    })
}

/// `int(v) if isinstance(v, int) else None`.
fn opt_int(v: &Value, what: &'static str) -> Result<Option<i128>, Error> {
    if is_int(v) {
        int_value(v, what).map(Some)
    } else {
        Ok(None)
    }
}

/// `_pattern_sample`: the sample of a per-layer array that describes this stack, else [].
fn pattern_sample(v: &Value, layers: i128, work: &mut Work) -> Result<Vec<Value>, Error> {
    if matches!(v, Value::Obj(_)) && get(v, "_array").truthy() {
        if int_or(get(v, "count"), 0, "sliding_window_pattern.count")? == layers {
            let sample = get(v, "sample");
            return if sample.truthy() {
                iterate(sample, work)
            } else {
                Ok(Vec::new())
            };
        }
        return Ok(Vec::new());
    }
    if let Value::Arr(items) = v {
        work.charge(items.len())?;
        return Ok(items.clone());
    }
    Ok(Vec::new())
}

/// `_period_of`: the shortest p with seq[i] == seq[i % p] for every i.
fn period_of(seq: &[Value], work: &mut Work) -> Result<usize, Error> {
    for p in 1..=seq.len() {
        let mut all = true;
        for (i, item) in seq.iter().enumerate() {
            let other = seq.get(i % p).ok_or(Error::Schema("period"))?;
            if !py_eq(item, other, work)? {
                all = false;
                break;
            }
        }
        if all {
            return Ok(p);
        }
    }
    Ok(seq.len())
}

/// `max(1, int(x))` as a float, with `int()`'s TypeError and ValueError falling back to
/// `kv_heads` (what kv_shape catches); other exceptions propagate.
fn period_heads(x: &Value, kv_heads: i128) -> Result<f64, Error> {
    match x {
        // int(x) of a finite float is exact, so float(max(1, int(x))) is max(1, trunc(x)).
        Value::Float(v) if v.is_nan() => Ok(f(kv_heads)),
        Value::Float(v) if v.is_infinite() => Err(Error::Python("OverflowError")),
        Value::Float(v) => Ok(if v.trunc() > 1.0 { v.trunc() } else { 1.0 }),
        other => match int_of(other, "attention_head_count_kv.sample") {
            Ok(n) => Ok(f(n.max(1))),
            Err(Error::Python("TypeError" | "ValueError")) => Ok(f(kv_heads)),
            Err(e) => Err(e),
        },
    }
}

struct Swa<'a> {
    window: Option<i128>,
    pattern: &'a Value,
    k_swa: Option<i128>,
    v_swa: Option<i128>,
    shared: Option<i128>,
    heads_pattern: &'a Value,
}

#[allow(clippy::too_many_arguments)]
fn kv_shape(
    gemma: bool,
    layers: i128,
    kv_heads: i128,
    head_dim: i128,
    key_length: Option<i128>,
    value_length: Option<i128>,
    full_attention_interval: Option<i128>,
    swa: &Swa<'_>,
    work: &mut Work,
) -> Result<Option<Shape>, Error> {
    if !(layers > 0 && kv_heads > 0) {
        return Ok(None);
    }
    let k_dim = key_length.filter(|&k| k != 0).unwrap_or(head_dim);
    let v_dim = value_length.filter(|&v| v != 0).unwrap_or(head_dim);
    if k_dim <= 0 || v_dim <= 0 {
        return Ok(None);
    }
    let mut shape = Shape {
        gemma,
        layers,
        kv_heads,
        k_dim,
        v_dim,
        hybrid_interval: None,
        window: None,
        k_swa: None,
        v_swa: None,
        shared: None,
        period: None,
        recurrent_bytes: None,
    };
    if let Some(interval) = full_attention_interval.filter(|&i| i > 1) {
        shape.hybrid_interval = Some(interval);
        return Ok(Some(shape));
    }
    if let Some(window) = swa.window.filter(|&w| w > 0) {
        let pattern = pattern_sample(swa.pattern, layers, work)?;
        let heads_seq = pattern_sample(swa.heads_pattern, layers, work)?;
        shape.shared = Some(swa.shared.unwrap_or(0).min(layers).max(0));
        shape.k_swa = Some(swa.k_swa.filter(|&k| k != 0).unwrap_or(k_dim));
        shape.v_swa = Some(swa.v_swa.filter(|&v| v != 0).unwrap_or(v_dim));
        shape.window = Some(window);
        if !pattern.is_empty() {
            let p = period_of(&pattern, work)?;
            let mut period = Vec::with_capacity(p);
            for (i, local) in pattern.iter().take(p).enumerate() {
                let h = match heads_seq.get(i) {
                    Some(x) => period_heads(x, kv_heads)?,
                    None => f(kv_heads),
                };
                period.push((local.truthy(), h));
            }
            shape.period = Some(period);
        }
        return Ok(Some(shape));
    }
    if !(gemma && layers >= 6) {
        shape.shared = Some(swa.shared.unwrap_or(0));
    }
    Ok(Some(shape))
}

fn main_gpu_reserve_gb(
    has_mmproj: bool,
    mmproj_gb: f64,
    mtp_gb: f64,
    layers: i128,
    hidden: i128,
) -> f64 {
    let mut gb = if has_mmproj {
        mmproj_gb * MMPROJ_VRAM_MULT + MMPROJ_COMPUTE_GB
    } else {
        0.0
    };
    gb += mtp_gb * 1.15;
    if has_mmproj && layers > 0 && hidden > 0 {
        // max(0, 1024 - 512): the vision ubatch above llama.cpp's default 512.
        let extra_ub = 512.0;
        gb += 7.0 * extra_ub * f(layers) * f(hidden) / 1e9;
    }
    gb
}

fn size_backends(sized: &[&Value], work: &mut Work) -> Result<Vec<SizedBackend>, Error> {
    let mut out = Vec::with_capacity(sized.len());
    for b in sized {
        let vram_gb = float_of(
            b.get("vram_gb").ok_or(Error::Python("KeyError"))?,
            "vram_gb",
        )?;
        let gpu_count = match b.get("gpu_count") {
            None => 1,
            Some(v) => int_of(v, "gpu_count")?,
        }
        .max(1);
        let cards_v = get(b, "card_vram_gb");
        let mut cards = Vec::new();
        if cards_v.truthy() {
            for c in iterate(cards_v, work)? {
                cards.push(match c {
                    Value::Bool(_) | Value::Int(_) | Value::Float(_) => {
                        float_of(&c, "card_vram_gb")?
                    }
                    _ => return Err(Error::Python("TypeError")),
                });
            }
        }
        let host_ram_gb = float_or_zero(get(b, "host_ram_gb"), "host_ram_gb")?;
        let mut same_as = None;
        for (j, o) in sized.iter().enumerate() {
            let on = o.get("name").ok_or(Error::Python("KeyError"))?;
            let bn = b.get("name").ok_or(Error::Python("KeyError"))?;
            if py_eq(on, bn, work)? {
                same_as = Some(j);
                break;
            }
        }
        out.push(SizedBackend {
            vram_gb,
            gpu_count,
            cards,
            host_ram_gb,
            same_as: same_as.ok_or(Error::Python("StopIteration"))?,
        });
    }
    Ok(out)
}

const PREP_KEYS: [&str; 6] = [
    "n_sessions",
    "arch",
    "model",
    "file_size",
    "backends",
    "projector",
];
const PROJECTOR_KEYS: [&str; 3] = ["has_mmproj", "mmproj_gb", "mtp_gb"];

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value, Error> {
    v.get(key).ok_or(Error::Schema("prep: missing field"))
}

fn number(v: &Value, what: &'static str) -> Result<f64, Error> {
    match v {
        Value::Int(_) | Value::Float(_) => float_of(v, what),
        _ => Err(Error::Schema(what)),
    }
}

/// `prepare_all` of a prep request.
pub fn prepare(inp: &Value, work: &mut Work) -> Result<Prep, Error> {
    let Value::Obj(pairs) = inp else {
        return Err(Error::Schema("prep"));
    };
    if pairs.iter().any(|(k, _)| !PREP_KEYS.contains(&k.as_str())) {
        return Err(Error::Schema("prep"));
    }
    let n_sessions = int_or(field(inp, "n_sessions")?, 1, "n_sessions")?.clamp(1, 8);
    let arch = field(inp, "arch")?;
    let gemma = if !arch.truthy() {
        false
    } else if matches!(arch, Value::Str(_)) {
        lower_starts_with(arch, "gemma")
    } else {
        return Err(Error::Python("AttributeError"));
    };
    let model = field(inp, "model")?;
    let empty = Value::Obj(Vec::new());
    let m = if model.truthy() { model } else { &empty };
    if !matches!(m, Value::Obj(_)) {
        return Err(Error::Python("AttributeError"));
    }
    let layers = int_or(get(m, "block_count"), 0, "block_count")?;
    if !(0..=MAX_BLOCK_COUNT).contains(&layers) {
        return Ok(Prep::BlockCount(layers));
    }
    let heads = int_or(get(m, "attention_head_count"), 1, "attention_head_count")?;
    let embed = int_or(get(m, "embedding_length"), 0, "embedding_length")?;
    let head_dim = if heads > 0 {
        embed.div_euclid(heads)
    } else {
        0
    };
    let kv_heads = kv_first_int(get(m, "attention_head_count_kv"), heads, work)?;
    let native_ctx = int_or(get(m, "context_length"), 0, "context_length")?;
    let experts = get(m, "expert_count");
    let key_length = opt_int(get(m, "key_length"), "key_length")?;
    let value_length = opt_int(get(m, "value_length"), "value_length")?;
    let fai = opt_int(get(m, "full_attention_interval"), "full_attention_interval")?;
    // ssm_state_size only ever decides alongside full_attention_interval > 1 (see kv_shape), so
    // it is not read.
    let swa = Swa {
        window: opt_int(get(m, "sliding_window"), "sliding_window")?,
        pattern: get(m, "sliding_window_pattern"),
        k_swa: opt_int(get(m, "key_length_swa"), "key_length_swa")?,
        v_swa: opt_int(get(m, "value_length_swa"), "value_length_swa")?,
        shared: opt_int(get(m, "shared_kv_layers"), "shared_kv_layers")?,
        heads_pattern: get(m, "attention_head_count_kv"),
    };
    let model_gb_raw = match field(inp, "file_size")? {
        Value::Bool(b) => f(i128::from(*b)) / GIB,
        v @ Value::Int(_) => {
            let n = crate::pyval::big(v, "file_size")?;
            if n.abs() > MAX_EXACT {
                return Err(Error::OutOfRange("file_size"));
            }
            f(n) / GIB
        }
        Value::Float(x) => x / GIB,
        _ => return Err(Error::Python("TypeError")),
    };
    let moe = moe_ratio(experts)?;
    let is_moe = is_int(experts) && int_value(experts, "expert_count")? > 1;

    let backends_v = field(inp, "backends")?;
    if !backends_v.truthy() {
        return Ok(Prep::NoBackends);
    }
    let backends: Vec<&Value> = match backends_v {
        Value::Arr(items) => items.iter().collect(),
        // Iterating a string or a dict gives strings, whose .get() Python lacks.
        Value::Str(_) | Value::Obj(_) => return Err(Error::Python("AttributeError")),
        _ => return Err(Error::Python("TypeError")),
    };
    work.charge(backends.len())?;
    let mut ram: Option<f64> = None;
    for b in &backends {
        if !matches!(b, Value::Obj(_)) {
            return Err(Error::Python("AttributeError"));
        }
        let x = float_or_zero(get(b, "host_ram_gb"), "host_ram_gb")?;
        ram = Some(ram.map_or(x, |a| pyfloat::max(a, x)));
    }
    let mut vram: Option<f64> = None;
    for b in &backends {
        let x = float_or_zero(get(b, "vram_gb"), "vram_gb")?;
        vram = Some(vram.map_or(x, |a| pyfloat::max(a, x)));
    }
    let (ram, vram) = (ram.unwrap_or(0.0), vram.unwrap_or(0.0));
    if ram > 0.0 && model_gb_raw > vram + ram {
        return Ok(Prep::Ram {
            model_gb: model_gb_raw,
            vram_gb: vram,
            ram_gb: ram,
        });
    }
    let mut sized = Vec::new();
    for (i, b) in backends.iter().enumerate() {
        if float_or_zero(get(b, "vram_gb"), "vram_gb")? > 0.0 {
            sized.push(i);
        }
    }
    if sized.is_empty() {
        let mut names = Vec::with_capacity(backends.len());
        for b in &backends {
            let n = get(b, "name");
            names.push(if n.truthy() {
                py_str(n, "backend name")?
            } else {
                "?".to_owned()
            });
        }
        return Ok(Prep::UnsizedVram(names));
    }

    // Per-layer heads with 0 entries (#1159): only the attention layers hold KV. Left to the
    // interleaved-attention and sliding-window paths when those are declared.
    let (mut shape_layers, mut shape_heads) = (layers, kv_heads.value);
    if fai.is_none_or(|i| i <= 1) && swa.window.is_none_or(|w| w <= 0) {
        match attention_layers(get(m, "attention_head_count_kv"), layers, work)? {
            Some(Attn::Partial { known, count }) => {
                return Ok(Prep::KvLayers {
                    known,
                    count,
                    layers,
                    shared: 0,
                })
            }
            Some(Attn::Full { .. }) if swa.shared.is_some_and(|n| n > 0) => {
                return Ok(Prep::KvLayers {
                    known: usize::try_from(layers).map_err(|_| Error::OutOfRange("block_count"))?,
                    count: layers.to_string(),
                    layers,
                    shared: swa.shared.unwrap_or(0),
                })
            }
            Some(Attn::Full { n_attn, max_heads }) => {
                shape_layers = n_attn;
                shape_heads = max_heads;
            }
            None => {}
        }
    }
    let shape = kv_shape(
        gemma,
        shape_layers,
        shape_heads,
        head_dim,
        key_length,
        value_length,
        fai,
        &swa,
        work,
    )?;
    let mut shape = shape;
    if let Some(s) = shape.as_mut() {
        if shape_layers != layers || s.hybrid_interval.is_some() {
            // Non-attention layers still hold a per-sequence recurrent state (#1159). The keys
            // are read only here, as Python reads them, so other models never see them.
            let per = ssm_layer_bytes(
                opt_int(get(m, "ssm_state_size"), "ssm_state_size")?,
                opt_int(get(m, "ssm_inner_size"), "ssm_inner_size")?,
                opt_int(get(m, "ssm_conv_kernel"), "ssm_conv_kernel")?,
                opt_int(get(m, "ssm_group_count"), "ssm_group_count")?,
                embed,
            )?;
            let per_seq = per.checked_mul(n_sessions).ok_or(OVERFLOW)?;
            match s.hybrid_interval {
                None => {
                    s.recurrent_bytes = Some(
                        (layers - shape_layers)
                            .checked_mul(per_seq)
                            .ok_or(OVERFLOW)?,
                    );
                }
                Some(interval) => {
                    let full = ((layers + interval - 1).div_euclid(interval)).max(1);
                    let extra = (layers - full)
                        .checked_mul(per_seq - SSM_STATE_BYTES)
                        .ok_or(OVERFLOW)?;
                    if extra > 0 {
                        s.recurrent_bytes = Some(extra);
                    }
                }
            }
        }
    }
    let kv = match &shape {
        Some(s) => kv_shape_bytes(s, 4096, CACHE_BYTES, CACHE_BYTES, work)?,
        None => 0.0,
    };
    let shape = match shape {
        Some(s) if kv > 0.0 => s,
        _ => {
            let missing = [
                ("block_count", layers),
                ("attention_head_count_kv", shape_heads),
                (
                    "head_dim (embedding_length / attention_head_count)",
                    head_dim,
                ),
            ]
            .into_iter()
            .filter(|&(_, v)| v == 0)
            .map(|(k, _)| k)
            .collect();
            return Ok(Prep::Kv(missing));
        }
    };

    let pj = field(inp, "projector")?;
    let Value::Obj(pj_pairs) = pj else {
        return Err(Error::Schema("projector"));
    };
    if pj_pairs
        .iter()
        .any(|(k, _)| !PROJECTOR_KEYS.contains(&k.as_str()))
    {
        return Err(Error::Schema("projector"));
    }
    let has_mmproj = match field(pj, "has_mmproj")? {
        Value::Bool(b) => *b,
        _ => return Err(Error::Schema("projector.has_mmproj")),
    };
    let mmproj_gb = number(field(pj, "mmproj_gb")?, "projector.mmproj_gb")?;
    let mtp_gb = number(field(pj, "mtp_gb")?, "projector.mtp_gb")?;
    let reserve = main_gpu_reserve_gb(has_mmproj, mmproj_gb, mtp_gb, layers, embed);
    let sized_v: Vec<&Value> = sized
        .iter()
        .filter_map(|&i| backends.get(i).copied())
        .collect();
    let sized_backends = size_backends(&sized_v, work)?;
    Ok(Prep::Ok(Box::new(Prepared {
        n_sessions,
        layers,
        native_ctx,
        hidden: embed,
        is_moe,
        moe_ratio: moe,
        model_gb_raw,
        shape,
        kv_heads_is_bool: kv_heads.is_bool,
        sized,
        mmproj_vram_gb: reserve,
        backends: sized_backends,
    })))
}

fn opt(out: &mut String, v: Option<i128>) {
    match v {
        Some(n) => {
            let _ = write!(out, "{n}");
        }
        None => out.push_str("null"),
    }
}

/// The answer as the JSON object `prepare_all` returns (key order aside).
pub fn prep_json(p: &Prep) -> String {
    let mut out = String::new();
    match p {
        Prep::BlockCount(n) => {
            let _ = write!(out, "{{\"refuse\":\"block_count\",\"layers\":{n}}}");
        }
        Prep::NoBackends => out.push_str("{\"refuse\":\"no_backends\"}"),
        Prep::Ram {
            model_gb,
            vram_gb,
            ram_gb,
        } => {
            let _ = write!(
                out,
                "{{\"refuse\":\"ram\",\"model_gb\":{},\"vram_gb\":{},\"ram_gb\":{}}}",
                pyfloat::json(*model_gb),
                pyfloat::json(*vram_gb),
                pyfloat::json(*ram_gb)
            );
        }
        Prep::UnsizedVram(names) => {
            out.push_str("{\"refuse\":\"unsized_vram\",\"names\":[");
            for (i, n) in names.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                crate::json_str(&mut out, n);
            }
            out.push_str("]}");
        }
        Prep::Kv(missing) => {
            out.push_str("{\"refuse\":\"kv\",\"missing\":[");
            for (i, n) in missing.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                crate::json_str(&mut out, n);
            }
            out.push_str("]}");
        }
        Prep::KvLayers {
            known,
            count,
            layers,
            shared,
        } => {
            let _ = write!(
                out,
                "{{\"refuse\":\"kv_layers\",\"known\":{known},\"count\":{count},\"layers\":{layers},\"shared\":{shared}}}"
            );
        }
        Prep::Ok(p) => {
            let s = &p.shape;
            let kv_heads = if p.kv_heads_is_bool {
                (s.kv_heads != 0).to_string()
            } else {
                s.kv_heads.to_string()
            };
            let _ = write!(
                out,
                "{{\"refuse\":null,\"n_sessions\":{},\"layers\":{},\"native_ctx\":{},\"hidden\":{},\"is_moe\":{},\"moe_ratio\":{},\"model_gb_raw\":{},\"shape\":{{\"gemma\":{},\"layers\":{},\"kv_heads\":{kv_heads},\"k_dim\":{},\"v_dim\":{},\"hybrid_interval\":",
                p.n_sessions,
                p.layers,
                p.native_ctx,
                p.hidden,
                p.is_moe,
                pyfloat::json(p.moe_ratio),
                pyfloat::json(p.model_gb_raw),
                s.gemma,
                s.layers,
                s.k_dim,
                s.v_dim
            );
            opt(&mut out, s.hybrid_interval);
            out.push_str(",\"window\":");
            opt(&mut out, s.window);
            out.push_str(",\"k_swa\":");
            opt(&mut out, s.k_swa);
            out.push_str(",\"v_swa\":");
            opt(&mut out, s.v_swa);
            out.push_str(",\"shared\":");
            opt(&mut out, s.shared);
            out.push_str(",\"period\":");
            match &s.period {
                None => out.push_str("null"),
                Some(period) => {
                    out.push('[');
                    for (i, (local, h)) in period.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        let _ = write!(out, "[{local},{}]", pyfloat::json(*h));
                    }
                    out.push(']');
                }
            }
            if let Some(rec) = s.recurrent_bytes {
                let _ = write!(out, ",\"recurrent_bytes\":{rec}");
            }
            out.push_str("},\"sized\":[");
            for (i, n) in p.sized.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{n}");
            }
            let _ = write!(
                out,
                "],\"mmproj_vram_gb\":{},\"backends\":[",
                pyfloat::json(p.mmproj_vram_gb)
            );
            for (i, b) in p.backends.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(
                    out,
                    "{{\"vram_gb\":{},\"gpu_count\":{},\"cards\":[",
                    pyfloat::json(b.vram_gb),
                    b.gpu_count
                );
                for (j, c) in b.cards.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push_str(&pyfloat::json(*c));
                }
                let _ = write!(
                    out,
                    "],\"host_ram_gb\":{},\"same_as\":{}}}",
                    pyfloat::json(b.host_ram_gb),
                    b.same_as
                );
            }
            out.push_str("]}");
        }
    }
    out
}
