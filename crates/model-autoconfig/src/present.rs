//! Diff and presentation: autoconfig_core.py's `present`, what analyze() reports beside the
//! values: the values the container's baseline already covers, the quirks (baseline conflicts,
//! architecture, template and reasoning hints, multi-session, multi-GPU, projector, rope, MoE),
//! the knobs that do not apply, the preset the saved section matches, the diff against it
//! (#1152: changed keys in `values` order, then the superseded ones sorted), the keys Fill
//! must clear, and the quality warnings (`quality_warnings`, `_fmt_ctx`).

use crate::pyfloat::f;
use crate::pystr::{fixed, fmt_ctx, lower_eq, lower_tok, repr, strip, thousands};
use crate::pyval::{float_of, get, int_of, int_str, py_str};
use crate::values::{rope_owned, Values};
use crate::{Error, Work};
use model_files::json::Value;

/// Most values, saved-section keys and presets one request may hold.
pub const MAX_VALUES: usize = 256;
pub const MAX_CURRENT: usize = 1024;
pub const MAX_PRESETS: usize = 64;

/// AUTOCONFIG_DOMAIN (with SPEC_PROFILE_KEYS) minus _NEVER_CLEAR, sorted.
pub const DISPLACES: [&str; 34] = [
    "batch-size",
    "cache-ram",
    "cache-reuse",
    "cache-type-k",
    "cache-type-v",
    "chat-template-kwargs",
    "cont-batching",
    "context-shift",
    "cpu-moe",
    "ctx-size",
    "fit",
    "flash-attn",
    "image-max-tokens",
    "jinja",
    "keep",
    "mmproj",
    "mmproj-offload",
    "n-cpu-moe",
    "ngl",
    "parallel",
    "reasoning",
    "reasoning-format",
    "reasoning-preserve",
    "rope-scale",
    "rope-scaling",
    "spec-draft-model",
    "spec-draft-n-max",
    "spec-draft-n-min",
    "spec-draft-ngl",
    "spec-draft-p-min",
    "spec-type",
    "split-mode",
    "tensor-split",
    "ubatch-size",
];
const NEVER_CLEAR: &str = "model";
const ESSENTIAL: [&str; 19] = [
    "model",
    "ctx-size",
    "jinja",
    "cpu-moe",
    "n-cpu-moe",
    "parallel",
    "cont-batching",
    "context-shift",
    "keep",
    "batch-size",
    "ubatch-size",
    "cache-reuse",
    "reasoning",
    "reasoning-preserve",
    "mmproj",
    "mmproj-offload",
    "image-max-tokens",
    "tensor-split",
    "split-mode",
];
const PRESENT_KEYS: [&str; 21] = [
    "values",
    "current",
    "recommended",
    "rec_name",
    "rec_backend",
    "arch",
    "chat_template",
    "features",
    "n_sessions",
    "rec_ctx",
    "has_mmproj",
    "mmproj_vram_gb",
    "mmproj_gb",
    "rope",
    "native_ctx",
    "layers",
    "experts",
    "offload",
    "presets",
    "model_rel",
    "general",
];
const MIN_USEFUL_CTX: i128 = 16384;
const SUB_Q4_PARAM_LIMIT: f64 = 100_000_000_000.0;
/// _RESERVE_PER_GPU and int((_MODEL_OVERHEAD_SPLIT - 1) * 100).
const RESERVE_PER_GPU: f64 = 1.0;
const MODEL_OVERHEAD_SPLIT: f64 = 1.08;

fn field<'a>(v: &'a Value, k: &str) -> Result<&'a Value, Error> {
    v.get(k).ok_or(Error::Schema("present: missing field"))
}

fn boolean(v: &Value, what: &'static str) -> Result<bool, Error> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(Error::Schema(what)),
    }
}

fn int(v: &Value, what: &'static str) -> Result<i128, Error> {
    match v {
        Value::Int(_) => crate::pyval::big(v, what),
        _ => Err(Error::Schema(what)),
    }
}

fn string<'a>(v: &'a Value, what: &'static str) -> Result<&'a str, Error> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(Error::Schema(what)),
    }
}

