//! noevia-core's `server/llamacpp-autoconfig.cjs` in Rust, exported from `dav-parse.wasm`
//! (LLAMACPP_AUTOCONFIG_IMPL). The pure sizing logic only; reading the GGUF header, file sizes,
//! presets and the budget stays in JS, and nothing here starts llama.cpp or loads weights.
//!
//! - [`kv_cache_bytes`]: `kvCacheBytes` (hybrid attention + SSM, sliding window with or without a
//!   per-layer pattern, shared-KV layers, plain attention) and `draftKvBytes` (MTP heads).
//! - `suggest`: the largest qualified context whose estimate fits the budget, with the prompt
//!   cache, projector and MTP knobs and the notes, or the JS's error.
//! - `estimateInputs`: the read-only ingredients of the "Will it fit?" panel.
//! - `estimateFootprint`: what loading one preset costs (#697), the load gate's estimate.
//! - `cacheRamMibOf`, `isPromptCacheFree`, `parseMemoryLimit`.
//!
//! Every answer is written exactly as `JSON.stringify` writes the JS's answer (same key order,
//! `Number#toString`, non-finite numbers as `null`), so the host compares the texts byte for byte.
//! The host (noevia-core) always computes the JS answer and returns it; this port only confirms it.
//!
//! # JS semantics
//!
//! Arithmetic is f64 in the JS's evaluation order; `Math.min`/`Math.max` propagate NaN and order
//! `-0` below `+0`; `Math.round` rounds half up; `Number(…)` is ECMAScript `StringToNumber`
//! (Unicode whitespace trimmed, hex/octal/binary literals, `Infinity`); `String(…)` of a number is
//! `Number#toString`. The `/i` regexes are not Unicode-aware, so their case folding is ASCII-only;
//! `toLowerCase` can only produce the ASCII keys compared here from ASCII input (no non-ASCII code
//! point lowercases to only ASCII letters among `t r u e o n` or the cache-type keys). A property
//! read on a JSON value is the object's own member (no key read here is an `Object.prototype`
//! name) or `undefined`; the cache-type table *is* read by a computed key, so `constructor` and
//! `__proto__` give the JS's NaN scale.
//!
//! # Refusals
//!
//! [`Refusal::Input`] where the JS throws (`current`/`options` explicitly `null` when read), or the
//! request is not an object; [`Refusal::TooLarge`] past [`MAX_INPUT_BYTES`].
//!
//! # Stricter than the JS
//!
//! [`Refusal::Ambiguous`] (the host then treats the port's answer as missing) where the JS answers
//! but the port does not model it:
//!
//! - a model metadata number field (`contextLength`, `blockCount`, … as gguf-meta `summarize`
//!   writes them: a number or `null`) holding anything else, or an `arch` that is not a string;
//! - a `headCountKv` or `slidingWindowPattern` array holding an array or object;
//! - an object or array where the JS converts a value with `Number(…)`/`String(…)` (byte sizes,
//!   the budget, `cacheRamMaxMib`, the preset options read, the model name);
//! - a hexadecimal/octal/binary option literal past 128 bits;
//! - a sliding-window pattern whose period is over [`MAX_PATTERN_PERIOD`] (bounds the work);
//! - a successful suggestion whose native context is not an integer up to 2^53 - 1: the note
//!   formats it with `toLocaleString('en-US')`, whose digits past that depend on ICU (#1115).
//!
//! Linear time, bounded input and output, no panics.

#![forbid(unsafe_code)]
// `!(a > b)` is deliberate throughout: it is the JS's own test, true for NaN.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

use prompt_framing::js::trim;
use prompt_framing::json::{self, Value};
use std::collections::HashMap;
use tool_exchange::js_number;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024 + 1;
/// The largest sliding-window pattern period sized (the JS walks one period per context size).
pub const MAX_PATTERN_PERIOD: usize = 4096;
/// Parse depth of a request: args (0), meta/current (1), a metadata array (2), its items (3).
const WIRE_DEPTH: usize = 6;

/// llamacpp-autoconfig.cjs CTX_CANDIDATES.
pub const CTX_CANDIDATES: [f64; 52] = [
    4096.0, 8192.0, 12288.0, 16384.0, 24576.0, 32768.0, 40960.0, 49152.0, 57344.0, 65536.0,
    73728.0, 81920.0, 90112.0, 98304.0, 106496.0, 114688.0, 122880.0, 131072.0, 139264.0, 147456.0,
    151552.0, 155648.0, 159744.0, 163840.0, 172032.0, 180224.0, 188416.0, 196608.0, 204800.0,
    212992.0, 221184.0, 229376.0, 237568.0, 245760.0, 253952.0, 262144.0, 294912.0, 327680.0,
    360448.0, 393216.0, 425984.0, 458752.0, 491520.0, 524288.0, 589824.0, 655360.0, 720896.0,
    786432.0, 851968.0, 917504.0, 983040.0, 1048576.0,
];
const MIN_CTX: f64 = 4096.0;
const Q8_BYTES: f64 = 1.0625;
const RESERVE_GIB: f64 = 1.0;
const SSM_STATE_BYTES: f64 = 4.0 * 1024.0 * 1024.0;
const MMPROJ_COMPUTE_GIB: f64 = 0.5;
const IMAGE_MAX_TOKENS: f64 = 1024.0;
const SAFETY: f64 = 1.05;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// llama-server's own --cache-ram default (MiB) when a preset sets none (#697).
pub const LLAMA_CACHE_RAM_DEFAULT_MIB: f64 = 8192.0;
/// llamacpp-autoconfig.cjs KV_TYPE_BYTES.
const KV_TYPE_BYTES: [(&str, f64); 9] = [
    ("f32", 4.0),
    ("f16", 2.0),
    ("bf16", 2.0),
    ("q8_0", 1.0625),
    ("q5_1", 0.75),
    ("q5_0", 0.6875),
    ("q4_1", 0.625),
    ("q4_0", 0.5625),
    ("iq4_nl", 0.5625),
];
const SOURCE: &str = "Adapted from Model Loader autoconfig (scratchhax/model-loader, MIT).";

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The JS throws, or the request is not what the host sends.
    Input,
    /// Past [`MAX_INPUT_BYTES`].
    TooLarge,
    /// The JS answers, but the port does not model it (see the crate docs).
    Ambiguous,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

type R<T> = Result<T, Refusal>;

// ── JS number and string semantics ──────────────────────────────────────────────────────────

/// `Math.min(a, b)`.
pub fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_negative() {
            a
        } else {
            b
        }
    } else if a < b {
        a
    } else {
        b
    }
}

