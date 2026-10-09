//! `summarize()` from gguf_meta.py: the shape the model-manager API, autoconfig and the
//! config panel consume. Field names, types and null-vs-absent match the Python exactly.

use std::collections::HashMap;

use crate::json::{Json, PyInt};
use crate::pyfmt::{py_float_repr, py_str, truthy};
use crate::raw::{Raw, Value, MAX_ARRAY_ELEMENTS_KEPT};

/// Raised where Python's `summarize()` itself raises (an unhashable `general.file_type`). The message follows Python's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryError(pub String);

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SummaryError {}

/// Subset of llama.cpp LlamaFileType — enough to name every real-world GGUF quant.
pub fn file_type_name(n: i128) -> Option<&'static str> {
    Some(match n {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        33 => "Q4_0_4_4",
        34 => "Q4_0_4_8",
        35 => "Q4_0_8_8",
        36 => "TQ1_0",
        37 => "TQ2_0",
        _ => return None,
    })
}

/// `int(f)` for a float: truncates toward zero. [`summarize`] maps NaN/±Inf to `None` before
/// any conversion, so the non-finite arms only guard direct misuse.
fn float_to_int(f: f64) -> Result<PyInt, SummaryError> {
    if f.is_nan() {
        return Err(SummaryError("cannot convert float NaN to integer".into()));
    }
    if f.is_infinite() {
        return Err(SummaryError(
            "cannot convert float infinity to integer".into(),
        ));
    }
    let t = f.trunc();
    if t.abs() < 1.7e38 {
        return Ok(PyInt::Small(t as i128));
    }
    Ok(PyInt::Big(big_integral_decimal(t)))
}

/// Exact decimal digits of a finite, integral f64 of magnitude >= 2**53.
fn big_integral_decimal(t: f64) -> String {
    let bits = t.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    let mantissa = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
    let shift = exp_bits - 1075; // t == mantissa * 2**shift, shift > 0 here
                                 // Little-endian base-1e9 limbs.
    let mut limbs: Vec<u64> = vec![mantissa % 1_000_000_000, mantissa / 1_000_000_000];
    for _ in 0..shift.max(0) {
        let mut carry = 0u64;
        for limb in limbs.iter_mut() {
            let v = *limb * 2 + carry;
            *limb = v % 1_000_000_000;
            carry = v / 1_000_000_000;
        }
        if carry > 0 {
            limbs.push(carry);
        }
    }
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut s = String::new();
    if t < 0.0 {
        s.push('-');
    }
    let mut iter = limbs.iter().rev();
    if let Some(top) = iter.next() {
        s.push_str(&top.to_string());
    }
    for limb in iter {
        s.push_str(&format!("{limb:09}"));
    }
    s
}

/// `int(x)` for something `isinstance(x, (int, float))` accepts (bool included).
fn numeric_int(v: &Value) -> Result<Option<PyInt>, SummaryError> {
    Ok(match v {
        Value::Bool(b) => Some(PyInt::Small(i128::from(*b))),
        Value::Int(i) => Some(PyInt::Small(*i)),
        Value::Float(f) => Some(float_to_int(*f)?),
        _ => None,
    })
}

/// The most common numeric value of `items`, the first seen winning a tie (`_scalar_int`'s
/// `max(counts, key=…)` over an insertion-ordered dict).
fn most_common(items: &[Value]) -> Result<Option<PyInt>, SummaryError> {
    let mut counts: Vec<(PyInt, usize)> = Vec::new();
    let mut index: HashMap<PyInt, usize> = HashMap::new();
    for x in items {
        if let Some(k) = numeric_int(x)? {
            match index.get(&k).and_then(|&i| counts.get_mut(i)) {
                Some((_, n)) => *n += 1,
                None => {
                    index.insert(k.clone(), counts.len());
                    counts.push((k, 1));
                }
            }
        }
    }
    let mut best: Option<&(PyInt, usize)> = None;
    for entry in &counts {
        if best.is_none_or(|b| entry.1 > b.1) {
            best = Some(entry);
        }
    }
    Ok(best.map(|(k, _)| k.clone()))
}

/// `_scalar_int`: unwrap array summaries and whole per-layer lists (longer than any sample,
/// #1186) to their most common value, and shorter lists to their first element.
fn scalar_int(v: Option<&Value>) -> Result<Option<PyInt>, SummaryError> {
    let Some(v) = v else { return Ok(None) };
    match v {
        Value::Bool(_) | Value::Int(_) | Value::Float(_) => numeric_int(v),
        Value::ArraySummary { sample, .. } => most_common(sample),
        Value::List(items) if items.len() as u64 > MAX_ARRAY_ELEMENTS_KEPT => most_common(items),
        Value::List(items) => match items.first() {
            Some(first) => numeric_int(first),
            None => Ok(None),
        },
        Value::Str(_) | Value::None => Ok(None),
    }
}

