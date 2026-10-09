//! Values assembly: autoconfig_core.py's `assemble_values` - the settings analyze() writes
//! (context, slots, offload, caches, templating and reasoning flags, the projector with its
//! batch/ubatch/image-token bounds, rope, split-mode), as an ordered list of string pairs.

use crate::pyfloat;
use crate::pyval::{float_repr, get, int_of, lower_starts_with, py_strip};
use crate::Error;
use model_files::json::Value;

const VALUES_KEYS: [&str; 14] = [
    "model_rel",
    "n_sessions",
    "chat_template",
    "features",
    "section",
    "vision",
    "current",
    "mmproj_rel",
    "has_mmproj",
    "spec",
    "plan",
    "rope",
    "native_ctx",
    "rec_gpu_count",
];
const PLAN_KEYS: [&str; 6] = ["initial_ctx", "sized", "ctx", "ngl", "fit", "cache_ram"];
/// Largest context taken as exact in the rope ratio (Python divides big ints exactly).
const MAX_EXACT: i128 = 1 << 53;
const IMAGE_MAX_TOKENS: i128 = 1024;
/// Most speculative-decoding entries accepted. Python writes at most 7 (spec-type, the draft
/// model and ngl, the profile knobs); the cap keeps Values::set's linear search bounded
/// (noevia#1153).
pub const MAX_SPEC: usize = 16;

/// A Python dict of strings: insertion order kept, an assignment to an existing key keeps
/// its place, `pop` removes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Values(pub Vec<(String, String)>);

impl Values {
    pub(crate) fn set(&mut self, k: &str, v: String) {
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some((_, slot)) => *slot = v,
            None => self.0.push((k.to_owned(), v)),
        }
    }

    pub(crate) fn pop(&mut self, k: &str) {
        self.0.retain(|(key, _)| key != k);
    }

    pub(crate) fn get(&self, k: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }
}

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value, Error> {
    v.get(key).ok_or(Error::Schema("values: missing field"))
}

fn int(v: &Value, what: &'static str) -> Result<i128, Error> {
    match v {
        Value::Int(_) => crate::pyval::big(v, what),
        _ => Err(Error::Schema(what)),
    }
}

fn opt_int(v: &Value, what: &'static str) -> Result<Option<i128>, Error> {
    match v {
        Value::Null => Ok(None),
        other => int(other, what).map(Some),
    }
}

fn boolean(v: &Value, what: &'static str) -> Result<bool, Error> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(Error::Schema(what)),
    }
}

fn string<'a>(v: &'a Value, what: &'static str) -> Result<&'a str, Error> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(Error::Schema(what)),
    }
}

/// `int(x or 0)` inside `try: ... except ValueError: 0`.
fn int_or_zero_lenient(v: Option<&Value>, what: &'static str) -> Result<i128, Error> {
    match v {
        Some(v) if v.truthy() => match int_of(v, what) {
            Err(Error::Python("ValueError")) => Ok(0),
            other => other,
        },
        _ => Ok(0),
    }
}

/// `rope_owned_by_gguf(model)`.
pub(crate) fn rope_owned(model: &Value) -> Result<bool, Error> {
    if !matches!(model, Value::Obj(_)) {
        return Err(Error::Python("AttributeError"));
    }
    // str(arch or "").lower() startswith "gemma3" (the only architecture listed).
    let arch = get(model, "arch");
    if arch.truthy() && lower_starts_with(arch, "gemma3") {
        return Ok(true);
    }
    // str(rtype or "").strip().lower() not in ("", "none"): str() of any truthy non-string is
    // non-empty and never "none"; no non-ASCII character lower-cases to "n", "o" or "e".
    let rtype = get(model, "rope_scaling_type");
    if rtype.truthy() {
        match rtype {
            Value::Str(s) => {
                let t = py_strip(s);
                if !t.is_empty() && !t.eq_ignore_ascii_case("none") {
                    return Ok(true);
                }
            }
            _ => return Ok(true),
        }
    }
    Ok(match get(model, "rope_scaling_factor") {
        Value::Bool(b) => *b,
        // An integer literal of any length: positive unless zero or signed.
        Value::Int(i) => !i.is_zero() && !i.to_string().starts_with('-'),
        Value::Float(x) => *x > 0.0,
        _ => false,
    })
}