/// `d.get(k)` of something Python calls `.get` on: a dict, else AttributeError.
fn dict_get<'a>(d: &'a Value, k: &str) -> Result<&'a Value, Error> {
    match d {
        Value::Obj(_) => Ok(get(d, k)),
        _ => Err(Error::Python("AttributeError")),
    }
}

/// `x or {}` then `.get(k)`.
fn or_dict_get<'a>(d: &'a Value, k: &str) -> Result<&'a Value, Error> {
    if d.truthy() {
        dict_get(d, k)
    } else {
        Ok(&Value::Null)
    }
}

/// `str(a).lower() == str(b).lower()`.
fn str_lower_eq(a: &Value, b: &str) -> Result<bool, Error> {
    lower_eq(&py_str(a, "a baseline value")?, b)
}

/// The leftmost match of `(?:^|[-_.])(?:UD-)?(IQ[123]\w*|Q[123](?:_[\w]+)*)(?:[-_.]|$)` (re.I)
/// in an ASCII string, as Python's `re.search` finds it (backtracking order included): group 1.
fn sub_q4(name: &str) -> Option<&str> {
    let b = name.as_bytes();
    let n = b.len();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let sep = |c: u8| matches!(c, b'-' | b'_' | b'.');
    let at = |i: usize| b.get(i).copied();
    let ci = |i: usize, c: u8| at(i).is_some_and(|x| x.eq_ignore_ascii_case(&c));
    let digit = |i: usize| at(i).is_some_and(|x| matches!(x, b'1'..=b'3'));
    // `$`: the end, or just before a newline that ends the string.
    let end_ok =
        |e: usize| e == n || (e + 1 == n && at(e) == Some(b'\n')) || at(e).is_some_and(sep);
    let run_end = |from: usize| (from..n).find(|&i| !at(i).is_some_and(word)).unwrap_or(n);
    let group = |gs: usize| -> Option<usize> {
        if ci(gs, b'I') && ci(gs + 1, b'Q') && digit(gs + 2) {
            let m = run_end(gs + 3);
            return (gs + 3..=m).rev().find(|&e| end_ok(e));
        }
        if ci(gs, b'Q') && digit(gs + 1) {
            let p0 = gs + 2;
            if at(p0) == Some(b'_') {
                let m = run_end(p0);
                if let Some(e) = (p0 + 2..=m).rev().find(|&e| end_ok(e)) {
                    return Some(e);
                }
            }
            return end_ok(p0).then_some(p0);
        }
        None
    };
    for start in 0..=n {
        let mut starts = Vec::with_capacity(2);
        if start == 0 {
            starts.push(0);
        }
        if at(start).is_some_and(sep) {
            starts.push(start + 1);
        }
        for g in starts {
            let mut tries = Vec::with_capacity(2);
            if ci(g, b'U') && ci(g + 1, b'D') && at(g + 2) == Some(b'-') {
                tries.push(g + 3);
            }
            tries.push(g);
            for gs in tries {
                if let Some(e) = group(gs) {
                    return name.get(gs..e);
                }
            }
        }
    }
    None
}

/// `quality_warnings(model_rel=..., params=..., recommended_ctx=..., native_ctx=...)`.
pub fn quality_warnings(
    model_rel: &Value,
    params: &Value,
    rec_ctx: i128,
    native_ctx: i128,
) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let rel = match model_rel {
        Value::Str(s) => s.as_str(),
        v if !v.truthy() => "",
        _ => return Err(Error::Python("AttributeError")),
    };
    let name = rel.rsplit('/').next().unwrap_or("");
    if !name.is_empty() {
        if !name.is_ascii() {
            return Err(Error::Unsupported("a non-ASCII model file name"));
        }
        if let Some(quant) = sub_q4(name) {
            if !params.truthy() || float_of(params, "params")? < SUB_Q4_PARAM_LIMIT {
                out.push(format!(
                    "{} is below Q4; on a model this size that usually costs more quality than the memory it saves.",
                    quant.to_ascii_uppercase()
                ));
            }
        }
    }
    let usable = if rec_ctx != 0 { rec_ctx } else { native_ctx };
    if usable != 0 && usable < MIN_USEFUL_CTX {
        out.push(format!(
            "{} tokens of context is little use once tool definitions and results are in the prompt; {} is a sensible floor.",
            thousands(usable),
            thousands(MIN_USEFUL_CTX)
        ));
    }
    Ok(out)
}