/// Python's ordering of two ints (a `Big` only ever comes from `int()` of a huge float).
fn py_int_cmp(a: &PyInt, b: &PyInt) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if let (PyInt::Small(x), PyInt::Small(y)) = (a, b) {
        return x.cmp(y);
    }
    let (sa, sb) = (a.to_string(), b.to_string());
    let (na, nb) = (sa.starts_with('-'), sb.starts_with('-'));
    let (da, db) = (sa.trim_start_matches('-'), sb.trim_start_matches('-'));
    let mag = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
    match (na, nb) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => mag,
        (true, true) => mag.reverse(),
    }
}

/// `_kv_dim_int`: a KV-cache dimension. A whole per-layer list (#1186) gives its largest value
/// (Python's `max`, the first of equals), so the cache is never sized below the old sample's
/// most common value; everything else reads as [`scalar_int`].
fn kv_dim_int(v: Option<&Value>) -> Result<Option<PyInt>, SummaryError> {
    match v {
        Some(Value::List(items)) if items.len() as u64 > MAX_ARRAY_ELEMENTS_KEPT => {
            let mut best: Option<PyInt> = None;
            for x in items {
                if let Some(k) = numeric_int(x)? {
                    if best
                        .as_ref()
                        .is_none_or(|b| py_int_cmp(&k, b) == std::cmp::Ordering::Greater)
                    {
                        best = Some(k);
                    }
                }
            }
            Ok(best)
        }
        other => scalar_int(other),
    }
}

fn scalar_float(v: Option<&Value>) -> Result<Option<f64>, SummaryError> {
    match v {
        Some(Value::Bool(b)) => Ok(Some(if *b { 1.0 } else { 0.0 })),
        Some(Value::Int(i)) => Ok(Some(*i as f64)),
        Some(Value::Float(f)) => Ok(Some(*f)),
        other => Ok(scalar_int(other)?.map(|n| n.to_f64())),
    }
}

fn int_json(v: Option<PyInt>) -> Json {
    v.map_or(Json::Null, Json::Int)
}

fn float_json(v: Option<f64>) -> Json {
    v.map_or(Json::Null, Json::Float)
}

/// `_fmt_params`: "7.2 B" style parameter counts.
fn fmt_params(v: Option<&Value>) -> Result<Json, SummaryError> {
    let n = match v {
        Some(Value::Bool(b)) => {
            if !*b {
                return Ok(Json::Null);
            }
            1.0
        }
        Some(Value::Int(i)) => {
            if *i <= 0 {
                return Ok(Json::Null);
            }
            *i as f64
        }
        Some(Value::Float(f)) => {
            if *f <= 0.0 {
                return Ok(Json::Null);
            }
            *f
        }
        _ => return Ok(Json::Null),
    };
    let s = if n >= 1e12 {
        format!("{:.1} T", n / 1e12)
    } else if n >= 1e9 {
        format!("{:.1} B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1} K", n / 1e3)
    } else {
        float_to_int(n)?.to_string()
    };
    Ok(Json::Str(s))
}

fn quant_name(v: Option<&Value>) -> Result<Json, SummaryError> {
    let named =
        |n: i128, fallback: String| Json::Str(file_type_name(n).map_or(fallback, str::to_string));
    Ok(match v {
        None | Some(Value::None) => Json::Null,
        Some(Value::Bool(b)) => named(i128::from(*b), if *b { "True" } else { "False" }.into()),
        Some(Value::Int(i)) => named(*i, i.to_string()),
        Some(Value::Float(f)) => {
            let integral = f.is_finite() && f.fract() == 0.0 && f.abs() < 1e18;
            if integral {
                named(*f as i128, py_float_repr(*f))
            } else {
                Json::Str(py_float_repr(*f))
            }
        }
        Some(Value::Str(s)) => Json::Str(s.clone()),
        Some(Value::List(_)) => return Err(SummaryError("unhashable type: 'list'".into())),
        Some(Value::ArraySummary { .. }) => {
            return Err(SummaryError("unhashable type: 'dict'".into()))
        }
    })
}

/// `scan_chat_template_features`: reasoning-related capabilities read off the raw template.
pub fn scan_chat_template_features(template: Option<&Value>) -> Json {
    let t = match template {
        Some(Value::Str(s)) if !s.is_empty() => s.to_lowercase(),
        _ => return Json::Object(Vec::new()),
    };
    Json::object([
        (
            "accepts_enable_thinking",
            Json::Bool(t.contains("enable_thinking")),
        ),
        (
            "accepts_reasoning_effort",
            Json::Bool(t.contains("reasoning_effort")),
        ),
        (
            "accepts_preserve_thinking",
            Json::Bool(t.contains("preserve_thinking")),
        ),
        ("uses_think_tags", Json::Bool(t.contains("<think>"))),
        (
            "uses_channel_thought",
            Json::Bool(
                t.contains("channel>thought")
                    || t.contains("channel|>thought")
                    || t.contains("<|channel|>thought"),
            ),
        ),
    ])
}

/// `_finite`: a copy with every NaN/±Infinity float replaced by `None` (noevia#901).
fn finite(v: &Value) -> Value {
    match v {
        Value::Float(f) if !f.is_finite() => Value::None,
        Value::List(items) => Value::List(items.iter().map(finite).collect()),
        Value::ArraySummary { count, sample } => Value::ArraySummary {
            count: *count,
            sample: sample.iter().map(finite).collect(),
        },
        other => other.clone(),
    }
}

