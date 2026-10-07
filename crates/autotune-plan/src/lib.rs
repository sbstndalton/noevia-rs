//! The step planner behind noevia's model auto-tune (sbstndalton/noevia#1003).
//!
//! Auto-tune stays a script: nothing here guesses, learns or calls a model. Given a model's facts
//! (GGUF metadata, file sizes), the inference memory budget and every result measured so far,
//! [`plan`] returns the one next step, always the same step for the same input:
//!
//! 1. **The KV cache type, precision first** (noevia#1057). The caller lists the allowed types
//!    most precise first (the list is the floor: noevia-core offers `bf16, q8_0` by default). The
//!    run takes the most precise type that fits the smallest rung; walking the list, a more
//!    compact type replaces that choice only when its largest fitting rung (by the estimate, at
//!    most the trained context) is at least twice the current choice's. So a more compact cache
//!    is chosen only when it about doubles the context, never to win a few rungs.
//! 2. **Then the context for that type:** its largest fitting rung first.
//! 3. **Fill and recall.** Each `probe` fills about 90% of that context with synthetic text and a
//!    needle, then checks recall, memory and quality. Any failure steps the context down
//!    (bisection over the type's rungs, at most [`MAX_PROBES`] probes in all). Only when the type
//!    has failed at every context it fits does the run step to the next more compact type (the
//!    doubling rule then applies from there). A quality failure rules that type and every more
//!    compact one out for the whole run, so the search goes back to a more precise type.
//! 4. **The remaining phases** (`sampling`, `drafting`, `batch`), each once.
//! 5. **One final fill check** (`verify`) with the finished settings, stepping down a rung at a
//!    time on failure (at most [`MAX_VERIFY`] attempts), then the **serving check**.
//!
//! No step that already has a result is planned again, so a resumed run continues where it
//! stopped. Memory is integer arithmetic over bytes (no floating point), mirroring noevia-core's
//! `llamacpp-autoconfig.cjs` `kvCacheBytes` estimate with the same 1 GiB runtime reserve and 5%
//! margin, so native and WebAssembly builds give byte-identical replies.
//!
//! Input is untrusted JSON (see [`parse`]); anything malformed or over a cap is refused with a
//! fixed error code and never echoed. Nothing here panics.

use serde_json::{json, Map, Value};

/// Requests longer than this are refused ([`PlanError::TooLarge`]).
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// At most this many context rungs.
pub const MAX_LADDER: usize = 128;
/// At most this many results.
pub const MAX_RESULTS: usize = 256;
/// Per-layer metadata arrays (`headCountKv`, `slidingWindowPattern`) at most this long.
pub const MAX_LAYER_ARRAY: usize = 4096;
/// Fill-and-recall probes in the context stage, at most.
pub const MAX_PROBES: usize = 8;
/// Final fill checks, at most.
pub const MAX_VERIFY: usize = 3;
/// The share of the context a probe fills, in tenths, less [`FILL_HEADROOM`] tokens for the reply.
pub const FILL_TENTHS: u64 = 9;
/// Tokens left free for the instructions and the answer.
pub const FILL_HEADROOM: u64 = 256;
/// Smallest and largest context rung accepted.
pub const MIN_RUNG: u64 = 256;
pub const MAX_RUNG: u64 = 1 << 24;

const MIB: u128 = 1 << 20;
const GIB: u128 = 1 << 30;
/// Runtime, driver context and compute scratch (`RESERVE_GIB` in llamacpp-autoconfig.cjs).
const RUNTIME_RESERVE: u128 = GIB;
/// Recurrent state per SSM layer of a hybrid model (`SSM_STATE_BYTES`).
const SSM_STATE: u128 = 4 * MIB;
/// Vision encoder scratch beyond the projector weights (`MMPROJ_COMPUTE_GIB`).
const MMPROJ_COMPUTE: u128 = GIB / 2;
/// `IMAGE_MAX_TOKENS`: the micro-batch a projector forces at least.
const IMAGE_MAX_TOKENS: u128 = 1024;
const MAX_DIM: u64 = 1 << 20;
const MAX_BYTES: u64 = 1 << 50;
const MAX_MIB: u64 = 1 << 30;

/// A KV cache element type, in llama.cpp's block layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvType {
    F32,
    F16,
    Bf16,
    Q8_0,
    Q5_1,
    Q5_0,
    Q4_1,
    Q4_0,
    Iq4Nl,
}

impl KvType {
    pub const ALL: [KvType; 9] = [
        KvType::F32,
        KvType::F16,
        KvType::Bf16,
        KvType::Q8_0,
        KvType::Q5_1,
        KvType::Q5_0,
        KvType::Q4_1,
        KvType::Q4_0,
        KvType::Iq4Nl,
    ];
    pub fn name(self) -> &'static str {
        match self {
            KvType::F32 => "f32",
            KvType::F16 => "f16",
            KvType::Bf16 => "bf16",
            KvType::Q8_0 => "q8_0",
            KvType::Q5_1 => "q5_1",
            KvType::Q5_0 => "q5_0",
            KvType::Q4_1 => "q4_1",
            KvType::Q4_0 => "q4_0",
            KvType::Iq4Nl => "iq4_nl",
        }
    }
    pub fn from_name(s: &str) -> Option<KvType> {
        KvType::ALL.into_iter().find(|k| k.name() == s)
    }
    /// Bytes per element times 32 (`KV_TYPE_BYTES` in llamacpp-autoconfig.cjs, exactly).
    fn bytes32(self) -> u128 {
        match self {
            KvType::F32 => 128,
            KvType::F16 | KvType::Bf16 => 64,
            KvType::Q8_0 => 34,
            KvType::Q5_1 => 24,
            KvType::Q5_0 => 22,
            KvType::Q4_1 => 20,
            KvType::Q4_0 | KvType::Iq4Nl => 18,
        }
    }
}

/// The model facts the estimate needs (gguf-meta.cjs `summarize` names). Zero means unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub n_ctx_train: u64,
    pub block_count: u64,
    pub head_count: u64,
    /// One value, or one per layer.
    pub head_count_kv: Vec<u64>,
    pub embedding_length: u64,
    pub key_length: u64,
    pub value_length: u64,
    pub key_length_swa: u64,
    pub value_length_swa: u64,
    pub sliding_window: u64,
    pub sliding_window_pattern: Vec<u64>,
    pub shared_kv_layers: u64,
    pub full_attention_interval: u64,
    pub nextn_predict_layers: u64,
    pub model_bytes: u64,
    pub mmproj_bytes: u64,
    pub ubatch: u64,
    /// Parallel slots (at least 1); a probe fills one slot's share.
    pub slots: u64,
}