/// `Math.max(a, b)`.
pub fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a == b {
        if a.is_sign_positive() {
            a
        } else {
            b
        }
    } else if a > b {
        a
    } else {
        b
    }
}

/// `Math.round(x)`: half rounds up; `-0` and `(-0.5, 0)` give `-0`.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let f = x.floor();
    // Exact: x - floor(x) is representable for every finite double.
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    if r == 0.0 && x < 0.0 {
        -0.0
    } else {
        r
    }
}

fn round2(n: f64) -> f64 {
    js_round(n * 100.0) / 100.0
}

/// `String(n)` for any number.
pub fn number_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n == f64::INFINITY {
        "Infinity".into()
    } else if n == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        js_number(n)
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `StringToNumber(s)` (ECMAScript 7.1.4.1.1); a radix literal past 128 bits is refused.
pub fn string_to_number(s: &[u16]) -> R<f64> {
    let t = trim(s);
    if t.is_empty() {
        return Ok(0.0);
    }
    if t.iter().any(|&c| c >= 0x80) {
        return Ok(f64::NAN);
    }
    let t: String = t.iter().map(|&c| char::from(c as u8)).collect();
    for (prefix, radix) in [("0x", 16u32), ("0o", 8), ("0b", 2)] {
        let lower = t.get(..2).map(str::to_ascii_lowercase);
        if lower.as_deref() == Some(prefix) {
            let digits = t.get(2..).unwrap_or("");
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return Ok(f64::NAN);
            }
            let mut n: u128 = 0;
            for c in digits.chars() {
                let d = u128::from(c.to_digit(radix).unwrap_or(0));
                n = n
                    .checked_mul(u128::from(radix))
                    .and_then(|n| n.checked_add(d))
                    .ok_or(Refusal::Ambiguous)?;
            }
            // u128 -> f64 rounds to nearest, ties to even, as the spec's MV rounding does.
            return Ok(n as f64);
        }
    }
    let (neg, body) = match t.as_bytes().first() {
        Some(b'+') => (false, t.get(1..).unwrap_or("")),
        Some(b'-') => (true, t.get(1..).unwrap_or("")),
        _ => (false, t.as_str()),
    };
    if body == "Infinity" {
        return Ok(if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    // StrUnsignedDecimalLiteral: digits [. [digits]] [exp] | . digits [exp].
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (
            body.get(..i).unwrap_or(""),
            Some(body.get(i + 1..).unwrap_or("")),
        ),
        None => (body, None),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = |x: &str| x.bytes().all(|c| c.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
        return Ok(f64::NAN);
    }
    let exp = match exp {
        Some(e) => {
            let unsigned = e.strip_prefix(['+', '-']).unwrap_or(e);
            if unsigned.is_empty() || !digits(unsigned) {
                return Ok(f64::NAN);
            }
            e
        }
        None => "0",
    };
    let int = if int.is_empty() { "0" } else { int };
    let frac = if frac.is_empty() { "0" } else { frac };
    let text = format!("{}{int}.{frac}e{exp}", if neg { "-" } else { "" });
    Ok(text.parse::<f64>().unwrap_or(f64::NAN))
}

/// A JSON value where the JS converts it as a primitive; arrays and objects are refused.
#[derive(Clone, Copy, Debug)]
enum Scalar<'a> {
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    Str(&'a [u16]),
}

fn scalar(v: Option<&Value>) -> R<Scalar<'_>> {
    Ok(match v {
        None => Scalar::Undef,
        Some(Value::Null) => Scalar::Null,
        Some(Value::Bool(b)) => Scalar::Bool(*b),
        Some(Value::Num(n)) => Scalar::Num(*n),
        Some(Value::Str(s)) => Scalar::Str(s),
        Some(Value::Arr(_) | Value::Obj(_) | Value::Deep) => return Err(Refusal::Ambiguous),
    })
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Num(n)) => *n != 0.0 && !n.is_nan(),
        Some(Value::Str(s)) => !s.is_empty(),
        Some(Value::Arr(_) | Value::Obj(_) | Value::Deep) => true,
    }
}

/// `Number(v)` / ToNumber.
fn to_number(s: Scalar<'_>) -> R<f64> {
    Ok(match s {
        Scalar::Undef => f64::NAN,
        Scalar::Null => 0.0,
        Scalar::Bool(b) => f64::from(u8::from(b)),
        Scalar::Num(n) => n,
        Scalar::Str(s) => string_to_number(s)?,
    })
}

/// `String(v)` / ToString.
fn to_string(s: Scalar<'_>) -> Vec<u16> {
    match s {
        Scalar::Undef => units("undefined"),
        Scalar::Null => units("null"),
        Scalar::Bool(b) => units(if b { "true" } else { "false" }),
        Scalar::Num(n) => units(&number_string(n)),
        Scalar::Str(s) => s.to_vec(),
    }
}

/// `Number(v) || 0`.
fn number_or_zero(v: Option<&Value>) -> R<f64> {
    let n = to_number(scalar(v)?)?;
    Ok(if n == 0.0 || n.is_nan() { 0.0 } else { n })
}

/// Case-insensitive (ASCII, as a non-Unicode `/i` regex) substring test; `needle` is lowercase.
fn contains_ci(hay: &[u16], needle: &str) -> bool {
    let n: Vec<u16> = units(needle);
    if n.is_empty() {
        return true;
    }
    hay.windows(n.len()).any(|w| {
        w.iter().zip(&n).all(|(&a, &b)| {
            let a = if (0x41..=0x5a).contains(&a) {
                a + 0x20
            } else {
                a
            };
            a == b
        })
    })
}

/// `s.toLowerCase()` when it can only match an ASCII key: `None` for non-ASCII text.
fn ascii_lower(s: &[u16]) -> Option<String> {
    if s.iter().any(|&c| c >= 0x80) {
        return None;
    }
    Some(
        s.iter()
            .map(|&c| char::from(c as u8).to_ascii_lowercase())
            .collect(),
    )
}

// ── Model metadata ──────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    Num(u64),
    Str(Vec<u16>),
    Bool(bool),
    Null,
}

/// A scalar array item: its `===`/SameValueZero key, truthiness and ToNumber.
fn item(v: &Value) -> R<(Key, bool, f64)> {
    Ok(match v {
        Value::Null => (Key::Null, false, 0.0),
        Value::Bool(b) => (Key::Bool(*b), *b, f64::from(u8::from(*b))),
        // JSON carries no NaN; -0 and +0 are one key for both comparisons.
        Value::Num(n) => (
            Key::Num(if *n == 0.0 { 0 } else { n.to_bits() }),
            *n != 0.0,
            *n,
        ),
        Value::Str(s) => (Key::Str(s.clone()), !s.is_empty(), string_to_number(s)?),
        Value::Arr(_) | Value::Obj(_) | Value::Deep => return Err(Refusal::Ambiguous),
    })
}