/// `int(s)` of a saved string, where any character from U+0080 to U+024F other than the two
/// spaces there (U+0085, U+00A0) makes it a certain ValueError: none of them is a decimal digit
/// (checked against CPython 3.12), so Python's answer needs no Unicode table. Other non-ASCII
/// text is refused, as int_str refuses it.
fn saved_int(s: &str) -> Result<i128, Error> {
    if s.chars()
        .any(|c| ('\u{80}'..'\u{250}').contains(&c) && !crate::pyval::is_py_space(c))
    {
        return Err(Error::Python("ValueError"));
    }
    int_str(s, "ctx-size")
}

/// A saved section, every value `str()`-ed (the ini holds strings).
fn current_strs(cur: &Value, work: &mut Work) -> Result<Values, Error> {
    let Value::Obj(pairs) = cur else {
        return Err(Error::Python("AttributeError"));
    };
    if pairs.len() > MAX_CURRENT {
        return Err(Error::OutOfRange("present.current"));
    }
    work.charge(pairs.len().saturating_mul(pairs.len()))?;
    let mut out = Values::default();
    for (k, v) in pairs {
        out.set(k, py_str(v, "a saved value")?);
    }
    Ok(out)
}

struct Preset<'a> {
    key: &'a str,
    ctx: i128,
    kind: &'a str,
    ngl: i128,
    n_cpu_moe: i128,
}

fn presets<'a>(v: &'a Value, work: &mut Work) -> Result<Vec<Preset<'a>>, Error> {
    let Value::Arr(items) = v else {
        return Err(Error::Schema("present.presets"));
    };
    if items.len() > MAX_PRESETS {
        return Err(Error::OutOfRange("present.presets"));
    }
    work.charge(items.len())?;
    items
        .iter()
        .map(|p| {
            Ok(Preset {
                key: string(field(p, "key")?, "preset.key")?,
                ctx: int(field(p, "ctx")?, "preset.ctx")?,
                kind: string(field(p, "offload_kind")?, "preset.offload_kind")?,
                ngl: int(field(p, "ngl")?, "preset.ngl")?,
                n_cpu_moe: int(field(p, "n_cpu_moe")?, "preset.n_cpu_moe")?,
            })
        })
        .collect()
}

pub struct Report {
    pub minimal: Values,
    pub redundant: Values,
    pub quirks: Vec<String>,
    pub unavailable: Vec<String>,
    pub current_preset: String,
    pub current_diff: Vec<String>,
    pub displaced: Vec<String>,
    pub warnings: Vec<String>,
}

/// `isinstance(experts, int) and experts > 1`.
fn is_moe(experts: &Value) -> bool {
    match experts {
        Value::Int(i) => {
            let s = i.to_string();
            !s.starts_with('-') && !i.is_zero() && s != "1"
        }
        _ => false,
    }
}