/// Build the summary gguf_meta.py's `summarize(raw)` returns.
pub fn summarize(raw: &Raw) -> Result<Json, SummaryError> {
    let finite_raw: Raw = raw.iter().map(|(k, v)| (k.clone(), finite(v))).collect();
    let raw = &finite_raw;
    let empty = Value::Str(String::new());
    let arch: &Value = match raw.get("general.architecture") {
        Some(v) if truthy(v) => v,
        _ => &empty,
    };
    let prefix = if truthy(arch) {
        Some(py_str(arch))
    } else {
        None
    };
    let a = |key: &str| -> Option<&Value> {
        prefix
            .as_ref()
            .and_then(|p| raw.get(format!("{p}.{key}").as_str()))
    };
    let g = |key: &str| -> Json { Json::from(raw.get(key)) };

    let vocab_count;
    let vocab_size = match raw.get("tokenizer.ggml.tokens") {
        Some(Value::ArraySummary { count, .. }) => {
            vocab_count = Value::Int(i128::from(*count));
            Some(&vocab_count)
        }
        _ => a("vocab_size"),
    };

    let general = Json::object([
        ("name", g("general.name")),
        ("description", g("general.description")),
        ("author", g("general.author")),
        ("license", g("general.license")),
        ("url", g("general.url")),
        ("quant", quant_name(raw.get("general.file_type"))?),
        ("quant_version", g("general.quantization_version")),
        ("params", fmt_params(raw.get("general.parameter_count"))?),
        ("params_raw", g("general.parameter_count")),
        ("gguf_version", g("_gguf_version")),
        ("tensor_count", g("_tensor_count")),
        ("kv_count", g("_kv_count")),
        ("header_error", g("_error")),
    ]);

    let si = |key: &str| -> Result<Json, SummaryError> { Ok(int_json(scalar_int(a(key))?)) };
    let kd = |key: &str| -> Result<Json, SummaryError> { Ok(int_json(kv_dim_int(a(key))?)) };
    let sf = |key: &str| -> Result<Json, SummaryError> { Ok(float_json(scalar_float(a(key))?)) };
    let model = Json::object([
        ("arch", Json::from(arch)),
        ("context_length", si("context_length")?),
        ("embedding_length", si("embedding_length")?),
        ("block_count", si("block_count")?),
        ("feed_forward_length", si("feed_forward_length")?),
        ("attention_head_count", si("attention.head_count")?),
        (
            "attention_head_count_kv",
            Json::from(a("attention.head_count_kv")),
        ),
        ("rope_freq_base", sf("rope.freq_base")?),
        ("rope_scaling_type", Json::from(a("rope.scaling.type"))),
        ("rope_scaling_factor", sf("rope.scaling.factor")?),
        (
            "rope_scaling_original_context",
            si("rope.scaling.original_context_length")?,
        ),
        ("vocab_size", int_json(scalar_int(vocab_size)?)),
        ("expert_count", si("expert_count")?),
        ("nextn_predict_layers", si("nextn_predict_layers")?),
        ("expert_used_count", si("expert_used_count")?),
        ("key_length", kd("attention.key_length")?),
        ("value_length", kd("attention.value_length")?),
        ("full_attention_interval", si("full_attention_interval")?),
        ("sliding_window", si("attention.sliding_window")?),
        ("key_length_swa", kd("attention.key_length_swa")?),
        ("value_length_swa", kd("attention.value_length_swa")?),
        ("shared_kv_layers", si("attention.shared_kv_layers")?),
        (
            "sliding_window_pattern",
            Json::from(a("attention.sliding_window_pattern")),
        ),
        ("ssm_state_size", si("ssm.state_size")?),
        ("ssm_inner_size", si("ssm.inner_size")?),
        ("ssm_conv_kernel", si("ssm.conv_kernel")?),
        ("ssm_group_count", si("ssm.group_count")?),
    ]);

    let tokenizer = Json::object([
        ("model", g("tokenizer.ggml.model")),
        ("pre", g("tokenizer.ggml.pre")),
        ("bos_token_id", g("tokenizer.ggml.bos_token_id")),
        ("eos_token_id", g("tokenizer.ggml.eos_token_id")),
        ("unknown_token_id", g("tokenizer.ggml.unknown_token_id")),
        ("padding_token_id", g("tokenizer.ggml.padding_token_id")),
        ("add_bos_token", g("tokenizer.ggml.add_bos_token")),
        ("add_eos_token", g("tokenizer.ggml.add_eos_token")),
    ]);

    Ok(Json::object([
        ("arch", Json::from(arch)),
        ("general", general),
        ("model", model),
        ("tokenizer", tokenizer),
        ("chat_template", g("tokenizer.chat_template")),
        (
            "chat_template_features",
            scan_chat_template_features(raw.get("tokenizer.chat_template")),
        ),
    ]))
}