#[derive(Clone, Debug, Default)]
enum Heads {
    /// `typeof headCountKv === 'number'`.
    Num(f64),
    /// An array: the most frequent item (first seen on ties) and every item, as numbers.
    Arr { mode: Option<f64>, seq: Vec<f64> },
    #[default]
    Other,
}

#[derive(Clone, Debug)]
struct Pattern {
    len: usize,
    period: usize,
    truthy: Vec<bool>,
}

/// gguf-meta `summarize()` output as the sizing reads it (absent and `null` numbers are 0, which
/// every use here treats the same as the JS's `undefined`/`null`).
#[derive(Clone, Debug, Default)]
pub struct Meta {
    arch: Vec<u16>,
    has_chat_template: bool,
    context_length: f64,
    embedding_length: f64,
    block_count: f64,
    head_count: f64,
    key_length: f64,
    value_length: f64,
    key_length_swa: f64,
    value_length_swa: f64,
    sliding_window: f64,
    shared_kv_layers: f64,
    full_attention_interval: f64,
    expert_count: f64,
    nextn_predict_layers: f64,
    heads: Heads,
    pattern: Option<Pattern>,
}

/// The smallest `p` with `seq[i] === seq[i % p]` for every `i` (periodOf), in linear time: the
/// length less the longest proper border (prefix function).
fn period_of(keys: &[Key]) -> usize {
    let n = keys.len();
    let mut pi = vec![0usize; n];
    for i in 1..n {
        let mut k = pi.get(i - 1).copied().unwrap_or(0);
        while k > 0 && keys.get(i) != keys.get(k) {
            k = pi.get(k - 1).copied().unwrap_or(0);
        }
        if keys.get(i) == keys.get(k) {
            k += 1;
        }
        if let Some(slot) = pi.get_mut(i) {
            *slot = k;
        }
    }
    n - pi.last().copied().unwrap_or(0)
}

impl Meta {
    /// `meta || {}` read from a JSON value.
    pub fn from_value(v: Option<&Value>) -> R<Meta> {
        let obj = match v {
            Some(Value::Deep) => return Err(Refusal::Ambiguous),
            Some(o @ Value::Obj(_)) => o,
            _ => return Ok(Meta::default()),
        };
        let num = |key: &str| -> R<f64> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(0.0),
                Some(Value::Num(n)) => Ok(*n),
                Some(_) => Err(Refusal::Ambiguous),
            }
        };
        let arch = match obj.get("arch") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Str(s)) => s.clone(),
            Some(_) => return Err(Refusal::Ambiguous),
        };
        let heads = match obj.get("headCountKv") {
            Some(Value::Num(n)) => Heads::Num(*n),
            Some(Value::Arr(items)) => {
                let mut counts: HashMap<Key, (usize, usize)> = HashMap::new();
                let mut seq = Vec::with_capacity(items.len());
                for (i, x) in items.iter().enumerate() {
                    let (k, _, n) = item(x)?;
                    counts.entry(k).or_insert((0, i)).0 += 1;
                    seq.push(n);
                }
                let best = counts
                    .values()
                    .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
                    .map(|&(_, first)| first);
                let mode = best.and_then(|i| seq.get(i).copied());
                Heads::Arr { mode, seq }
            }
            _ => Heads::Other,
        };
        let pattern = match obj.get("slidingWindowPattern") {
            Some(Value::Arr(items)) => {
                let mut keys = Vec::with_capacity(items.len());
                let mut truthy = Vec::with_capacity(items.len());
                for x in items {
                    let (k, t, _) = item(x)?;
                    keys.push(k);
                    truthy.push(t);
                }
                Some(Pattern {
                    len: items.len(),
                    period: period_of(&keys),
                    truthy,
                })
            }
            _ => None,
        };
        let m = Meta {
            arch,
            has_chat_template: truthy(obj.get("hasChatTemplate")),
            context_length: num("contextLength")?,
            embedding_length: num("embeddingLength")?,
            block_count: num("blockCount")?,
            head_count: num("headCount")?,
            key_length: num("keyLength")?,
            value_length: num("valueLength")?,
            key_length_swa: num("keyLengthSwa")?,
            value_length_swa: num("valueLengthSwa")?,
            sliding_window: num("slidingWindow")?,
            shared_kv_layers: num("sharedKvLayers")?,
            full_attention_interval: num("fullAttentionInterval")?,
            expert_count: num("expertCount")?,
            nextn_predict_layers: num("nextnPredictLayers")?,
            heads,
            pattern,
        };
        // The pattern loop runs once per period per context size: bound it where it is reached.
        if let Some(p) = &m.pattern {
            let reached = p.len as f64 == m.block_count
                && m.sliding_window > 0.0
                && !(m.full_attention_interval > 1.0);
            if reached && p.period > MAX_PATTERN_PERIOD {
                return Err(Refusal::Ambiguous);
            }
        }
        Ok(m)
    }

    /// `kvHeadsOf(m.headCountKv, fallback)`, as a number.
    fn kv_heads(&self, fallback: f64) -> f64 {
        match &self.heads {
            Heads::Num(v) if *v > 0.0 => *v,
            Heads::Arr { mode: Some(m), .. } => *m,
            _ => fallback,
        }
    }

    fn head_seq(&self, layers: f64) -> Option<&[f64]> {
        match &self.heads {
            Heads::Arr { seq, .. } if seq.len() as f64 == layers => Some(seq),
            _ => None,
        }
    }

    fn native(&self) -> f64 {
        self.context_length
    }

    fn is_bert(&self) -> bool {
        contains_ci(&self.arch, "bert")
    }
}

/// `scalar(v) || fallback`.
fn positive_or(v: f64, fallback: f64) -> f64 {
    if v > 0.0 {
        v
    } else {
        fallback
    }
}