/// `present(inp)`.
#[allow(clippy::too_many_lines)]
pub fn present(inp: &Value, work: &mut Work) -> Result<Report, Error> {
    let Value::Obj(pairs) = inp else {
        return Err(Error::Schema("present"));
    };
    if pairs
        .iter()
        .any(|(k, _)| !PRESENT_KEYS.contains(&k.as_str()))
    {
        return Err(Error::Schema("present"));
    }
    let mut values = Values::default();
    match field(inp, "values")? {
        Value::Arr(items) if items.len() <= MAX_VALUES => {
            work.charge(items.len().saturating_mul(items.len()))?;
            for item in items {
                match item {
                    Value::Arr(p) => match p.as_slice() {
                        [Value::Str(k), Value::Str(v)] => values.set(k, v.clone()),
                        _ => return Err(Error::Schema("present.values")),
                    },
                    _ => return Err(Error::Schema("present.values")),
                }
            }
        }
        Value::Arr(_) => return Err(Error::OutOfRange("present.values")),
        _ => return Err(Error::Schema("present.values")),
    }
    let current = field(inp, "current")?;
    let recommended = boolean(field(inp, "recommended")?, "present.recommended")?;
    let rec_backend = field(inp, "rec_backend")?;
    let n_sessions = int(field(inp, "n_sessions")?, "present.n_sessions")?;
    if !(1..=8).contains(&n_sessions) {
        return Err(Error::OutOfRange("present.n_sessions"));
    }
    let rec_ctx = int(field(inp, "rec_ctx")?, "present.rec_ctx")?;
    let native_ctx = int(field(inp, "native_ctx")?, "present.native_ctx")?;
    let layers = int(field(inp, "layers")?, "present.layers")?;
    let m = field(inp, "rope")?;
    let experts = field(inp, "experts")?;
    let features_v = field(inp, "features")?;
    let empty = Value::Obj(Vec::new());
    let features = if features_v.truthy() {
        features_v
    } else {
        &empty
    };
    let feature = |k: &str| dict_get(features, k).map(Value::truthy);

    // baseline redundancy
    let mut redundant = Values::default();
    let mut minimal = values.clone();
    let base = if recommended {
        or_dict_get(rec_backend, "baseline")?
    } else {
        &Value::Null
    };
    let base = if base.truthy() { base } else { &empty };
    if recommended {
        work.charge(values.0.len().saturating_mul(4))?;
        for (k, v) in &values.0 {
            let bv = dict_get(base, k)?;
            if *bv != Value::Null && str_lower_eq(bv, v)? {
                redundant.set(k, v.clone());
                minimal.pop(k);
            }
        }
    }
    for k in ESSENTIAL {
        if let Some(v) = values.get(k) {
            if minimal.get(k).is_none() {
                minimal.set(k, v.to_owned());
            }
        }
    }

    let mut quirks: Vec<String> = Vec::new();
    if recommended {
        let mut conflicts = Vec::new();
        for (k, v) in &values.0 {
            if let Some(bv) = base.get(k) {
                if !str_lower_eq(bv, v)? {
                    conflicts.push(format!(
                        "`{k}`: preset wants {v}, container forces {}",
                        py_str(bv, "a baseline value")?
                    ));
                }
            }
        }
        if !conflicts.is_empty() {
            quirks.push(format!(
                "CONFLICT \u{2014} the container's CLI args override models.ini, so these preset values \
                 will NOT take effect: {}. Fix by removing those flags from the `{}` command in your compose file \
                 so per-model presets can control them. Keep only router-level args there \
                 (--models-dir, --models-preset, --host, --port, --models-max).",
                conflicts.join("; "),
                py_str(field(inp, "rec_name")?, "rec_name")?
            ));
        }
    }

    let arch = field(inp, "arch")?;
    let gemma = match arch {
        Value::Str(s) => lower_tok(s).starts_with("gemma"),
        v if !v.truthy() => false,
        _ => return Err(Error::Python("AttributeError")),
    };
    if gemma {
        quirks.push(
            "Gemma sliding-window attention: the ctx\u{2192}KV math above assumes swa-full=false (default). \
             Enabling swa-full multiplies full-attn KV ~5\u{d7} and will OOM."
                .to_owned(),
        );
    }
    if !boolean(field(inp, "chat_template")?, "present.chat_template")? {
        quirks.push(
            "No embedded chat template \u{2014} you'll need to set `chat-template` or `chat-template-file` \
             manually to get correct multi-turn formatting."
                .to_owned(),
        );
    }
    let mut detected: Vec<&str> = Vec::new();
    if feature("accepts_enable_thinking")? {
        detected.push("`enable_thinking` kwarg (set true/false)");
    }
    if feature("accepts_reasoning_effort")? {
        detected.push("`reasoning_effort` kwarg (low/medium/high)");
    }
    if feature("accepts_preserve_thinking")? {
        detected.push("`preserve_thinking` kwarg (keep reasoning across turns)");
    }
    if !detected.is_empty() {
        quirks.push(format!(
            "Chat template supports {}. Thinking is enabled via the dedicated `reasoning = on` flag (setting enable_thinking through \
             chat-template-kwargs is deprecated in current llama.cpp); anything without a dedicated flag, \
             such as reasoning_effort, is pre-filled into `chat-template-kwargs`. Override either in the form.",
            detected.join(", ")
        ));
    }
    if feature("uses_think_tags")? || feature("uses_channel_thought")? {
        quirks.push(
            "Model emits <think> or channel-based thought tags. Set `reasoning-format = deepseek` so OpenAI-compatible \
             clients (OpenWebUI etc.) render thoughts as a collapsible instead of inline in the answer."
                .to_owned(),
        );
    }

    if n_sessions > 1 && rec_ctx > 0 {
        let per = fmt_ctx(rec_ctx);
        quirks.push(format!(
            "Sizing for {n_sessions} concurrent sessions: each user gets {per} of context, \
             llama-server allocates ctx-size = {} total across the -np {n_sessions} slots. \
             KV cache is sized against the total; per-session throughput drops roughly linearly with load.",
            fmt_ctx(rec_ctx * n_sessions)
        ));
        quirks.push(format!(
            "When a session hits its {per} cap it will SLIDE: oldest tokens drop, \
             generation continues. Set `context-shift = on` and `keep = 256` (tune to your system-prompt \
             length in tokens so instructions survive the shift). To fail hard instead of forgetting old \
             turns, set `context-shift = off` in the form."
        ));
        quirks.push(
            "TTFT tuning for multi-slot: set `batch-size = 4096` (safe, negligible VRAM). \
             To also improve prompt-eval when a second session arrives mid-generation, bump `ubatch-size` \
             in the form \u{2014} this is the main lever, but it's costly. On layer-split multi-GPU, compute-buffer \
             VRAM grows as ~8 \u{d7} ubatch \u{d7} layers \u{d7} hidden. A dense 27B (64L, 5120H) at ubatch=2048 costs ~4.7 GB \
             of compute buffers across cards. Don't bump ubatch above 512 unless you have 2+ GB of measured \
             free VRAM after boot. MoE with CPU offload has much more room to work with."
                .to_owned(),
        );
    }

    if recommended {
        let gc = match rec_backend {
            Value::Obj(_) => match rec_backend.get("gpu_count") {
                None => 1,
                Some(v) => int_of(v, "gpu_count")?,
            },
            v if !v.truthy() => 1,
            _ => return Err(Error::Python("AttributeError")),
        };
        if gc > 1 {
            let pct = ((MODEL_OVERHEAD_SPLIT - 1.0) * 100.0).trunc();
            quirks.push(format!(
                "Multi-GPU backend ({gc} cards, layer-split): reserved \
                 {} GB total for CUDA runtime \
                 (0.5 GB \u{d7} {gc}) and applied a {}% model-VRAM \
                 multiplier for cross-card handoffs. Real cap will be a bit lower than pure sum-of-VRAMs.",
                fixed(RESERVE_PER_GPU * f(gc), 1)?,
                crate::pyfloat::trunc_int(pct)?
            ));
        }
    }

    if boolean(field(inp, "has_mmproj")?, "present.has_mmproj")? {
        let vram = float_of(field(inp, "mmproj_vram_gb")?, "mmproj_vram_gb")?;
        let gb = float_of(field(inp, "mmproj_gb")?, "mmproj_gb")?;
        quirks.push(format!(
            "Multimodal model (mmproj companion present \u{2014} vision, audio, or other modality). \
             Reserved {} GB for the projector ({} GB weights + \
             0.5 GB encoder scratch). If several projector precisions ship in the \
             directory the smallest is chosen, since it competes directly with the KV cache.\n\
             TREAT THIS CTX AS OPTIMISTIC AND VERIFY IT LOADS. The projector and its encoder buffer are \
             NOT layer-split \u{2014} both land entirely on the main GPU \u{2014} so the real limit is that one card, \
             not the pooled total this estimate is based on. Worse, measured encoder scratch varies ~4x \
             between models (0.55 GB on a 27B with a 0.87 GB projector vs 2.3 GB on Qwen3-VL-4B with a \
             0.78 GB one) and is not derivable from GGUF metadata, so no single constant fits all. \
             If it OOMs on device 0 while the other card still shows free VRAM, that is exactly this \
             limitation \u{2014} step ctx-size down until it loads.",
            fixed(vram, 2)?,
            fixed(gb, 2)?
        ));
    }

    let owned = rope_owned(m)?;
    if values.get("rope-scaling") == Some("linear") && !owned && native_ctx > 0 {
        let scale = values.get("rope-scale").unwrap_or("?");
        quirks.push(format!(
            "Extended ctx from native {} to {} \
             via `rope-scaling=linear, rope-scale={scale}`. Linear scaling degrades quality gracefully up \
             to ~2\u{d7} native; beyond that outputs get progressively worse. Drop ctx-size in the form to back off.",
            fmt_ctx(native_ctx),
            fmt_ctx(rec_ctx)
        ));
    }

    let rope_type = dict_get(m, "rope_scaling_type")?;
    let moe = is_moe(experts);
    let mut unavailable = Vec::new();
    if !moe {
        unavailable.push("cpu-moe / n-cpu-moe (not MoE)".to_owned());
    }
    if owned {
        unavailable.push(
            "rope-scaling (the GGUF sets it per layer; a preset value would override it)"
                .to_owned(),
        );
    } else if !rope_type.truthy() || lower_tok(&py_str(rope_type, "rope_scaling_type")?) == "none" {
        unavailable.push("rope-scaling (model doesn't declare one)".to_owned());
    }

    if moe {
        let experts_s = py_str(experts, "expert_count")?;
        if recommended {
            let (kind, n_cm) = match field(inp, "offload")? {
                Value::Arr(o) => match o.as_slice() {
                    [Value::Str(k), n @ Value::Int(_)] => (k.as_str(), int(n, "offload")?),
                    _ => return Err(Error::Schema("present.offload")),
                },
                _ => return Err(Error::Schema("present.offload")),
            };
            if kind == "cpu-moe" || kind == "n-cpu-moe" {
                let est = if kind == "cpu-moe" {
                    format!("all {layers} layers'")
                } else {
                    format!("roughly the first {n_cm} of {layers} layers'")
                };
                quirks.push(format!(
                    "MoE model ({experts_s} experts): needs expert weights on the CPU \u{2014} estimated {est} worth. \
                     Placement is left to llama.cpp: `fit = on` with ngl, tensor-split and n-cpu-moe all unset, \
                     so llama-server sizes it at load time against real free VRAM. That estimate is advisory; \
                     the loader decides. Expect slower generation than a fully-GPU model."
                ));
                quirks.push(
                    "Pinning ngl or tensor-split here would DISABLE that fitting (`--fit` only adjusts unset \
                     arguments \u{2014} the log says \"n_gpu_layers already set by user to 999, abort\"), and our own \
                     placement maths has no term for compute buffers, which reached 3.6 GiB on a single card \
                     on a 177B model at 256K ctx. Leave them unset unless you are tuning by measurement."
                        .to_owned(),
                );
            } else {
                quirks.push(format!(
                    "MoE model ({experts_s} experts): fits fully on GPU at this ctx \u{2014} no CPU offload needed."
                ));
            }
        } else {
            quirks.push(format!(
                "MoE model ({experts_s} experts): does not fit even with all experts offloaded to CPU. \
                 You need a bigger GPU, a smaller quant, or a shorter context."
            ));
        }
    }

    let presets = presets(field(inp, "presets")?, work)?;
    let mut current_preset = String::new();
    let cur = if current.truthy() {
        Some(current_strs(current, work)?)
    } else {
        None
    };
    if let (Some(cur), false) = (&cur, presets.is_empty()) {
        let cur_ctx = match cur.get("ctx-size") {
            Some(s) if !s.is_empty() => match saved_int(s) {
                Ok(n) => n,
                Err(Error::Python("ValueError")) => 0,
                Err(e) => return Err(e),
            },
            _ => 0,
        };
        let ngl = strip(cur.get("ngl").unwrap_or(""));
        let ncm = strip(cur.get("n-cpu-moe").unwrap_or(""));
        for p in &presets {
            if cur_ctx != p.ctx.saturating_mul(n_sessions) {
                continue;
            }
            let ok = match p.kind {
                "ngl" => ngl == p.ngl.to_string(),
                "n-cpu-moe" => ncm == p.n_cpu_moe.to_string(),
                "cpu-moe" => {
                    let c = lower_tok(cur.get("cpu-moe").unwrap_or(""));
                    c == "true" || c == "on" || c == "1"
                }
                _ => (ngl.is_empty() || ngl == "999") && ncm.is_empty(),
            };
            if ok {
                current_preset = p.key.to_owned();
                break;
            }
        }
    }

    let mut current_diff = Vec::new();
    if let Some(cur) = &cur {
        work.charge(values.0.len().saturating_mul(cur.0.len().max(1)))?;
        for (k, v) in &values.0 {
            let c = cur.get(k);
            if c.unwrap_or("") != v {
                match c {
                    Some(c) => {
                        current_diff.push(format!("{k}: {} \u{2192} {}", repr(c)?, repr(v)?))
                    }
                    None => current_diff.push(format!("{k}: unset \u{2192} {}", repr(v)?)),
                }
            }
        }
        for k in DISPLACES {
            if let Some(c) = cur.get(k) {
                if !c.is_empty() && values.get(k).is_none() {
                    current_diff.push(format!("{k}: {} \u{2192} unset (superseded)", repr(c)?));
                }
            }
        }
    }
    let displaced: Vec<String> = DISPLACES
        .iter()
        .filter(|k| values.get(k).is_none())
        .map(|k| (*k).to_owned())
        .collect();

    let mut gaps: Vec<&str> = values
        .0
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| *k != NEVER_CLEAR && !DISPLACES.contains(k))
        .collect();
    gaps.sort_unstable();
    gaps.dedup();
    if !gaps.is_empty() {
        let names: Vec<String> = gaps.iter().map(|g| format!("`{g}`")).collect();
        quirks.push(format!(
            "Autoconfig set {}, which AUTOCONFIG_DOMAIN does not declare. Fill will not clear \
             {} on a later run, so a stale value could survive. Add them to the domain.",
            names.join(", "),
            if gaps.len() > 1 { "them" } else { "it" }
        ));
    }

    let params = or_dict_get(field(inp, "general")?, "params_raw")?;
    let warnings = quality_warnings(field(inp, "model_rel")?, params, rec_ctx, native_ctx)?;
    Ok(Report {
        minimal,
        redundant,
        quirks,
        unavailable,
        current_preset,
        current_diff,
        displaced,
        warnings,
    })
}