/// The memory a profile may use, in MiB.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// The administrator's inference memory budget.
    pub budget_mib: u64,
    /// `MemAvailable` with the tuned model unloaded, read once per model; `None` when unreadable.
    pub mem_available_mib: Option<u64>,
    /// Kept free for the services that run alongside (Laya).
    pub reserve_mib: u64,
    /// The safety floor the run aborts at.
    pub floor_mib: u64,
    /// The profile's prompt cache (`cache-ram`), host memory spent on inference too.
    pub cache_ram_mib: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Passed,
    /// Available memory fell below the floor, or the engine died.
    Oom,
    /// The engine refused or failed to load the profile.
    LoadFailed,
    /// The needle was not recalled, or fewer tokens were accepted than sent.
    RecallFailed,
    /// Filling the context took longer than the admin's prompt time limit.
    OverTime,
    /// The quality probes failed with this cache type.
    QualityFailed,
}

impl ProbeOutcome {
    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "passed" => Self::Passed,
            "oom" => Self::Oom,
            "load_failed" => Self::LoadFailed,
            "recall_failed" => Self::RecallFailed,
            "over_time" => Self::OverTime,
            "quality_failed" => Self::QualityFailed,
            _ => return None,
        })
    }
    fn memory(self) -> bool {
        matches!(self, Self::Oom | Self::LoadFailed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Sampling,
    Drafting,
    Batch,
}

impl Phase {
    pub const ORDER: [Phase; 3] = [Phase::Sampling, Phase::Drafting, Phase::Batch];
    pub fn name(self) -> &'static str {
        match self {
            Phase::Sampling => "sampling",
            Phase::Drafting => "drafting",
            Phase::Batch => "batch",
        }
    }
    fn from_name(s: &str) -> Option<Self> {
        Phase::ORDER.into_iter().find(|p| p.name() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Skipped,
    Failed,
}

impl Outcome {
    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "passed" => Self::Passed,
            "skipped" => Self::Skipped,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// One measured result, as the caller recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Probe {
        ctx: u64,
        kv: KvType,
        outcome: ProbeOutcome,
    },
    Verify {
        ctx: u64,
        kv: KvType,
        outcome: ProbeOutcome,
    },
    Phase {
        id: Phase,
        outcome: Outcome,
    },
    Serving {
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub facts: Facts,
    pub memory: Memory,
    /// Context rungs, strictly increasing.
    pub ladder: Vec<u64>,
    /// Allowed cache types, most precise first.
    pub kv: Vec<KvType>,
    pub results: Vec<Entry>,
}

/// Why a request was refused before planning. Codes are fixed and never carry input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    TooLarge,
    Input,
}

impl PlanError {
    pub fn code(self) -> &'static str {
        match self {
            PlanError::TooLarge => "too_large",
            PlanError::Input => "input",
        }
    }
}

/// Why the plan stops without a profile. Each has one fixed public message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The KV cache cannot be sized from the metadata; the caller may fall back to its old order.
    Unsizeable,
    /// No ladder rung is at or below the trained context.
    NoRung,
    /// Not even the smallest rung fits the memory budget.
    DoesNotFit,
    /// No context passed the fill-and-recall probe.
    NoContext,
    /// A required phase failed.
    PhaseFailed,
    /// The finished profile failed every final fill check.
    VerifyFailed,
    /// The finished profile cannot serve a realistic chat request.
    ServingFailed,
}

impl Stop {
    pub fn code(self) -> &'static str {
        match self {
            Stop::Unsizeable => "unsizeable",
            Stop::NoRung => "no_rung",
            Stop::DoesNotFit => "does_not_fit",
            Stop::NoContext => "no_context",
            Stop::PhaseFailed => "phase_failed",
            Stop::VerifyFailed => "verify_failed",
            Stop::ServingFailed => "serving_failed",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Stop::Unsizeable => "Auto-tune cannot size this model's KV cache from its metadata.",
            Stop::NoRung => "No supported context size is at or below this model's trained context.",
            Stop::DoesNotFit => "This model does not fit the inference memory budget even at the smallest context size.",
            Stop::NoContext => "No context size passed the fill-and-recall test.",
            Stop::PhaseFailed => "A required tuning phase failed.",
            Stop::VerifyFailed => "The finished profile could not fill and recall its context.",
            Stop::ServingFailed => "The finished profile cannot serve a realistic chat request.",
        }
    }
}

/// The next step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Write `ctx` and `kv`, load, fill `fill` tokens with a needle, check recall, memory and quality.
    Probe {
        ctx: u64,
        kv: KvType,
        fill: u64,
        estimate_mib: u64,
    },
    /// Run this phase at the chosen context and cache type.
    Phase {
        id: Phase,
        ctx: u64,
        kv: KvType,
    },
    /// Write `ctx` with the finished settings and fill it again.
    Verify {
        ctx: u64,
        kv: KvType,
        fill: u64,
        estimate_mib: u64,
    },
    /// The realistic chat request.
    Serving {
        ctx: u64,
        kv: KvType,
    },
    /// Finished: this profile is the tune.
    Done {
        ctx: u64,
        kv: KvType,
    },
    Fail(Stop),
}

// ---------------------------------------------------------------------------------------------
// Parsing

fn uint(o: &Map<String, Value>, key: &str, max: u64) -> Result<u64, PlanError> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => v.as_u64().filter(|&n| n <= max).ok_or(PlanError::Input),
    }
}

fn uint_or_array(o: &Map<String, Value>, key: &str) -> Result<Vec<u64>, PlanError> {
    match o.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => {
            if a.len() > MAX_LAYER_ARRAY {
                return Err(PlanError::Input);
            }
            a.iter()
                .map(|v| match v {
                    Value::Bool(b) => Ok(u64::from(*b)),
                    v => v.as_u64().filter(|&n| n <= MAX_DIM).ok_or(PlanError::Input),
                })
                .collect()
        }
        Some(v) => v
            .as_u64()
            .filter(|&n| n <= MAX_DIM)
            .map(|n| vec![n])
            .ok_or(PlanError::Input),
    }
}