/// kvCacheBytes(m, ctx); with `draft`, of `{ ...m, blockCount: 1, fullAttentionInterval: null,
/// slidingWindow: null, sharedKvLayers: 0 }` (draftKvBytes's per-layer figure).
fn kv_bytes(m: &Meta, ctx: f64, draft: bool) -> f64 {
    let layers = if draft { 1.0 } else { m.block_count };
    let heads = m.head_count;
    let head_dim = if heads != 0.0 {
        (m.embedding_length / heads).floor()
    } else {
        0.0
    };
    let kv_heads = m.kv_heads(heads);
    let k_dim = positive_or(m.key_length, head_dim);
    let v_dim = positive_or(m.value_length, head_dim);
    if !(ctx > 0.0 && layers > 0.0 && kv_heads > 0.0 && k_dim > 0.0 && v_dim > 0.0) {
        return 0.0;
    }
    let per_layer_token = kv_heads * (k_dim + v_dim) * Q8_BYTES;
    let interval = if draft {
        0.0
    } else {
        m.full_attention_interval
    };
    if interval != 0.0 && interval > 1.0 {
        let full = js_max(1.0, (layers / interval).ceil());
        return full * per_layer_token * ctx + (layers - full) * SSM_STATE_BYTES;
    }
    let shared_raw = if draft { 0.0 } else { m.shared_kv_layers };
    let shared = js_max(0.0, js_min(shared_raw, layers));
    let sw = if draft { 0.0 } else { m.sliding_window };
    if sw != 0.0 && sw > 0.0 {
        let alloc_frac = (layers - shared) / layers;
        let local_elem = (positive_or(m.key_length_swa, k_dim)
            + positive_or(m.value_length_swa, v_dim))
            * Q8_BYTES;
        let global_elem = (k_dim + v_dim) * Q8_BYTES;
        let window = js_min(sw, ctx);
        let pattern = m.pattern.as_ref().filter(|p| p.len as f64 == layers);
        let head_seq = m.head_seq(layers);
        if let Some(p) = pattern {
            let reps = (layers / p.period as f64) * alloc_frac;
            let mut total = 0.0;
            for i in 0..p.period {
                let h = match head_seq {
                    Some(seq) => js_max(1.0, seq.get(i).copied().unwrap_or(f64::NAN)),
                    None => kv_heads,
                };
                total += if p.truthy.get(i).copied().unwrap_or(false) {
                    reps * h * local_elem * window
                } else {
                    reps * h * global_elem * ctx
                };
            }
            return total;
        }
        let global = js_max(1.0, (layers / 6.0).floor());
        let local = layers - global;
        return alloc_frac
            * (global * kv_heads * global_elem * ctx + local * kv_heads * local_elem * window);
    }
    js_max(1.0, layers - js_min(shared, layers - 1.0)) * per_layer_token * ctx
}

/// `kvCacheBytes(m, ctx)`.
pub fn kv_cache_bytes(m: &Meta, ctx: f64) -> f64 {
    kv_bytes(m, ctx, false)
}

/// `draftKvBytes(m, ctx)`.
pub fn draft_kv_bytes(m: &Meta, ctx: f64) -> f64 {
    let n = m.nextn_predict_layers;
    if n == 0.0 || m.block_count == 0.0 {
        return 0.0;
    }
    n * kv_bytes(m, ctx, true)
}

// ── Preset options ──────────────────────────────────────────────────────────────────────────

/// `current`/`options`: an object's members, `undefined` for any other value, a throw for `null`.
#[derive(Clone, Copy)]
enum Opts<'a> {
    Obj(&'a Value),
    Empty,
    Null,
}

impl<'a> Opts<'a> {
    /// The parameter `name = {}` of `args`.
    fn of(args: &'a Value, name: &str) -> R<Opts<'a>> {
        Ok(match args.get(name) {
            None => Opts::Empty,
            Some(Value::Null) => Opts::Null,
            Some(Value::Deep) => return Err(Refusal::Ambiguous),
            Some(o @ Value::Obj(_)) => Opts::Obj(o),
            Some(_) => Opts::Empty,
        })
    }

    /// `opts[key]`; reading a property of `null` throws.
    fn get(self, key: &str) -> R<Option<&'a Value>> {
        match self {
            Opts::Obj(o) => Ok(o.get(key)),
            Opts::Empty => Ok(None),
            Opts::Null => Err(Refusal::Input),
        }
    }
}

/// `cacheRamMibOf(value)`: MiB, `Infinity` for unbounded.
fn cache_ram_mib(v: Option<&Value>) -> R<f64> {
    let s = scalar(v)?;
    if matches!(s, Scalar::Undef | Scalar::Null) {
        return Ok(LLAMA_CACHE_RAM_DEFAULT_MIB);
    }
    let text = to_string(s);
    let text = trim(&text);
    if text.is_empty() {
        return Ok(LLAMA_CACHE_RAM_DEFAULT_MIB);
    }
    let n = string_to_number(text)?;
    if !n.is_finite() {
        return Ok(LLAMA_CACHE_RAM_DEFAULT_MIB);
    }
    Ok(if n < 0.0 { f64::INFINITY } else { n })
}

/// `isPromptCacheFree(model, options)`.
fn prompt_cache_free(model: Option<&Value>, options: Opts<'_>) -> R<bool> {
    for key in ["embedding", "embeddings", "reranking", "rerank"] {
        let s = scalar(options.get(key)?)?;
        let text = match s {
            Scalar::Undef | Scalar::Null => Vec::new(),
            other => to_string(other),
        };
        if let Some(t) = ascii_lower(trim(&text)) {
            if matches!(t.as_str(), "true" | "1" | "on") {
                return Ok(true);
            }
        }
    }
    let name = if truthy(model) {
        to_string(scalar(model)?)
    } else {
        Vec::new()
    };
    Ok(contains_ci(&name, "embed") || contains_ci(&name, "rerank"))
}

/// `typeBytes(t)`; NaN for the two `Object.prototype` members a lowercase key can name (the JS
/// then adds a function or object, which makes the scale NaN).
fn type_bytes(v: Option<&Value>) -> R<f64> {
    let text = if truthy(v) {
        to_string(scalar(v)?)
    } else {
        units("f16")
    };
    let Some(key) = ascii_lower(&text) else {
        return Ok(2.0);
    };
    if key == "constructor" || key == "__proto__" {
        return Ok(f64::NAN);
    }
    Ok(KV_TYPE_BYTES
        .iter()
        .find(|(k, _)| *k == key)
        .map_or(2.0, |&(_, b)| b))
}

/// The projector's pinned GiB: weights, vision scratch and the compute growth past ubatch 512.
fn pinned_gib(m: &Meta, mmproj: f64, ubatch_raw: Option<&Value>) -> R<f64> {
    let ubatch = js_max(IMAGE_MAX_TOKENS, number_or_zero(ubatch_raw)?);
    Ok(mmproj / GIB
        + MMPROJ_COMPUTE_GIB
        + 7.0 * js_max(0.0, ubatch - 512.0) * m.block_count * m.embedding_length / 1e9)
}