fn json_strs(out: &mut String, items: &[String]) {
    out.push('[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        crate::json_str(out, s);
    }
    out.push(']');
}

/// The report as `present` returns it.
pub fn report_json(r: &Report) -> String {
    let mut out = String::from("{\"minimal\":");
    out.push_str(&crate::values::values_json(&r.minimal));
    out.push_str(",\"redundant\":");
    out.push_str(&crate::values::values_json(&r.redundant));
    out.push_str(",\"quirks\":");
    json_strs(&mut out, &r.quirks);
    out.push_str(",\"unavailable\":");
    json_strs(&mut out, &r.unavailable);
    out.push_str(",\"current_preset\":");
    crate::json_str(&mut out, &r.current_preset);
    out.push_str(",\"current_diff\":");
    json_strs(&mut out, &r.current_diff);
    out.push_str(",\"displaced\":");
    json_strs(&mut out, &r.displaced);
    out.push_str(",\"warnings\":");
    json_strs(&mut out, &r.warnings);
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_q4_matches_python() {
        // Checked against CPython 3.12's re.search(...)[1].
        for (name, want) in [
            ("Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf", Some("IQ3_XXS")),
            ("gpt-oss-20b-Q4_K_M.gguf", None),
            ("m-Q2_K.gguf", Some("Q2_K")),
            ("huge-UD-IQ2_M.gguf", Some("IQ2_M")),
            ("x-q3_k_s-y", Some("q3_k_s")),
            ("Q2", Some("Q2")),
            ("Q2_", Some("Q2")),
            ("Q2__K.gguf", Some("Q2__K")),
            ("IQ3xx", Some("IQ3xx")),
            ("aQ2_K", None),
            ("m-Q3\n", Some("Q3")),
        ] {
            assert_eq!(sub_q4(name), want, "{name:?}");
        }
    }

    #[test]
    fn displaces_is_sorted() {
        assert!(DISPLACES.is_sorted() && DISPLACES.windows(2).all(|w| w.first() != w.last()));
    }
}