fn object<'a>(v: &'a Value, key: &str) -> Result<&'a Map<String, Value>, PlanError> {
    v.get(key)
        .and_then(Value::as_object)
        .ok_or(PlanError::Input)
}

fn str_field<'a>(o: &'a Map<String, Value>, key: &str) -> Result<&'a str, PlanError> {
    o.get(key).and_then(Value::as_str).ok_or(PlanError::Input)
}

fn kv_field(o: &Map<String, Value>, kv: &[KvType]) -> Result<KvType, PlanError> {
    let k = KvType::from_name(str_field(o, "kv")?).ok_or(PlanError::Input)?;
    if kv.contains(&k) {
        Ok(k)
    } else {
        Err(PlanError::Input)
    }
}

fn parse_entry(v: &Value, kv: &[KvType]) -> Result<Entry, PlanError> {
    let o = v.as_object().ok_or(PlanError::Input)?;
    let outcome = str_field(o, "outcome")?;
    Ok(match str_field(o, "step")? {
        s @ ("probe" | "verify") => {
            let ctx = uint(o, "ctx", MAX_RUNG)?;
            let kv = kv_field(o, kv)?;
            let outcome = ProbeOutcome::from_name(outcome).ok_or(PlanError::Input)?;
            if s == "probe" {
                Entry::Probe { ctx, kv, outcome }
            } else {
                Entry::Verify { ctx, kv, outcome }
            }
        }
        "phase" => Entry::Phase {
            id: Phase::from_name(str_field(o, "id")?).ok_or(PlanError::Input)?,
            outcome: Outcome::from_name(outcome).ok_or(PlanError::Input)?,
        },
        "serving" => Entry::Serving {
            outcome: Outcome::from_name(outcome).ok_or(PlanError::Input)?,
        },
        _ => return Err(PlanError::Input),
    })
}

/// Parse a request:
///
/// ```json
/// {"facts":{"nCtxTrain":131072,"blockCount":32,"headCount":32,"headCountKv":8,
///   "embeddingLength":4096,"modelBytes":4920000000,"slots":1, …},
///  "memory":{"budgetMib":16384,"memAvailableMib":30000,"reserveMib":2560,"floorMib":2048,
///   "cacheRamMib":1024},
///  "ladder":[4096,8192,…],"kv":["bf16","q8_0"],
///  "results":[{"step":"probe","ctx":65536,"kv":"bf16","outcome":"oom"}, …]}
/// ```
pub fn parse(text: &str) -> Result<Input, PlanError> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(PlanError::TooLarge);
    }
    let v: Value = serde_json::from_str(text).map_err(|_| PlanError::Input)?;
    let f = object(&v, "facts")?;
    let facts = Facts {
        n_ctx_train: uint(f, "nCtxTrain", MAX_RUNG)?,
        block_count: uint(f, "blockCount", MAX_LAYER_ARRAY as u64)?,
        head_count: uint(f, "headCount", MAX_DIM)?,
        head_count_kv: uint_or_array(f, "headCountKv")?,
        embedding_length: uint(f, "embeddingLength", MAX_DIM)?,
        key_length: uint(f, "keyLength", MAX_DIM)?,
        value_length: uint(f, "valueLength", MAX_DIM)?,
        key_length_swa: uint(f, "keyLengthSwa", MAX_DIM)?,
        value_length_swa: uint(f, "valueLengthSwa", MAX_DIM)?,
        sliding_window: uint(f, "slidingWindow", MAX_RUNG)?,
        sliding_window_pattern: uint_or_array(f, "slidingWindowPattern")?,
        shared_kv_layers: uint(f, "sharedKvLayers", MAX_LAYER_ARRAY as u64)?,
        full_attention_interval: uint(f, "fullAttentionInterval", MAX_LAYER_ARRAY as u64)?,
        nextn_predict_layers: uint(f, "nextnPredictLayers", MAX_LAYER_ARRAY as u64)?,
        model_bytes: uint(f, "modelBytes", MAX_BYTES)?,
        mmproj_bytes: uint(f, "mmprojBytes", MAX_BYTES)?,
        ubatch: uint(f, "ubatch", MAX_RUNG)?,
        slots: uint(f, "slots", 1024)?.max(1),
    };
    let m = object(&v, "memory")?;
    let memory = Memory {
        budget_mib: uint(m, "budgetMib", MAX_MIB)?,
        mem_available_mib: match m.get("memAvailableMib") {
            None | Some(Value::Null) => None,
            Some(_) => Some(uint(m, "memAvailableMib", MAX_MIB)?),
        },
        reserve_mib: uint(m, "reserveMib", MAX_MIB)?,
        floor_mib: uint(m, "floorMib", MAX_MIB)?,
        cache_ram_mib: uint(m, "cacheRamMib", MAX_MIB)?,
    };
    let ladder = v
        .get("ladder")
        .and_then(Value::as_array)
        .ok_or(PlanError::Input)?;
    // Empty is allowed: no rung at or below the trained context (planned as `no_rung`).
    if ladder.len() > MAX_LADDER {
        return Err(PlanError::Input);
    }
    let ladder: Vec<u64> = ladder
        .iter()
        .map(|r| {
            r.as_u64()
                .filter(|n| (MIN_RUNG..=MAX_RUNG).contains(n))
                .ok_or(PlanError::Input)
        })
        .collect::<Result<_, _>>()?;
    if ladder.windows(2).any(|w| matches!(w, [a, b] if a >= b)) {
        return Err(PlanError::Input);
    }
    let kv_list = v
        .get("kv")
        .and_then(Value::as_array)
        .ok_or(PlanError::Input)?;
    if kv_list.is_empty() || kv_list.len() > KvType::ALL.len() {
        return Err(PlanError::Input);
    }
    let mut kv = Vec::with_capacity(kv_list.len());
    for k in kv_list {
        let k = k
            .as_str()
            .and_then(KvType::from_name)
            .ok_or(PlanError::Input)?;
        if kv.contains(&k) {
            return Err(PlanError::Input);
        }
        kv.push(k);
    }
    let results = match v.get("results") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) if a.len() <= MAX_RESULTS => a
            .iter()
            .map(|e| parse_entry(e, &kv))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(PlanError::Input),
    };
    Ok(Input {
        facts,
        memory,
        ladder,
        kv,
        results,
    })
}