/// The candidate contexts at or below a nonzero native context.
fn candidates(native: f64) -> impl Iterator<Item = f64> {
    CTX_CANDIDATES
        .into_iter()
        .filter(move |&c| native == 0.0 || c <= native)
}

/// `n.toLocaleString('en-US')` for an integer up to 2^53 - 1 (grouped digits); refused beyond.
fn locale_en_us(n: f64) -> R<String> {
    if !(n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0) {
        return Err(Refusal::Ambiguous);
    }
    let digits = format!("{}", n.abs() as u64);
    let mut out = String::new();
    if n < 0.0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    Ok(out)
}

// ── JSON output ─────────────────────────────────────────────────────────────────────────────

struct Out(Vec<u8>);

impl Out {
    fn raw(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    fn key(&mut self, k: &str) -> &mut Self {
        json::push_ascii(&mut self.0, k);
        self.0.push(b':');
        self
    }
    fn str16(&mut self, s: &[u16]) -> &mut Self {
        json::push_str(&mut self.0, s);
        self
    }
    fn str(&mut self, s: &str) -> &mut Self {
        self.str16(&units(s))
    }
    fn num(&mut self, n: f64) -> &mut Self {
        if n.is_finite() {
            let s = js_number(n);
            self.raw(&s)
        } else {
            self.raw("null")
        }
    }
    fn bool(&mut self, b: bool) -> &mut Self {
        self.raw(if b { "true" } else { "false" })
    }
    /// A primitive as JSON.stringify writes it (`undefined` is never written here).
    fn scalar(&mut self, s: Scalar<'_>) -> &mut Self {
        match s {
            Scalar::Undef | Scalar::Null => self.raw("null"),
            Scalar::Bool(b) => self.bool(b),
            Scalar::Num(n) => self.num(n),
            Scalar::Str(s) => self.str16(s),
        }
    }
    fn text(self) -> String {
        String::from_utf8(self.0).unwrap_or_default()
    }
}

fn error_only(msg: &str) -> String {
    let mut o = Out(Vec::new());
    o.raw("{").key("error").str(msg).raw("}");
    o.text()
}

// ── suggest ─────────────────────────────────────────────────────────────────────────────────

struct Row {
    ctx: f64,
    model_gib: f64,
    kv_gib: f64,
    extra_gib: f64,
    cache_gib: f64,
    total_gib: f64,
    fits: bool,
}

fn write_rows(o: &mut Out, rows: &[Row]) {
    o.raw("[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            o.raw(",");
        }
        o.raw("{").key("ctx").num(r.ctx);
        o.raw(",").key("modelGib").num(round2(r.model_gib));
        o.raw(",").key("kvGib").num(round2(r.kv_gib));
        o.raw(",").key("extraGib").num(round2(r.extra_gib));
        o.raw(",").key("cacheRamGib").num(round2(r.cache_gib));
        o.raw(",").key("totalGib").num(round2(r.total_gib));
        o.raw(",").key("fits").bool(r.fits).raw("}");
    }
    o.raw("]");
}