/// `assemble_values` of a values request.
pub fn assemble(inp: &Value) -> Result<Values, Error> {
    let Value::Obj(pairs) = inp else {
        return Err(Error::Schema("values"));
    };
    if pairs
        .iter()
        .any(|(k, _)| !VALUES_KEYS.contains(&k.as_str()))
    {
        return Err(Error::Schema("values"));
    }
    let n = int(field(inp, "n_sessions")?, "n_sessions")?;
    if !(1..=8).contains(&n) {
        return Err(Error::OutOfRange("n_sessions"));
    }
    let current = field(inp, "current")?;
    let Value::Obj(cur_pairs) = current else {
        return Err(Error::Schema("current"));
    };
    if cur_pairs.iter().any(|(_, v)| !matches!(v, Value::Str(_))) {
        return Err(Error::Schema("current: not a string"));
    }
    let plan = field(inp, "plan")?;
    let Value::Obj(plan_pairs) = plan else {
        return Err(Error::Schema("plan"));
    };
    if plan_pairs
        .iter()
        .any(|(k, _)| !PLAN_KEYS.contains(&k.as_str()))
    {
        return Err(Error::Schema("plan"));
    }
    let mut rec_ctx = int(field(plan, "initial_ctx")?, "plan.initial_ctx")?;
    let mut values = Values::default();

    let model_rel = string(field(inp, "model_rel")?, "model_rel")?;
    if !model_rel.is_empty() {
        values.set("model", model_rel.to_owned());
    }
    if rec_ctx > 0 {
        values.set("ctx-size", (rec_ctx * n).to_string());
    }
    values.set("parallel", n.to_string());
    if n > 1 {
        values.set("cont-batching", "on".to_owned());
        values.set("context-shift", "on".to_owned());
        values.set("keep", "256".to_owned());
        values.set("batch-size", "4096".to_owned());
    }
    values.set("ngl", "999".to_owned());
    values.set("flash-attn", "on".to_owned());
    values.set("cache-type-k", "q8_0".to_owned());
    values.set("cache-type-v", "q8_0".to_owned());
    values.set("cache-reuse", "1".to_owned());
    if field(inp, "chat_template")?.truthy() {
        values.set("jinja", "true".to_owned());
    }

    if field(inp, "section")?.truthy() && field(inp, "vision")?.truthy() {
        let cur = match current.get("mmproj") {
            Some(Value::Str(s)) => py_strip(s),
            _ => "",
        };
        if !cur.is_empty() {
            values.set("mmproj", cur.to_owned());
        } else {
            let rel = string(field(inp, "mmproj_rel")?, "mmproj_rel")?;
            if !rel.is_empty() {
                values.set("mmproj", rel.to_owned());
            }
        }
    }

    match field(inp, "spec")? {
        Value::Arr(items) if items.len() > MAX_SPEC => return Err(Error::OutOfRange("spec")),
        Value::Arr(items) => {
            for item in items {
                match item {
                    Value::Arr(pair) => match pair.as_slice() {
                        [Value::Str(k), Value::Str(v)] => values.set(k, v.clone()),
                        _ => return Err(Error::Schema("spec")),
                    },
                    _ => return Err(Error::Schema("spec")),
                }
            }
        }
        _ => return Err(Error::Schema("spec")),
    }

    if boolean(field(plan, "sized")?, "plan.sized")? {
        rec_ctx = int(field(plan, "ctx")?, "plan.ctx")?;
        values.set("ctx-size", (rec_ctx * n).to_string());
        if let Some(ngl) = opt_int(field(plan, "ngl")?, "plan.ngl")? {
            values.set("ngl", ngl.to_string());
        }
        if boolean(field(plan, "fit")?, "plan.fit")? {
            values.set("fit", "on".to_owned());
            for k in ["ngl", "cpu-moe", "n-cpu-moe", "tensor-split"] {
                values.pop(k);
            }
        }
        if let Some(cache) = opt_int(field(plan, "cache_ram")?, "plan.cache_ram")? {
            values.set("cache-ram", cache.to_string());
        }
    }

    let features_v = field(inp, "features")?;
    let empty = Value::Obj(Vec::new());
    let features = if features_v.truthy() {
        features_v
    } else {
        &empty
    };
    if !matches!(features, Value::Obj(_)) {
        return Err(Error::Python("AttributeError"));
    }
    let feature = |k: &str| get(features, k).truthy();
    if feature("accepts_enable_thinking") {
        values.set("reasoning", "on".to_owned());
    }
    if feature("accepts_reasoning_effort") {
        values.set(
            "chat-template-kwargs",
            "{\"reasoning_effort\": \"medium\"}".to_owned(),
        );
    }
    if feature("uses_think_tags") || feature("uses_channel_thought") {
        values.set("reasoning-format", "deepseek".to_owned());
    }
    if feature("accepts_preserve_thinking") {
        values.set("reasoning-preserve", "on".to_owned());
    }

    if boolean(field(inp, "has_mmproj")?, "has_mmproj")? {
        values.set("mmproj-offload", "on".to_owned());
        values.set("image-max-tokens", IMAGE_MAX_TOKENS.to_string());
        let imt = IMAGE_MAX_TOKENS;
        let cur_ub = int_or_zero_lenient(current.get("ubatch-size"), "ubatch-size")?;
        values.set("ubatch-size", imt.max(cur_ub).to_string());
        let batch_value = values.get("batch-size").map(|s| Value::Str(s.to_owned()));
        let batch = match &batch_value {
            Some(v) if v.truthy() => Some(v),
            _ => current.get("batch-size"),
        };
        let cur_b = int_or_zero_lenient(batch, "batch-size")?;
        // Raise a set batch below the ubatch; an unset one is llama-server's 2048, which only
        // an image bound above 2048 would exceed.
        if (cur_b != 0 && cur_b < imt) || (cur_b == 0 && imt > 2048) {
            values.set("batch-size", imt.to_string());
        }
    }

    let native_ctx = int(field(inp, "native_ctx")?, "native_ctx")?;
    if !rope_owned(field(inp, "rope")?)? && rec_ctx > native_ctx && native_ctx > 0 {
        if rec_ctx > MAX_EXACT || native_ctx > MAX_EXACT {
            return Err(Error::OutOfRange("a context past 2^53"));
        }
        let ratio = pyfloat::f(rec_ctx) / pyfloat::f(native_ctx);
        values.set("rope-scaling", "linear".to_owned());
        values.set(
            "rope-scale",
            float_repr(pyfloat::round_nd(ratio, 1), "rope-scale")?,
        );
    }

    if let Some(g) = opt_int(field(inp, "rec_gpu_count")?, "rec_gpu_count")? {
        if g > 1 {
            values.set("split-mode", "layer".to_owned());
        }
    }
    Ok(values)
}

/// The values as the JSON list `check_reference` returns: [[key, value]...].
pub fn values_json(v: &Values) -> String {
    let mut out = String::from("[");
    for (i, (k, val)) in v.0.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        crate::json_str(&mut out, k);
        out.push(',');
        crate::json_str(&mut out, val);
        out.push(']');
    }
    out.push(']');
    out
}