// ---------------------------------------------------------------------------------------------
// The memory estimate

/// The most frequent value; ties go to the one seen first (as llamacpp-autoconfig.cjs kvHeadsOf).
fn mode(values: &[u64]) -> Option<u64> {
    let mut best: Option<(u64, usize)> = None;
    let mut seen: Vec<(u64, usize)> = Vec::new();
    for &v in values {
        match seen.iter_mut().find(|(x, _)| *x == v) {
            Some((_, n)) => *n += 1,
            None => seen.push((v, 1)),
        }
    }
    for (v, n) in seen {
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((v, n));
        }
    }
    best.map(|(v, _)| v)
}

fn period(seq: &[u64]) -> usize {
    (1..=seq.len())
        .find(|&p| {
            seq.iter()
                .enumerate()
                .all(|(i, v)| seq.get(i % p) == Some(v))
        })
        .unwrap_or(seq.len())
}

/// KV elements (all layers, K and V) for `ctx` tokens including a built-in draft head, and the
/// fixed recurrent-state bytes of a hybrid model. `None`: the metadata cannot size the cache.
fn kv_elements(f: &Facts, ctx: u64) -> Option<(u128, u128)> {
    let ctx = u128::from(ctx);
    let layers = u128::from(f.block_count);
    let heads = u128::from(f.head_count);
    let head_dim = u128::from(f.embedding_length)
        .checked_div(heads)
        .unwrap_or(0);
    let kv_heads = match f.head_count_kv.as_slice() {
        [] => heads,
        [one] if *one > 0 => u128::from(*one),
        [_] => heads,
        many => u128::from(mode(many)?),
    };
    let k_dim = if f.key_length > 0 {
        u128::from(f.key_length)
    } else {
        head_dim
    };
    let v_dim = if f.value_length > 0 {
        u128::from(f.value_length)
    } else {
        head_dim
    };
    if ctx == 0 || layers == 0 || kv_heads == 0 || k_dim == 0 || v_dim == 0 {
        return None;
    }
    let per_layer_token = kv_heads * (k_dim + v_dim);
    let draft = u128::from(f.nextn_predict_layers) * per_layer_token * ctx;
    let interval = u128::from(f.full_attention_interval);
    if interval > 1 {
        let full = layers.div_ceil(interval).max(1);
        return Some((
            full * per_layer_token * ctx + draft,
            layers.saturating_sub(full) * SSM_STATE,
        ));
    }
    let shared = u128::from(f.shared_kv_layers).min(layers);
    if f.sliding_window > 0 {
        let window = u128::from(f.sliding_window).min(ctx);
        let k_swa = if f.key_length_swa > 0 {
            u128::from(f.key_length_swa)
        } else {
            k_dim
        };
        let v_swa = if f.value_length_swa > 0 {
            u128::from(f.value_length_swa)
        } else {
            v_dim
        };
        let local_elem = k_swa + v_swa;
        let global_elem = k_dim + v_dim;
        let allocated = layers - shared;
        let pattern = &f.sliding_window_pattern;
        if pattern.len() as u128 == layers {
            let p = period(pattern);
            let head_seq =
                (f.head_count_kv.len() as u128 == layers).then_some(f.head_count_kv.as_slice());
            let mut sum: u128 = 0;
            for (i, &local) in pattern.iter().take(p).enumerate() {
                let h = match head_seq {
                    Some(s) => u128::from(s.get(i).copied().unwrap_or(1).max(1)),
                    None => kv_heads,
                };
                sum += if local != 0 {
                    h * local_elem * window
                } else {
                    h * global_elem * ctx
                };
            }
            return Some((sum * allocated / p as u128 + draft, 0));
        }
        let global = (layers / 6).max(1);
        let local = layers - global.min(layers);
        let inner = global * kv_heads * global_elem * ctx + local * kv_heads * local_elem * window;
        return Some((inner * allocated / layers + draft, 0));
    }
    let own = layers.saturating_sub(shared.min(layers - 1)).max(1);
    Some((own * per_layer_token * ctx + draft, 0))
}

/// Estimated bytes of inference memory for `ctx` with `kv`, or `None` when unsizeable.
pub fn estimate_bytes(f: &Facts, m: &Memory, ctx: u64, kv: KvType) -> Option<u128> {
    let (elements, fixed) = kv_elements(f, ctx)?;
    let kv_bytes = (elements * kv.bytes32()).div_ceil(32) + fixed;
    let pinned = if f.mmproj_bytes > 0 {
        let ubatch = u128::from(f.ubatch).max(IMAGE_MAX_TOKENS);
        let growth =
            (7 * (ubatch - 512) * u128::from(f.block_count) * u128::from(f.embedding_length) * GIB)
                .div_ceil(1_000_000_000);
        u128::from(f.mmproj_bytes) + MMPROJ_COMPUTE + growth
    } else {
        0
    };
    let engine = u128::from(f.model_bytes) + kv_bytes + pinned + RUNTIME_RESERVE;
    Some((engine * 21).div_ceil(20) + u128::from(m.cache_ram_mib) * MIB)
}

/// The bytes a profile may use: the budget, and no more than what is available less the
/// reserve and the floor.
pub fn usable_bytes(m: &Memory) -> u128 {
    let budget = u128::from(m.budget_mib) * MIB;
    match m.mem_available_mib {
        Some(avail) => budget.min(
            u128::from(
                avail
                    .saturating_sub(m.reserve_mib)
                    .saturating_sub(m.floor_mib),
            ) * MIB,
        ),
        None => budget,
    }
}

fn fill_for(f: &Facts, ctx: u64) -> u64 {
    (ctx / f.slots.max(1) * FILL_TENTHS / 10).saturating_sub(FILL_HEADROOM)
}

fn mib_ceil(bytes: u128) -> u64 {
    u64::try_from(bytes.div_ceil(MIB)).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------------------------
// The planner

struct Ctx<'a> {
    input: &'a Input,
    usable: u128,
    /// Rungs at or below the trained context.
    rungs: Vec<u64>,
}