/// What the knobs of a successful suggestion are: `[(name, value)]` in the JS's insertion order.
fn push_value(values: &mut Vec<(&'static str, String)>, k: &'static str, v: String) {
    if let Some(slot) = values.iter_mut().find(|(n, _)| *n == k) {
        slot.1 = v;
    } else {
        values.push((k, v));
    }
}

/// `suggest(args)`, as JSON.stringify writes its result.
pub fn suggest(args: &Value) -> R<String> {
    let m = Meta::from_value(args.get("meta"))?;
    let model_bytes = to_number(scalar(args.get("modelBytes"))?)?;
    let mmproj = match args.get("mmprojBytes") {
        None => 0.0,
        v => to_number(scalar(v)?)?,
    };
    let budget_raw = scalar(args.get("budgetGib"))?;
    let budget = to_number(budget_raw)?;
    let current = Opts::of(args, "current")?;
    let cache_max_raw = match args.get("cacheRamMaxMib") {
        None => Scalar::Num(1024.0),
        v => scalar(v)?,
    };
    let cache_max = to_number(cache_max_raw)?;

    if !(budget > RESERVE_GIB) {
        return Ok(error_only(
            "No inference memory budget is configured to size against.",
        ));
    }
    if !m.has_chat_template || m.is_bert() {
        return Ok(error_only("Suggestions cover chat models only. Embedding and projector files keep their qualified settings."));
    }
    if kv_cache_bytes(&m, 4096.0) <= 0.0 {
        let mut msg = units("Cannot size the KV cache from this model's metadata (");
        if m.arch.is_empty() {
            msg.extend(units("unknown architecture"));
        } else {
            msg.extend_from_slice(&m.arch);
        }
        msg.extend(units("). Set the context manually and load-test it."));
        let mut o = Out(Vec::new());
        o.raw("{").key("error").str16(&msg).raw("}");
        return Ok(o.text());
    }
    if m.expert_count != 0.0 && m.expert_count > 1.0 && model_bytes / GIB > budget - RESERVE_GIB {
        return Ok(error_only("This mixture-of-experts model needs CPU expert offload, which the preset editor does not manage. Use a smaller quantization or configure offload manually."));
    }
    let model_gib = model_bytes / GIB;
    let has_mmproj = mmproj > 0.0;
    let pinned = if has_mmproj {
        pinned_gib(&m, mmproj, current.get("ubatch-size")?)?
    } else {
        0.0
    };
    let native = m.native();
    if native != 0.0 && native < MIN_CTX {
        return Ok(error_only(&format!("This model's native context ({}) is below the smallest supported context size (4096). Set the context manually and load-test it.", number_string(native))));
    }
    let cache_gib = js_max(
        0.0,
        if cache_max == 0.0 || cache_max.is_nan() {
            0.0
        } else {
            cache_max
        },
    ) / 1024.0;
    let rows: Vec<Row> = candidates(native)
        .map(|ctx| {
            let kv_gib = (kv_cache_bytes(&m, ctx) + draft_kv_bytes(&m, ctx)) / GIB;
            let total = (model_gib + kv_gib + pinned + RESERVE_GIB) * SAFETY + cache_gib;
            Row {
                ctx,
                model_gib,
                kv_gib,
                extra_gib: pinned + RESERVE_GIB,
                cache_gib,
                total_gib: total,
                fits: total <= budget,
            }
        })
        .collect();
    let Some(best) = rows.iter().rev().find(|r| r.fits) else {
        let need = round2((model_gib + pinned + RESERVE_GIB) * SAFETY + cache_gib);
        let mut msg = units(&format!(
            "This model needs about {} GiB before any context, more than the ",
            number_string(need)
        ));
        msg.extend(to_string(budget_raw));
        msg.extend(units(" GiB budget. Use a smaller quantization."));
        let mut o = Out(Vec::new());
        o.raw("{").key("error").str16(&msg).raw(",").key("rows");
        write_rows(&mut o, &rows);
        o.raw(",").key("budgetGib").scalar(budget_raw).raw("}");
        return Ok(o.text());
    };

    let mut values: Vec<(&'static str, String)> = vec![
        ("ctx-size", number_string(best.ctx)),
        ("parallel", "1".into()),
        ("n-gpu-layers", "999".into()),
        ("flash-attn", "on".into()),
        ("cache-type-k", "q8_0".into()),
        ("cache-type-v", "q8_0".into()),
    ];
    let mut notes: Vec<String> = Vec::new();
    let convo_mib = js_round((kv_cache_bytes(&m, best.ctx) / GIB) * 4.0 * 1024.0);
    push_value(
        &mut values,
        "cache-ram",
        number_string(js_max(256.0, js_min(convo_mib, cache_max))),
    );
    if has_mmproj {
        push_value(
            &mut values,
            "image-max-tokens",
            number_string(IMAGE_MAX_TOKENS),
        );
        let ubatch = js_max(
            IMAGE_MAX_TOKENS,
            number_or_zero(current.get("ubatch-size")?)?,
        );
        push_value(&mut values, "ubatch-size", number_string(ubatch));
        let batch = number_or_zero(current.get("batch-size")?)?;
        // Number(String(u)) is u: Number#toString round-trips and "Infinity" parses back.
        if batch != 0.0 && batch < ubatch {
            push_value(&mut values, "batch-size", number_string(ubatch));
        }
    }
    if m.nextn_predict_layers > 0.0 {
        push_value(&mut values, "spec-type", "draft-mtp".into());
        notes.push("The model file includes a multi-token prediction head, so speculative decoding uses it.".into());
    } else if matches!(current.get("spec-type")?, Some(Value::Str(s)) if s.iter().copied().eq("draft-mtp".encode_utf16()))
    {
        push_value(&mut values, "spec-type", "none".into());
        notes.push("No prediction head found in this model file, so MTP is turned off.".into());
    }
    if native != 0.0 && best.ctx == native {
        notes.push(format!(
            "Context is the model's trained maximum ({} tokens).",
            locale_en_us(native)?
        ));
    } else {
        let n = if native != 0.0 {
            locale_en_us(native)?
        } else {
            "an unknown number of".into()
        };
        notes.push(format!(
            "Context is limited by memory; the model supports {n} tokens."
        ));
    }
    if !has_mmproj {
        notes.push("No vision projector is configured, so image limits are left unchanged.".into());
    }
    notes.push("Estimates cover one conversation slot. Load the model and test a long prompt before relying on the full allocation.".into());

    let mut o = Out(Vec::new());
    o.raw("{").key("values").raw("{");
    for (i, (k, v)) in values.iter().enumerate() {
        if i > 0 {
            o.raw(",");
        }
        o.key(k).str(v);
    }
    o.raw("},").key("rows");
    write_rows(&mut o, &rows);
    o.raw(",").key("budgetGib").scalar(budget_raw);
    o.raw(",").key("estimateGib").num(round2(best.total_gib));
    o.raw(",").key("notes").raw("[");
    for (i, n) in notes.iter().enumerate() {
        if i > 0 {
            o.raw(",");
        }
        o.str(n);
    }
    o.raw("],").key("source").str(SOURCE).raw("}");
    Ok(o.text())
}

// ── estimateInputs ──────────────────────────────────────────────────────────────────────────

/// `estimateInputs(args)`, as JSON.stringify writes its result.
pub fn estimate_inputs(args: &Value) -> R<String> {
    let m = Meta::from_value(args.get("meta"))?;
    let model_bytes = to_number(scalar(args.get("modelBytes"))?)?;
    let mmproj = match args.get("mmprojBytes") {
        None => 0.0,
        v => to_number(scalar(v)?)?,
    };
    let current = Opts::of(args, "current")?;
    let chat = m.has_chat_template && !m.is_bert();
    let pinned = if mmproj > 0.0 {
        pinned_gib(&m, mmproj, current.get("ubatch-size")?)?
    } else {
        0.0
    };
    let native = m.native();
    let sizeable = kv_cache_bytes(&m, 4096.0) > 0.0;
    let ctx_raw = match current.get("ctx-size")? {
        v if truthy(v) => v,
        _ => current.get("c")?,
    };
    let current_ctx = to_number(scalar(ctx_raw)?)?;
    let current_kv = match current.get("cache-type-k")? {
        Some(Value::Str(s)) => Some(s.as_slice()),
        _ => None,
    };
    let cache_mib = cache_ram_mib(current.get("cache-ram")?)?;

    let mut o = Out(Vec::new());
    o.raw("{").key("chat").bool(chat);
    o.raw(",").key("sizeable").bool(sizeable);
    o.raw(",").key("arch").str16(&m.arch);
    o.raw(",").key("nativeCtx");
    if native != 0.0 {
        o.num(native);
    } else {
        o.raw("null");
    }
    o.raw(",").key("modelGib").num(round2(model_bytes / GIB));
    o.raw(",").key("pinnedGib").num(round2(pinned));
    o.raw(",").key("reserveGib").num(RESERVE_GIB);
    o.raw(",").key("safety").num(SAFETY);
    o.raw(",")
        .key("moe")
        .bool(m.expert_count != 0.0 && m.expert_count > 1.0);
    o.raw(",").key("rows").raw("[");
    if sizeable {
        for (i, ctx) in candidates(native).enumerate() {
            if i > 0 {
                o.raw(",");
            }
            let kv = round2((kv_cache_bytes(&m, ctx) + draft_kv_bytes(&m, ctx)) / GIB);
            o.raw("{")
                .key("ctx")
                .num(ctx)
                .raw(",")
                .key("kvQ8Gib")
                .num(kv)
                .raw("}");
        }
    }
    o.raw("],").key("current").raw("{").key("ctx");
    if current_ctx != 0.0 && !current_ctx.is_nan() {
        o.num(current_ctx);
    } else {
        o.raw("null");
    }
    o.raw(",").key("kv");
    match current_kv {
        Some(s) => o.str16(s),
        None => o.raw("null"),
    };
    o.raw("},").key("cacheRamGib");
    if cache_mib.is_finite() {
        o.num(round2(cache_mib / 1024.0));
    } else {
        o.raw("null");
    }
    o.raw("}");
    Ok(o.text())
}

// ── estimateFootprint ───────────────────────────────────────────────────────────────────────

/// `estimateFootprint(args)`, as JSON.stringify writes its result.
pub fn estimate_footprint(args: &Value) -> R<String> {
    let m = Meta::from_value(args.get("meta"))?;
    let model_bytes_raw = scalar(args.get("modelBytes"))?;
    let mmproj = match args.get("mmprojBytes") {
        None => 0.0,
        v => to_number(scalar(v)?)?,
    };
    let options = Opts::of(args, "options")?;
    let model = args.get("model");
    let native = m.native();
    let ctx_raw = match options.get("ctx-size")? {
        v if truthy(v) => v,
        _ => options.get("c")?,
    };
    let ctx_num = to_number(scalar(ctx_raw)?)?;
    let ctx = if ctx_num != 0.0 && !ctx_num.is_nan() {
        ctx_num
    } else if native != 0.0 {
        native
    } else {
        4096.0
    };
    let kv_scale = (type_bytes(options.get("cache-type-k")?)?
        + type_bytes(options.get("cache-type-v")?)?)
        / 2.0
        / Q8_BYTES;
    let sizeable = kv_cache_bytes(&m, 4096.0) > 0.0;
    let kv_gib = if sizeable {
        (kv_cache_bytes(&m, ctx) + draft_kv_bytes(&m, ctx)) * kv_scale / GIB
    } else {
        0.0
    };
    let pinned = if mmproj > 0.0 {
        pinned_gib(&m, mmproj, options.get("ubatch-size")?)?
    } else {
        0.0
    };
    let mb = to_number(model_bytes_raw)?;
    let model_gib = (if mb == 0.0 || mb.is_nan() { 0.0 } else { mb }) / GIB;
    // `model = ''` when absent.
    let cache_mib = if prompt_cache_free(model, options)? {
        0.0
    } else {
        cache_ram_mib(options.get("cache-ram")?)?
    };
    let cache_gib = if cache_mib == f64::INFINITY {
        f64::INFINITY
    } else {
        cache_mib / 1024.0
    };
    let engine_gib = (model_gib + kv_gib + pinned + RESERVE_GIB) * SAFETY;
    let total_gib = engine_gib + cache_gib;

    let mut o = Out(Vec::new());
    o.raw("{").key("ctx").num(ctx);
    o.raw(",").key("modelGib").num(round2(model_gib));
    o.raw(",").key("kvGib").num(round2(kv_gib));
    o.raw(",").key("extraGib").num(round2(pinned + RESERVE_GIB));
    o.raw(",").key("cacheRamGib");
    if cache_gib == f64::INFINITY {
        o.raw("null");
    } else {
        o.num(round2(cache_gib));
    }
    o.raw(",")
        .key("cacheRamUnbounded")
        .bool(cache_gib == f64::INFINITY);
    o.raw(",").key("totalGib");
    if total_gib == f64::INFINITY {
        o.raw("null");
    } else {
        o.num(round2(total_gib));
    }
    o.raw(",").key("sizeable").bool(sizeable).raw("}");
    Ok(o.text())
}

// ── The small helpers ───────────────────────────────────────────────────────────────────────

/// `parseMemoryLimit(value)`: GiB, or `None` where the JS returns null.
fn parse_memory_limit(v: Option<&Value>) -> R<Option<f64>> {
    let text = if truthy(v) {
        to_string(scalar(v)?)
    } else {
        Vec::new()
    };
    // /^\s*(\d+(?:\.\d+)?)\s*([kmgt]?)i?b?\s*$/i
    let is_space = |c: u16| prompt_framing::js::trim(&[c]).is_empty();
    let lower = |c: u16| {
        if (0x41..=0x5a).contains(&c) {
            c + 0x20
        } else {
            c
        }
    };
    let digit = |c: u16| (0x30..=0x39).contains(&c);
    let at = |i: usize| text.get(i).copied();
    let mut i = 0;
    while at(i).is_some_and(is_space) {
        i += 1;
    }
    let start = i;
    while at(i).is_some_and(digit) {
        i += 1;
    }
    if i == start {
        return Ok(None);
    }
    if at(i) == Some(0x2e) && at(i + 1).is_some_and(digit) {
        i += 1;
        while at(i).is_some_and(digit) {
            i += 1;
        }
    }
    let number: String = text
        .get(start..i)
        .unwrap_or(&[])
        .iter()
        .map(|&c| char::from(c as u8))
        .collect();
    while at(i).is_some_and(is_space) {
        i += 1;
    }
    let mut scale = 1.0 / GIB;
    if let Some(c) = at(i).map(lower) {
        let s = match c {
            0x6b => Some(1.0 / (1024.0 * 1024.0)),
            0x6d => Some(1.0 / 1024.0),
            0x67 => Some(1.0),
            0x74 => Some(1024.0),
            _ => None,
        };
        if let Some(s) = s {
            scale = s;
            i += 1;
        }
    }
    if at(i).map(lower) == Some(0x69) {
        i += 1;
    }
    if at(i).map(lower) == Some(0x62) {
        i += 1;
    }
    while at(i).is_some_and(is_space) {
        i += 1;
    }
    if i != text.len() {
        return Ok(None);
    }
    Ok(Some(number.parse::<f64>().unwrap_or(f64::NAN) * scale))
}

// ── The wasm call ───────────────────────────────────────────────────────────────────────────

/// One request: `u8(op)` and UTF-8 JSON. Ops: 1 `suggest(args)`, 2 `estimateInputs(args)`,
/// 3 `estimateFootprint(args)` (args the JS's parameter object; an absent member is
/// `undefined`), 4 `{"value":v}` → `{"mib":n}` / `{"unbounded":true}` (cacheRamMibOf),
/// 5 `{"model":m,"options":o}` → `{"free":bool}` (isPromptCacheFree), 6 `{"value":v}` →
/// `{"gib":n|null|"Infinity"}` (parseMemoryLimit). Status 0 with the reply, or 1 with
/// `{"error":"input"|"too_large"|"ambiguous"}`.
pub fn call(input: &[u8]) -> (u32, String) {
    match call_inner(input) {
        Ok(r) => (0, r),
        Err(e) => (1, e.json().to_owned()),
    }
}

fn call_inner(input: &[u8]) -> R<String> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Refusal::TooLarge);
    }
    let Some((&op, body)) = input.split_first() else {
        return Err(Refusal::Input);
    };
    let args = json::parse_utf8(body, WIRE_DEPTH).ok_or(Refusal::Input)?;
    if !matches!(args, Value::Obj(_)) {
        return Err(Refusal::Input);
    }
    match op {
        1 => suggest(&args),
        2 => estimate_inputs(&args),
        3 => estimate_footprint(&args),
        4 => {
            let mib = cache_ram_mib(args.get("value"))?;
            let mut o = Out(Vec::new());
            if mib == f64::INFINITY {
                o.raw(r#"{"unbounded":true}"#);
            } else {
                o.raw("{").key("mib").num(mib).raw("}");
            }
            Ok(o.text())
        }
        5 => {
            let free = prompt_cache_free(args.get("model"), Opts::of(&args, "options")?)?;
            Ok(format!(r#"{{"free":{free}}}"#))
        }
        6 => {
            let mut o = Out(Vec::new());
            o.raw("{").key("gib");
            match parse_memory_limit(args.get("value"))? {
                None => o.raw("null"),
                Some(g) if g == f64::INFINITY => o.raw(r#""Infinity""#),
                Some(g) => o.num(g),
            };
            o.raw("}");
            Ok(o.text())
        }
        _ => Err(Refusal::Input),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(op: u8, wire: &str) -> (u32, String) {
        let mut input = vec![op];
        input.extend(wire.as_bytes());
        call(&input)
    }

    #[test]
    fn js_math() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert!(js_round(-0.4).is_sign_negative());
        assert_eq!(js_round(0.499_999_999_999_999_94), 0.0);
        assert!(js_min(0.0, -0.0).is_sign_negative());
        assert!(js_max(-0.0, 0.0).is_sign_positive());
        assert!(js_min(1.0, f64::NAN).is_nan());
        assert_eq!(number_string(f64::NAN), "NaN");
        assert_eq!(locale_en_us(131072.0).unwrap(), "131,072");
        assert_eq!(locale_en_us(1048576.0).unwrap(), "1,048,576");
        assert_eq!(locale_en_us(4096.0).unwrap(), "4,096");
        assert_eq!(locale_en_us(5000.5), Err(Refusal::Ambiguous));
    }

    #[test]
    fn string_to_number_cases() {
        let n = |s: &str| string_to_number(&units(s)).unwrap();
        assert_eq!(n(" 1e3 "), 1000.0);
        assert_eq!(n("0x1F"), 31.0);
        assert_eq!(n("0B11"), 3.0);
        assert!(n("-0x1").is_nan());
        assert_eq!(n("5.e3"), 5000.0);
        assert_eq!(n(".5"), 0.5);
        assert!(n(".").is_nan());
        assert!(n("1_0").is_nan());
        assert_eq!(n("-Infinity"), f64::NEG_INFINITY);
        assert!(n("inf").is_nan());
        assert_eq!(n("\u{feff}\u{3000}7\u{a0}"), 7.0);
        assert_eq!(n(""), 0.0);
        assert_eq!(
            string_to_number(&units(&format!("0x{}", "f".repeat(40)))),
            Err(Refusal::Ambiguous)
        );
    }

    #[test]
    fn period() {
        let k = |xs: &[u8]| {
            xs.iter()
                .map(|&x| Key::Num(u64::from(x)))
                .collect::<Vec<_>>()
        };
        assert_eq!(period_of(&k(&[1, 1, 1, 1, 1, 0])), 6);
        assert_eq!(period_of(&k(&[1, 0, 1, 0, 1])), 2);
        assert_eq!(period_of(&k(&[1, 1, 1])), 1);
        assert_eq!(period_of(&k(&[1, 2, 1, 1, 2])), 3);
    }

    #[test]
    fn helpers_and_refusals() {
        assert_eq!(run(4, r#"{}"#), (0, r#"{"mib":8192}"#.into()));
        assert_eq!(
            run(4, r#"{"value":"-1"}"#),
            (0, r#"{"unbounded":true}"#.into())
        );
        assert_eq!(
            run(4, r#"{"value":" 0x400 "}"#),
            (0, r#"{"mib":1024}"#.into())
        );
        assert_eq!(
            run(4, r#"{"value":"Infinity"}"#),
            (0, r#"{"mib":8192}"#.into())
        );
        assert_eq!(
            run(5, r#"{"model":"Nomic-EMBED","options":{}}"#),
            (0, r#"{"free":true}"#.into())
        );
        assert_eq!(
            run(5, r#"{"model":"chat","options":{"rerank":" ON "}}"#),
            (0, r#"{"free":true}"#.into())
        );
        assert_eq!(
            run(5, r#"{"model":"chat","options":null}"#),
            (1, Refusal::Input.json().into())
        );
        assert_eq!(run(6, r#"{"value":"14GiB"}"#), (0, r#"{"gib":14}"#.into()));
        assert_eq!(
            run(6, r#"{"value":" 512 m "}"#),
            (0, r#"{"gib":0.5}"#.into())
        );
        assert_eq!(
            run(6, r#"{"value":"1.5.5g"}"#),
            (0, r#"{"gib":null}"#.into())
        );
        assert_eq!(
            run(6, r#"{"value":[1]}"#),
            (1, Refusal::Ambiguous.json().into())
        );
        assert_eq!(run(9, r#"{}"#), (1, Refusal::Input.json().into()));
        assert_eq!(run(1, r#"[]"#), (1, Refusal::Input.json().into()));
        assert_eq!(run(1, r#"{"#), (1, Refusal::Input.json().into()));
        assert_eq!(call(&[]), (1, Refusal::Input.json().into()));
        let big = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(call(&big), (1, Refusal::TooLarge.json().into()));
    }

    #[test]
    fn plain_suggestion() {
        let meta = r#"{"arch":"llama","hasChatTemplate":true,"contextLength":131072,"embeddingLength":4096,"blockCount":32,"headCount":32,"headCountKv":8}"#;
        let (s, r) = run(
            1,
            &format!(r#"{{"meta":{meta},"modelBytes":4294967296,"budgetGib":14}}"#),
        );
        assert_eq!(s, 0, "{r}");
        assert!(r.starts_with(r#"{"values":{"ctx-size":""#), "{r}");
        assert!(
            r.contains("131,072") || r.contains("Context is limited"),
            "{r}"
        );
        // A null current is read only past the early answers.
        let (s, _) = run(
            1,
            &format!(r#"{{"meta":{meta},"modelBytes":4294967296,"budgetGib":14,"current":null}}"#),
        );
        assert_eq!(s, 1);
        let (s, r) = run(
            1,
            &format!(r#"{{"meta":{meta},"budgetGib":0.5,"current":null}}"#),
        );
        assert_eq!(
            (s, r.as_str()),
            (
                0,
                r#"{"error":"No inference memory budget is configured to size against."}"#
            )
        );
    }
}