impl Ctx<'_> {
    fn fits(&self, ctx: u64, kv: KvType) -> bool {
        estimate_bytes(&self.input.facts, &self.input.memory, ctx, kv)
            .is_some_and(|b| b <= self.usable)
    }
    fn estimate_mib(&self, ctx: u64, kv: KvType) -> u64 {
        estimate_bytes(&self.input.facts, &self.input.memory, ctx, kv).map_or(0, mib_ceil)
    }
    fn kv_index(&self, kv: KvType) -> usize {
        self.input
            .kv
            .iter()
            .position(|&k| k == kv)
            .unwrap_or(usize::MAX)
    }
}

struct Search {
    probes: Vec<(u64, usize, ProbeOutcome)>,
    /// Types from this index on are ruled out by a quality failure.
    banned_from: usize,
}

/// Where one cache type's context search stands.
struct TypeSearch {
    /// The largest context that passed with this type.
    lo: Option<u64>,
    /// Rungs above `lo` that fit this type by the estimate.
    above: Vec<u64>,
    /// The prefix of `above` no measured failure rules out.
    open: Vec<u64>,
}

impl TypeSearch {
    /// Nothing passed and nothing is left to try: the run moves to the next type.
    fn exhausted(&self) -> bool {
        self.lo.is_none() && self.open.is_empty()
    }
}

impl Search {
    fn dominated(&self, ctx: u64, kv: usize) -> bool {
        self.probes
            .iter()
            .any(|&(c, k, o)| o.memory() && ctx >= c && kv <= k)
    }
    fn probed(&self, ctx: u64, kv: usize) -> bool {
        self.probes.iter().any(|&(c, k, _)| c == ctx && k == kv)
    }
    fn hard_failed(&self, ctx: u64) -> bool {
        self.probes.iter().any(|&(c, _, o)| {
            matches!(o, ProbeOutcome::RecallFailed | ProbeOutcome::OverTime) && ctx >= c
        })
    }
    fn allowed(&self, cx: &Ctx) -> usize {
        self.banned_from.min(cx.input.kv.len())
    }
    /// Whether `ctx` is still worth a probe with type `k`.
    fn open_at(&self, ctx: u64, k: usize) -> bool {
        !self.hard_failed(ctx) && !self.dominated(ctx, k) && !self.probed(ctx, k)
    }
    fn lo(&self, k: usize) -> Option<u64> {
        self.probes
            .iter()
            .filter(|&&(_, pk, o)| pk == k && o == ProbeOutcome::Passed)
            .map(|&(c, _, _)| c)
            .max()
    }
    fn of_type(&self, cx: &Ctx, k: usize) -> TypeSearch {
        let lo = self.lo(k);
        let above: Vec<u64> = match cx.input.kv.get(k) {
            Some(&kv) => cx
                .rungs
                .iter()
                .copied()
                .filter(|&c| c > lo.unwrap_or(0) && cx.fits(c, kv))
                .collect(),
            None => Vec::new(),
        };
        let open = above
            .iter()
            .copied()
            .take_while(|&c| self.open_at(c, k))
            .collect();
        TypeSearch { lo, above, open }
    }
}

enum Searched {
    Next(u64, usize),
    Chosen(u64, usize),
    Stop(Stop),
}

/// The largest rung type `k` fits by the estimate, or `None` when not even the smallest does.
fn ceiling(cx: &Ctx, k: usize) -> Option<u64> {
    let kv = *cx.input.kv.get(k)?;
    cx.rungs.iter().copied().rfind(|&c| cx.fits(c, kv))
}

/// The type to search: precision first. The most precise allowed type still in play; each more
/// compact one after it replaces the current choice only when it fits at least twice the
/// context the current choice fits (by the estimate, on the ladder).
fn preferred(cx: &Ctx, s: &Search) -> Option<(usize, TypeSearch)> {
    let mut alive = (0..s.allowed(cx)).filter_map(|k| {
        let cap = ceiling(cx, k)?;
        let t = s.of_type(cx, k);
        (!t.exhausted()).then_some((k, cap, t))
    });
    let (mut k, mut cap, mut t) = alive.next()?;
    for (nk, ncap, nt) in alive {
        if ncap >= cap.saturating_mul(2) {
            (k, cap, t) = (nk, ncap, nt);
        }
    }
    Some((k, t))
}

fn search(cx: &Ctx) -> Searched {
    let mut s = Search {
        probes: Vec::new(),
        banned_from: usize::MAX,
    };
    for e in &cx.input.results {
        if let Entry::Probe { ctx, kv, outcome } = *e {
            let k = cx.kv_index(kv);
            if outcome == ProbeOutcome::QualityFailed {
                s.banned_from = s.banned_from.min(k);
            }
            s.probes.push((ctx, k, outcome));
        }
    }
    // At the probe cap: the chosen type's best pass, or else the most precise type with one.
    let settle = |s: &Search, chosen: Option<(u64, usize)>| {
        chosen
            .or_else(|| (0..s.allowed(cx)).find_map(|k| s.lo(k).map(|c| (c, k))))
            .map_or(Searched::Stop(Stop::NoContext), |(c, k)| {
                Searched::Chosen(c, k)
            })
    };
    let Some((k, t)) = preferred(cx, &s) else {
        return settle(&s, None);
    };
    if s.probes.len() >= MAX_PROBES || t.open.is_empty() {
        return settle(&s, t.lo.map(|c| (c, k)));
    }
    // Nothing measured above the floor yet and nothing ruled out: the largest first. A memory
    // failure with a more precise type counts as a measured ceiling, so the search bisects.
    let hi_known =
        t.open.len() < t.above.len() || s.probes.iter().any(|&(_, pk, o)| o.memory() && pk < k);
    let ctx = if t.lo.is_none() && !hi_known {
        t.open.last().copied()
    } else {
        t.open.get(t.open.len() / 2).copied()
    };
    match ctx {
        Some(c) => Searched::Next(c, k),
        None => Searched::Stop(Stop::NoContext),
    }
}

/// The next step for `input`.
pub fn plan(input: &Input) -> Step {
    let cx = Ctx {
        input,
        usable: usable_bytes(&input.memory),
        rungs: input
            .ladder
            .iter()
            .copied()
            .filter(|&r| input.facts.n_ctx_train == 0 || r <= input.facts.n_ctx_train)
            .collect(),
    };
    let Some(&smallest) = cx.rungs.first() else {
        return Step::Fail(Stop::NoRung);
    };
    if kv_elements(&input.facts, smallest).is_none() {
        return Step::Fail(Stop::Unsizeable);
    }
    if !input.kv.iter().any(|&k| cx.fits(smallest, k)) {
        return Step::Fail(Stop::DoesNotFit);
    }
    let kv_at = |k: usize| input.kv.get(k).copied();
    let (ctx, kv) = match search(&cx) {
        Searched::Next(c, k) => {
            return match kv_at(k) {
                Some(kv) => Step::Probe {
                    ctx: c,
                    kv,
                    fill: fill_for(&input.facts, c),
                    estimate_mib: cx.estimate_mib(c, kv),
                },
                None => Step::Fail(Stop::NoContext),
            }
        }
        Searched::Stop(stop) => return Step::Fail(stop),
        Searched::Chosen(c, k) => match kv_at(k) {
            Some(kv) => (c, kv),
            None => return Step::Fail(Stop::NoContext),
        },
    };
    for id in Phase::ORDER {
        let outcome = input.results.iter().find_map(|e| match *e {
            Entry::Phase { id: p, outcome } if p == id => Some(outcome),
            _ => None,
        });
        match outcome {
            None => return Step::Phase { id, ctx, kv },
            // Sampling is optional: the caller restores it and goes on.
            Some(Outcome::Failed) if id != Phase::Sampling => return Step::Fail(Stop::PhaseFailed),
            Some(_) => {}
        }
    }
    let verifies: Vec<(u64, ProbeOutcome)> = input
        .results
        .iter()
        .filter_map(|e| match *e {
            Entry::Verify { ctx, outcome, .. } => Some((ctx, outcome)),
            _ => None,
        })
        .collect();
    let verified = match verifies.iter().find(|(_, o)| *o == ProbeOutcome::Passed) {
        Some(&(c, _)) => c,
        None => {
            let next = match verifies.iter().map(|&(c, _)| c).min() {
                None => Some(ctx),
                Some(_) if verifies.len() >= MAX_VERIFY => None,
                Some(low) => cx.rungs.iter().copied().filter(|&r| r < low).max(),
            };
            return match next {
                Some(c) => Step::Verify {
                    ctx: c,
                    kv,
                    fill: fill_for(&input.facts, c),
                    estimate_mib: cx.estimate_mib(c, kv),
                },
                None => Step::Fail(Stop::VerifyFailed),
            };
        }
    };
    match input.results.iter().find_map(|e| match *e {
        Entry::Serving { outcome } => Some(outcome),
        _ => None,
    }) {
        None => Step::Serving { ctx: verified, kv },
        Some(Outcome::Failed) => Step::Fail(Stop::ServingFailed),
        Some(_) => Step::Done { ctx: verified, kv },
    }
}

/// The reply for a step (keys sorted, so the bytes are stable).
pub fn step_json(step: &Step) -> String {
    let v = match *step {
        Step::Probe {
            ctx,
            kv,
            fill,
            estimate_mib,
        } => {
            json!({"step":"probe","ctx":ctx,"kv":kv.name(),"fill":fill,"estimateMib":estimate_mib})
        }
        Step::Verify {
            ctx,
            kv,
            fill,
            estimate_mib,
        } => {
            json!({"step":"verify","ctx":ctx,"kv":kv.name(),"fill":fill,"estimateMib":estimate_mib})
        }
        Step::Phase { id, ctx, kv } => {
            json!({"step":"phase","id":id.name(),"ctx":ctx,"kv":kv.name()})
        }
        Step::Serving { ctx, kv } => json!({"step":"serving","ctx":ctx,"kv":kv.name()}),
        Step::Done { ctx, kv } => json!({"step":"done","ctx":ctx,"kv":kv.name()}),
        Step::Fail(stop) => json!({"step":"fail","code":stop.code(),"message":stop.message()}),
    };
    v.to_string()
}

/// Parse and plan: `(0, step)` or `(1, {"error":"…"})`.
pub fn plan_json(text: &str) -> (u32, String) {
    match parse(text) {
        Ok(input) => (0, step_json(&plan(&input))),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const LADDER: [u64; 6] = [4096, 8192, 16384, 32768, 65536, 131072];

    fn llama8b() -> Facts {
        Facts {
            n_ctx_train: 131072,
            block_count: 32,
            head_count: 32,
            head_count_kv: vec![8],
            embedding_length: 4096,
            model_bytes: 5 * 1024 * 1024 * 1024,
            slots: 1,
            ..Facts::default()
        }
    }

    fn input(facts: Facts, budget_mib: u64, results: Vec<Entry>) -> Input {
        Input {
            facts,
            memory: Memory {
                budget_mib,
                mem_available_mib: None,
                reserve_mib: 0,
                floor_mib: 0,
                cache_ram_mib: 1024,
            },
            ladder: LADDER.to_vec(),
            kv: vec![KvType::Bf16, KvType::Q8_0],
            results,
        }
    }

    fn probe(ctx: u64, kv: KvType, outcome: ProbeOutcome) -> Entry {
        Entry::Probe { ctx, kv, outcome }
    }

    #[test]
    fn kv_bytes_match_the_js_estimate() {
        // Llama 3 8B: 32 layers x 8 KV heads x (128 + 128) x 131072 tokens at q8_0
        // = 8.5 GiB, which is kvCacheBytes(m, 131072) in llamacpp-autoconfig.cjs.
        let (e, fixed) = kv_elements(&llama8b(), 131072).unwrap();
        assert_eq!(fixed, 0);
        assert_eq!(e * 34 / 32, 9_126_805_504);
    }

    fn with_q5(mut i: Input) -> Input {
        i.kv = vec![KvType::Bf16, KvType::Q8_0, KvType::Q5_1, KvType::Q5_0];
        i
    }

    fn first_probe(i: &Input) -> (u64, KvType) {
        match plan(i) {
            Step::Probe { ctx, kv, .. } => (ctx, kv),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn precision_first_and_compact_only_when_it_doubles_the_context() {
        // 16 GiB: bf16 fits 64k, q8_0 also only 64k (128k is 0.2 GiB over): stay bf16 at 64k.
        let i = input(llama8b(), 16384, vec![]);
        match plan(&i) {
            Step::Probe { ctx, kv, fill, .. } => {
                assert_eq!((ctx, kv), (65536, KvType::Bf16));
                assert_eq!(fill, 65536 * 9 / 10 - 256);
            }
            s => panic!("{s:?}"),
        }
        // 10 GiB: bf16 fits 16k, q8_0 32k, exactly twice: q8_0.
        assert_eq!(
            first_probe(&input(llama8b(), 10 * 1024, vec![])),
            (32768, KvType::Q8_0)
        );
        // 20 GiB: bf16 64k, q8_0 the full 128k: q8_0.
        assert_eq!(
            first_probe(&input(llama8b(), 20 * 1024, vec![])),
            (131072, KvType::Q8_0)
        );
        // A bigger budget keeps bf16 at the trained maximum: nothing can double it.
        assert_eq!(
            first_probe(&input(llama8b(), 64 * 1024, vec![])),
            (131072, KvType::Bf16)
        );
    }

    #[test]
    fn q5_only_when_offered_and_only_when_it_doubles_the_choice() {
        // 16 GiB with q5 allowed: q8_0 does not double bf16's 64k, q5_1 fits 128k, which does.
        assert_eq!(
            first_probe(&with_q5(input(llama8b(), 16384, vec![]))),
            (131072, KvType::Q5_1)
        );
        // 10 GiB: q8_0 doubles bf16 (32k); q5_1 also only fits 32k, so q8_0 stays.
        assert_eq!(
            first_probe(&with_q5(input(llama8b(), 10 * 1024, vec![]))),
            (32768, KvType::Q8_0)
        );
        // 64 GiB: bf16 at the trained maximum whatever else is offered.
        assert_eq!(
            first_probe(&with_q5(input(llama8b(), 64 * 1024, vec![]))),
            (131072, KvType::Bf16)
        );
    }

    #[test]
    fn f16_in_place_of_bf16_plans_the_same() {
        let mut i = input(llama8b(), 16384, vec![]);
        i.kv = vec![KvType::F16, KvType::Q8_0];
        assert_eq!(first_probe(&i), (65536, KvType::F16));
    }

    #[test]
    fn rungs_over_the_estimate_do_not_count_as_measured_failures() {
        // 10 GiB: 64k and 128k fit no type, 32k fits with q8_0. The first probe is the largest rung
        // that fits (not the middle of the ladder), and passing it ends the search.
        let i = input(llama8b(), 10 * 1024, vec![]);
        let (ctx, kv) = first_probe(&i);
        assert_eq!((ctx, kv), (32768, KvType::Q8_0));
        let i = input(
            llama8b(),
            10 * 1024,
            vec![probe(ctx, kv, ProbeOutcome::Passed)],
        );
        assert!(matches!(
            plan(&i),
            Step::Phase {
                id: Phase::Sampling,
                ctx: 32768,
                ..
            }
        ));
    }

    #[test]
    fn memory_failure_steps_the_context_down_with_the_same_type() {
        let i = input(
            llama8b(),
            16384,
            vec![probe(65536, KvType::Bf16, ProbeOutcome::Oom)],
        );
        assert_eq!(first_probe(&i), (16384, KvType::Bf16));
        let i = input(
            llama8b(),
            16384,
            vec![probe(65536, KvType::Bf16, ProbeOutcome::LoadFailed)],
        );
        assert_eq!(first_probe(&i), (16384, KvType::Bf16));
    }

    #[test]
    fn a_type_that_fails_at_every_context_steps_to_the_next() {
        // bf16 refused even at the smallest rung: q8_0, bisecting (bf16's failure is a ceiling).
        let r = vec![probe(4096, KvType::Bf16, ProbeOutcome::LoadFailed)];
        assert_eq!(
            first_probe(&input(llama8b(), 16384, r.clone())),
            (16384, KvType::Q8_0)
        );
        // After a bisection down to nothing with bf16, the same.
        let r = vec![
            probe(65536, KvType::Bf16, ProbeOutcome::Oom),
            probe(16384, KvType::Bf16, ProbeOutcome::Oom),
            probe(8192, KvType::Bf16, ProbeOutcome::Oom),
            probe(4096, KvType::Bf16, ProbeOutcome::Oom),
        ];
        assert_eq!(
            first_probe(&input(llama8b(), 16384, r.clone())),
            (16384, KvType::Q8_0)
        );
        // A bf16 pass anywhere keeps bf16: its best pass is the choice.
        let r = vec![
            probe(65536, KvType::Bf16, ProbeOutcome::Oom),
            probe(16384, KvType::Bf16, ProbeOutcome::Passed),
            probe(32768, KvType::Bf16, ProbeOutcome::Oom),
        ];
        assert_eq!(
            plan(&input(llama8b(), 16384, r)),
            Step::Phase {
                id: Phase::Sampling,
                ctx: 16384,
                kv: KvType::Bf16
            }
        );
        // q8_0 (the floor) failing everywhere too: no context.
        let r = vec![
            probe(4096, KvType::Bf16, ProbeOutcome::Oom),
            probe(4096, KvType::Q8_0, ProbeOutcome::Oom),
        ];
        assert_eq!(
            plan(&input(llama8b(), 16384, r)),
            Step::Fail(Stop::NoContext)
        );
    }

    #[test]
    fn recall_failure_steps_down_and_a_pass_moves_on() {
        let mut r = vec![probe(131072, KvType::Bf16, ProbeOutcome::RecallFailed)];
        let i = input(llama8b(), 64 * 1024, r.clone());
        assert_eq!(first_probe(&i), (16384, KvType::Bf16));
        r.push(probe(16384, KvType::Bf16, ProbeOutcome::Passed));
        let (ctx, _) = first_probe(&input(llama8b(), 64 * 1024, r.clone()));
        assert_eq!(ctx, 65536);
        r.push(probe(65536, KvType::Bf16, ProbeOutcome::Passed));
        assert_eq!(
            plan(&input(llama8b(), 64 * 1024, r.clone())),
            Step::Phase {
                id: Phase::Sampling,
                ctx: 65536,
                kv: KvType::Bf16
            }
        );
    }

    #[test]
    fn quality_failure_rules_out_that_type_and_more_compact_ones() {
        // 10 GiB picks q8_0 at 32k; q8_0 failing quality leaves bf16, at its own largest rung.
        let r = vec![probe(32768, KvType::Q8_0, ProbeOutcome::QualityFailed)];
        assert_eq!(
            first_probe(&input(llama8b(), 10 * 1024, r)),
            (16384, KvType::Bf16)
        );
        // With q5 offered, a q5_1 quality failure goes back to bf16 (q8_0 does not double it).
        let r = vec![probe(131072, KvType::Q5_1, ProbeOutcome::QualityFailed)];
        assert_eq!(
            first_probe(&with_q5(input(llama8b(), 16384, r))),
            (65536, KvType::Bf16)
        );
    }

    #[test]
    fn phases_verify_and_serving_in_order() {
        let mut r = vec![probe(131072, KvType::Bf16, ProbeOutcome::Passed)];
        let big = 64 * 1024;
        for id in Phase::ORDER {
            assert_eq!(
                plan(&input(llama8b(), big, r.clone())),
                Step::Phase {
                    id,
                    ctx: 131072,
                    kv: KvType::Bf16
                }
            );
            r.push(Entry::Phase {
                id,
                outcome: if id == Phase::Sampling {
                    Outcome::Failed
                } else {
                    Outcome::Passed
                },
            });
        }
        assert!(matches!(
            plan(&input(llama8b(), big, r.clone())),
            Step::Verify { ctx: 131072, .. }
        ));
        r.push(Entry::Verify {
            ctx: 131072,
            kv: KvType::Bf16,
            outcome: ProbeOutcome::Oom,
        });
        assert!(matches!(
            plan(&input(llama8b(), big, r.clone())),
            Step::Verify { ctx: 65536, .. }
        ));
        r.push(Entry::Verify {
            ctx: 65536,
            kv: KvType::Bf16,
            outcome: ProbeOutcome::Passed,
        });
        assert_eq!(
            plan(&input(llama8b(), big, r.clone())),
            Step::Serving {
                ctx: 65536,
                kv: KvType::Bf16
            }
        );
        r.push(Entry::Serving {
            outcome: Outcome::Skipped,
        });
        assert_eq!(
            plan(&input(llama8b(), big, r)),
            Step::Done {
                ctx: 65536,
                kv: KvType::Bf16
            }
        );
    }

    #[test]
    fn stops_with_fixed_reasons() {
        assert_eq!(
            plan(&input(llama8b(), 4096, vec![])),
            Step::Fail(Stop::DoesNotFit)
        );
        let mut f = llama8b();
        f.n_ctx_train = 2048;
        assert_eq!(plan(&input(f, 65536, vec![])), Step::Fail(Stop::NoRung));
        let mut f = llama8b();
        f.head_count = 0;
        assert_eq!(plan(&input(f, 65536, vec![])), Step::Fail(Stop::Unsizeable));
        let r = vec![probe(4096, KvType::Bf16, ProbeOutcome::RecallFailed)];
        assert_eq!(
            plan(&input(llama8b(), 65536, r)),
            Step::Fail(Stop::NoContext)
        );
    }

    #[test]
    fn json_round_trip_and_refusals() {
        let (s, r) = plan_json(
            r#"{"facts":{"nCtxTrain":8192,"blockCount":2,"headCount":2,"embeddingLength":64,"modelBytes":1000},
                "memory":{"budgetMib":4096,"cacheRamMib":0},"ladder":[4096,8192],"kv":["f16"]}"#,
        );
        assert_eq!(s, 0);
        assert_eq!(
            r,
            r#"{"ctx":8192,"estimateMib":1080,"fill":7116,"kv":"f16","step":"probe"}"#
        );
        assert_eq!(plan_json("{").0, 1);
        assert_eq!(
            plan_json(&" ".repeat(MAX_INPUT_BYTES + 1)).1,
            r#"{"error":"too_large"}"#
        );
        let bad = [
            r#"{"facts":{},"memory":{},"ladder":[8192,4096],"kv":["f16"]}"#,
            r#"{"facts":{},"memory":{},"ladder":[4096],"kv":["f16","f16"]}"#,
            r#"{"facts":{},"memory":{},"ladder":[4096],"kv":["q2"]}"#,
            r#"{"facts":{},"memory":{},"ladder":["4096"],"kv":["f16"]}"#,
            r#"{"facts":{"blockCount":-1},"memory":{},"ladder":[4096],"kv":["f16"]}"#,
            r#"{"facts":{"blockCount":1.5},"memory":{},"ladder":[4096],"kv":["f16"]}"#,
            r#"{"facts":{},"memory":{},"ladder":[4096],"kv":["f16"],"results":[{"step":"probe","ctx":4096,"kv":"q8_0","outcome":"passed"}]}"#,
            r#"{"facts":{},"memory":{},"ladder":[4096],"kv":["f16"],"results":[{"step":"x","outcome":"passed"}]}"#,
        ];
        for b in bad {
            assert_eq!(plan_json(b), (1, r#"{"error":"input"}"#.to_owned()), "{b}");
        }
    }

    #[test]
    fn mem_available_less_reserve_and_floor_bounds_the_budget() {
        let mut i = input(llama8b(), 64 * 1024, vec![]);
        i.memory.mem_available_mib = Some(20 * 1024);
        i.memory.reserve_mib = 2560;
        i.memory.floor_mib = 2048;
        assert_eq!(
            usable_bytes(&i.memory),
            u128::from(20u64 * 1024 - 4608) * MIB
        );
        // 15.5 GiB usable: bf16 fits 32k, q8_0 64k, twice that: q8_0.
        assert_eq!(first_probe(&i), (65536, KvType::Q8_0));
    }

    #[test]
    fn sliding_window_and_hybrid_layouts() {
        // Gemma-style pattern: 5 local (window 1024) then 1 global, repeated.
        let mut f = llama8b();
        f.block_count = 6;
        f.sliding_window = 1024;
        f.sliding_window_pattern = vec![1, 1, 1, 1, 1, 0];
        let (e, _) = kv_elements(&f, 8192).unwrap();
        assert_eq!(e, 5 * 8 * 256 * 1024 + 8 * 256 * 8192);
        // Hybrid: every 4th layer has KV, the others a fixed recurrent state.
        let mut f = llama8b();
        f.full_attention_interval = 4;
        let (e, fixed) = kv_elements(&f, 8192).unwrap();
        assert_eq!(e, 8 * 8 * 256 * 8192);
        assert_eq!(fixed, 24 * SSM_STATE);
    }
}
