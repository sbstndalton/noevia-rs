//! `autoconfig.analyze(...)` as the route calls it, and `api.budget_backends`: the same steps in
//! the same order, each autoconfig rule answered by the `model-autoconfig` crate (the Rust port
//! `MODEL_AUTOCONFIG=rust` confirms Python with), the files it reads (the projector and the draft
//! head beside the model) read through the contained models-folder reader.
//!
//! Values are kept as the JSON the Python dicts would hold (`model_files::json::Value`), so each
//! request to a rule is built exactly as `analyze()` builds it.
use crate::draft_heads::{find_mtp, rule_listing, stat};
use crate::sections::{access::Root, ini};
use model_autoconfig::{files::Answer, prep::Prep, Error as RuleError, Work};
use model_files::json::{Limits, Value};
use std::path::Path;

/// Keep the reference exception class for callers (search) that catch Python exceptions.
#[derive(Debug)]
pub(crate) enum AnalysisError {
    Failed,
    Refused,
    Python(&'static str),
}
use AnalysisError as Stop;
impl From<crate::model_updates::Stop> for AnalysisError {
    fn from(stop: crate::model_updates::Stop) -> Self {
        match stop {
            crate::model_updates::Stop::Failed => Self::Failed,
            crate::model_updates::Stop::Refused => Self::Refused,
        }
    }
}
impl From<AnalysisError> for crate::model_updates::Stop {
    fn from(error: AnalysisError) -> Self {
        match error {
            AnalysisError::Refused => Self::Refused,
            AnalysisError::Failed | AnalysisError::Python(_) => Self::Failed,
        }
    }
}

/// `_PRESET_KEYS`.
const PRESET_KEYS: [&str; 3] = ["fast", "balanced", "long-ctx"];
/// `SPEC_SECTION_KEYS`.
const SPEC_SECTION_KEYS: [&str; 6] = [
    "spec-type",
    "spec-draft-model",
    "spec-draft-ngl",
    "spec-draft-n-max",
    "spec-draft-n-min",
    "spec-draft-p-min",
];
/// `SPEC_PROFILES` as `_plain` writes them (a fixed list; the differential checks it).
const SPEC_PROFILES: &str = r#"[{"key":"off","label":"Off","icon":"x","blurb":"No speculation. Always correct, never slower than the model itself.","spec_type":"","needs_head":false,"knobs":{}},{"key":"balanced","label":"Balanced","icon":"gauge","blurb":"Mixed use. Barely above llama.cpp's own defaults, with a mild confidence gate.","spec_type":"draft-mtp","needs_head":true,"knobs":{"spec-draft-n-max":"4","spec-draft-p-min":"0.25"}},{"key":"coding","label":"Coding","icon":"terminal","blurb":"Deep drafts, loose gate. Code repeats itself, so guesses land and guessing further pays.","spec_type":"draft-mtp,ngram-simple","needs_head":true,"knobs":{"spec-draft-n-max":"8","spec-draft-n-min":"1","spec-draft-p-min":"0.05"}},{"key":"writing","label":"Creative writing","icon":"pencil","blurb":"Shallow drafts, tight gate. Prose is unpredictable, so bad bets cost more than good ones pay.","spec_type":"draft-mtp","needs_head":true,"knobs":{"spec-draft-n-max":"2","spec-draft-p-min":"0.60"}},{"key":"ngram","label":"N-gram only","icon":"hash","blurb":"No draft head required. Replays literal repeats from the prompt: strong on edits and refactors, inert on new prose.","spec_type":"ngram-simple","needs_head":false,"knobs":{}}]"#;

/// `models_dir`, when `analyze()` reads the files beside the model (`models_dir is not None and
/// bool(section_name)`).
pub(crate) struct Files<'a> {
    /// `str(settings.models_dir)`.
    pub models_text: &'a str,
    pub root: &'a Root,
    pub base: &'a Path,
}

/// What a caller hands `analyze()`.
pub(crate) struct Inputs<'a> {
    pub name: &'a str,
    /// The GGUF summary as the dict Python holds.
    pub summary: &'a Value,
    pub file_size: u128,
    /// `budget_backends(_backend_list(), budget_gib)`.
    pub backends: Value,
    pub model_rel: &'a str,
    pub current: Option<ini::Items>,
    pub preset: &'a str,
    pub n_sessions: i64,
    pub model_subdir: &'a str,
    pub spec_profile: &'a str,
    pub vision: bool,
    pub prompt_tps: f64,
    pub prompt_budget_s: f64,
    pub verified_ctx: i64,
    pub mode: &'a str,
    /// `settings.cache_ram_limits[0]`.
    pub cache_ram_cap_mib: i64,
    /// `None` is `models_dir=None` (the search estimates): no file beside the model is read.
    pub files: Option<Files<'a>>,
    /// `mmproj_gb_override`.
    pub mmproj_gb_override: Option<f64>,
}

pub(super) fn s(text: &str) -> Value {
    Value::Str(text.to_owned())
}

pub(crate) fn int(n: i128) -> Value {
    Value::Int(
        model_files::BigInt::from_ascii_decimal(&n.to_string())
            .unwrap_or_else(model_files::BigInt::zero),
    )
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// A rule's refusal: Python's own exception is its 500; anything this port does not reproduce
/// (a cap, a shape, a value) is the native refusal.
fn rule(e: RuleError) -> Stop {
    match e {
        RuleError::Python(class) => Stop::Python(class),
        _ => Stop::Refused,
    }
}

/// A rule's JSON answer read back as the dict Python holds.
fn back(text: &str) -> Result<Value, Stop> {
    let limits = Limits {
        string_chars: 1 << 20,
        ..Limits::default()
    };
    model_files::json::parse_with_limits(text, limits).map_err(|_| Stop::Refused)
}

fn field<'v>(v: &'v Value, key: &str) -> Result<&'v Value, Stop> {
    v.get(key).ok_or(Stop::Refused)
}

/// `gguf_meta`'s summary value as the JSON Python holds.
pub(super) fn from_summary(v: &gguf::Json) -> Value {
    match v {
        gguf::Json::Null => Value::Null,
        gguf::Json::Bool(b) => Value::Bool(*b),
        gguf::Json::Int(i) => Value::Int(
            model_files::BigInt::from_ascii_decimal(&i.to_string())
                .unwrap_or_else(model_files::BigInt::zero),
        ),
        gguf::Json::Float(f) => Value::Float(*f),
        gguf::Json::Str(text) => Value::Str(text.clone()),
        gguf::Json::Array(items) => Value::Arr(items.iter().map(from_summary).collect()),
        gguf::Json::Object(pairs) => Value::Obj(
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), from_summary(v)))
                .collect(),
        ),
    }
}

/// Python truthiness of a summary value.
pub(super) fn truthy(v: Option<&gguf::Json>) -> bool {
    match v {
        None | Some(gguf::Json::Null) => false,
        Some(gguf::Json::Bool(b)) => *b,
        Some(gguf::Json::Int(n)) => n.to_string() != "0",
        Some(gguf::Json::Float(f)) => *f != 0.0,
        Some(gguf::Json::Str(text)) => !text.is_empty(),
        Some(gguf::Json::Array(items)) => !items.is_empty(),
        Some(gguf::Json::Object(pairs)) => !pairs.is_empty(),
    }
}

/// `min(a, b)` and `max(a, b)` as Python's builtins pick between two floats.
fn py_min(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}
fn py_max(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// `float(x or 0)` for the JSON values the backend list holds.
fn float_or_zero(v: Option<&Value>) -> Result<f64, Stop> {
    match v {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        Some(Value::Float(f)) => Ok(*f),
        Some(Value::Int(i)) => {
            let n: i64 = i.to_string().parse().map_err(|_| Stop::Refused)?;
            if n.unsigned_abs() > 1 << 53 {
                return Err(Stop::Refused);
            }
            Ok(n as f64)
        }
        // A string would be float()'s own parse: not reproduced.
        Some(_) => Err(Stop::Refused),
    }
}

/// `api.budget_backends(backends, budget_gib)` (#697): every backend's VRAM held to the budget
/// less the largest prompt cache, the cards in proportion. `cap_mib` is the cache cap.
pub(super) fn budget_backends(
    backends: Value,
    budget_gib: f64,
    cap_mib: i64,
) -> Result<Value, Stop> {
    let budget = budget_gib;
    if budget.is_nan() || budget <= 0.0 {
        return Ok(backends);
    }
    let Value::Arr(items) = backends else {
        return Err(Stop::Refused);
    };
    // `cache_ram_limits[0] / 1024` (a small int divided exactly), then max(0.0, ...).
    let room = py_max(0.0, budget - (cap_mib as f64) / 1024.0);
    let mut out = Vec::with_capacity(items.len());
    for b in items {
        let Value::Obj(mut pairs) = b else {
            return Err(Stop::Failed); // dict(b) of a non-mapping raises
        };
        let vram = py_min(float_or_zero(lookup(&pairs, "vram_gb"))?, room);
        set(&mut pairs, "vram_gb", Value::Float(vram));
        let cards = match lookup(&pairs, "card_vram_gb") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Arr(cards)) => cards.clone(),
            Some(v) if !v.truthy() => Vec::new(),
            Some(_) => return Err(Stop::Refused),
        };
        if !cards.is_empty() {
            let mut floats = Vec::with_capacity(cards.len());
            for c in &cards {
                floats.push(float_or_zero(Some(c))?);
            }
            // sum() from int 0, left to right; `or 1.0` when it is zero.
            let mut total = 0.0;
            for c in &floats {
                total += c;
            }
            if total == 0.0 {
                total = 1.0;
            }
            let held = floats
                .iter()
                .map(|c| Value::Float(py_min(*c, room * c / total)))
                .collect();
            set(&mut pairs, "card_vram_gb", Value::Arr(held));
        }
        out.push(Value::Obj(pairs));
    }
    Ok(Value::Arr(out))
}

fn lookup<'v>(pairs: &'v [(String, Value)], key: &str) -> Option<&'v Value> {
    pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// `d[key] = value` on a dict read from JSON: the first place of the key keeps it.
fn set(pairs: &mut Vec<(String, Value)>, key: &str, value: Value) {
    let mut seen = false;
    pairs.retain_mut(|(k, v)| {
        if k != key {
            return true;
        }
        if seen {
            return false;
        }
        seen = true;
        *v = value.clone();
        true
    });
    if !seen {
        pairs.push((key.to_owned(), value));
    }
}

/// `Path(str(p).replace("/models", str(models_dir), 1)).stat().st_size / 1024**3`, 0.0 when it
/// does not exist. Only a path inside the models folder is read; any other refuses.
fn stat_gb(inputs: &Files<'_>, path: &str) -> Result<f64, Stop> {
    if path.contains('\0') {
        return Err(Stop::Failed); // pathlib raises ValueError: unhandled
    }
    let mapped = path.replacen("/models", inputs.models_text, 1);
    let rel = if mapped == inputs.models_text {
        ""
    } else if inputs.models_text == "/" {
        mapped.strip_prefix('/').ok_or(Stop::Refused)?
    } else {
        mapped
            .strip_prefix(inputs.models_text)
            .and_then(|r| r.strip_prefix('/'))
            .ok_or(Stop::Refused)?
    };
    match stat(inputs.root, inputs.base, rel)? {
        // An int divided exactly by a power of two: one rounding either way.
        Some(meta) => Ok(meta.len() as f64 / 1_073_741_824.0),
        None => Ok(0.0),
    }
}

fn pick(call: Value) -> Result<Answer, Stop> {
    let mut work = Work::new(model_autoconfig::MAX_PREP_WORK);
    model_autoconfig::files::pick(&call, &mut work).map_err(rule)
}

/// `autoconfig._find_mmproj(models_dir, section, subdir)`.
fn find_mmproj(files: &Files<'_>, inputs: &Inputs<'_>) -> Result<String, Stop> {
    let call = if inputs.model_subdir.is_empty() {
        obj(vec![
            ("rule", s("mmproj_flat")),
            ("listing", rule_listing(files.root, files.base, ".")?),
            ("section", s(inputs.name)),
        ])
    } else {
        obj(vec![
            ("rule", s("mmproj_subdir")),
            (
                "listing",
                rule_listing(files.root, files.base, inputs.model_subdir)?,
            ),
            ("subdir", s(inputs.model_subdir)),
        ])
    };
    match pick(call)? {
        Answer::Path(path) => Ok(path),
        Answer::Projector { .. } => Err(Stop::Refused),
    }
}

/// `f"{n:,}"` for an int.
fn commas(n: i128) -> String {
    let raw = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in raw.chars().enumerate() {
        if i > 0 && (raw.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// `cap_reason_text(kind, ...)`.
fn cap_reason(kind: &str, prompt_tps: f64, prompt_budget_s: f64, verified_ctx: i64) -> String {
    match kind {
        "verified" => format!(
            "verified on this machine at {} tokens",
            commas(i128::from(verified_ctx))
        ),
        "time" => format!(
            "a full prompt must finish in {} s at the measured {prompt_tps:.0} tokens/s",
            prompt_budget_s.trunc() as i64
        ),
        "unmeasured" => "prompt speed not measured yet; measure context to go higher".to_owned(),
        _ => String::new(),
    }
}

/// `Recommendation(...)` as `_plain` writes it, in the dataclass's field order, from the fields
/// given (the rest at their defaults).
fn recommendation(given: Vec<(&str, Value)>) -> Value {
    let defaults: [(&str, Value); 29] = [
        ("plans", Value::Arr(Vec::new())),
        ("recommended_backend", s("")),
        ("recommended_ctx", int(0)),
        ("recommended_total_ctx", int(0)),
        ("n_sessions", int(1)),
        ("values", Value::Obj(Vec::new())),
        ("values_minimal", Value::Obj(Vec::new())),
        ("baseline_redundant", Value::Obj(Vec::new())),
        ("quirks", Value::Arr(Vec::new())),
        ("unavailable", Value::Arr(Vec::new())),
        ("current_diff", Value::Arr(Vec::new())),
        ("displaced", Value::Arr(Vec::new())),
        ("presets", Value::Arr(Vec::new())),
        ("frontier", Value::Arr(Vec::new())),
        ("fits_full_gpu", Value::Bool(false)),
        ("native_ctx", int(0)),
        ("estimated_ctx", int(0)),
        ("ctx_cap_reason", s("")),
        ("warnings", Value::Arr(Vec::new())),
        ("current_preset", s("")),
        ("has_unsaved", Value::Bool(false)),
        ("active_preset", s("")),
        ("spec_profiles", Value::Arr(Vec::new())),
        ("active_spec_profile", s("")),
        ("current_spec_profile", s("")),
        ("spec_head_rel", s("")),
        ("error", s("")),
        ("vision_available", s("")),
        ("vision", Value::Bool(true)),
    ];
    let mut given = given;
    Value::Obj(
        defaults
            .into_iter()
            .map(|(k, default)| {
                let value = given
                    .iter()
                    .position(|(g, _)| *g == k)
                    .map(|i| given.swap_remove(i).1)
                    .unwrap_or(default);
                (k.to_owned(), value)
            })
            .collect(),
    )
}

/// `f"{x:.0f}"`.
fn f0(x: f64) -> String {
    format!("{x:.0}")
}

/// `autoconfig._prep_refusal(prep)`: the recommendation for an early refusal.
fn refusal(prep: &Prep) -> Result<Value, Stop> {
    let error = match prep {
        Prep::BlockCount(layers) => format!(
            "This model's metadata declares an implausible block_count ({layers}; real models have \
             well under {}). The file is corrupt or hostile, so no context size is recommended.",
            model_autoconfig::MAX_BLOCK_COUNT
        ),
        Prep::NoBackends => "No GPU backend available to size against. Either no llama.cpp container \
             was discovered, or its VRAM probe returned 0 (CPU-only build, or nvidia-smi / rocm-smi \
             not usable inside the container). Check the Containers page: a backend must appear \
             there with a non-zero VRAM total before Autoconfig can plan. This is not a statement \
             about whether the model would fit."
            .to_owned(),
        Prep::Ram {
            model_gb,
            vram_gb,
            ram_gb,
        } => format!(
            "This model needs about {} GB, more than this machine's {} GB VRAM plus {} GB RAM \
             ({} GB total). CPU offload moves layers into system memory, so there is no offload \
             setting that makes it fit. A smaller quantisation of the same model is the option.",
            f0(*model_gb),
            f0(*vram_gb),
            f0(*ram_gb),
            f0(vram_gb + ram_gb)
        ),
        Prep::UnsizedVram(names) => format!(
            "Backend(s) found ({}) but none report their VRAM, so there is nothing to size \
             against. This is usually a Vulkan build: the image ships no vendor SMI tool, so VRAM \
             cannot be probed. Declare it instead — set GPU_VRAM=<container>:<GB> on the \
             model-loader service (e.g. GPU_VRAM=llama-vulkan:16) and restart it. Nothing here \
             says the model does not fit; the size of the card is simply unknown.",
            names.join(", ")
        ),
        Prep::KvLayers {
            known,
            count,
            layers,
            shared,
        } => {
            let where_ = if *shared != 0 {
                format!(
                    "it also declares {shared} layers sharing KV, which cannot be matched to its \
                     attention layers"
                )
            } else if *count == layers.to_string() {
                format!("only the first {known} of its {count} entries are readable")
            } else {
                format!("it has {count} entries for {layers} layers")
            };
            format!(
                "Cannot size the KV cache from this model's metadata: it is a hybrid model whose \
                 attention_head_count_kv marks non-attention layers with 0, and {where_}, so its \
                 KV cache cannot be sized safely. Autoconfig will not guess a context size. Set \
                 ctx-size manually in the form and verify it loads."
            )
        }
        Prep::Kv(missing) => {
            let mut text = "Cannot size the KV cache from this model's metadata".to_owned();
            if !missing.is_empty() {
                text.push_str(" — missing/zero: ");
                text.push_str(&missing.join(", "));
            }
            text.push_str(
                ". Autoconfig will not guess a context size, because an unsized KV cache would \
                 look free and produce a recommendation that OOMs on load. Set ctx-size manually \
                 in the form and verify it loads.",
            );
            text
        }
        Prep::Ok(_) => return Err(Stop::Refused),
    };
    Ok(recommendation(vec![("error", Value::Str(error))]))
}

fn prepare(prep_in: &Value) -> Result<Prep, Stop> {
    let mut work = Work::new(model_autoconfig::MAX_PREP_WORK);
    model_autoconfig::prep::prepare(prep_in, &mut work).map_err(rule)
}

fn with(base: &Value, key: &str, value: Value) -> Value {
    let mut pairs = match base {
        Value::Obj(p) => p.clone(),
        _ => Vec::new(),
    };
    set(&mut pairs, key, value);
    Value::Obj(pairs)
}

/// `{k: section[k] for k in keys if k in section}`.
fn subset(current: &ini::Items, keys: &[&str]) -> Value {
    Value::Obj(
        keys.iter()
            .filter_map(|k| {
                current
                    .iter()
                    .rev()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| ((*k).to_owned(), s(v)))
            })
            .collect(),
    )
}

fn pairs_value(values: &[(String, String)]) -> Value {
    Value::Arr(
        values
            .iter()
            .map(|(k, v)| Value::Arr(vec![s(k), s(v)]))
            .collect(),
    )
}

/// `dict(pairs)` written as an object.
fn dict(values: &[(String, String)]) -> Value {
    let mut out: Vec<(String, Value)> = Vec::new();
    for (k, v) in values {
        set(&mut out, k, s(v));
    }
    Value::Obj(out)
}

fn strings(items: &[String]) -> Value {
    Value::Arr(items.iter().map(|t| s(t)).collect())
}

/// `PresetOption(backend=name, **p)` as `_plain` writes it.
fn preset(p: &Value, backend: &Value) -> Result<Value, Stop> {
    let mut out = Vec::new();
    for key in ["key", "label", "icon"] {
        out.push((key.to_owned(), field(p, key)?.clone()));
    }
    out.push(("backend".to_owned(), backend.clone()));
    for key in [
        "ctx",
        "n_cpu_moe",
        "offload_kind",
        "gpu_layers",
        "total_layers",
        "gpu_gb",
        "kv_gb",
        "speed_score",
        "ngl",
    ] {
        out.push((key.to_owned(), field(p, key)?.clone()));
    }
    Ok(Value::Obj(out))
}

fn as_index(v: &Value) -> Result<Option<usize>, Stop> {
    match v {
        Value::Null => Ok(None),
        Value::Int(i) => i.to_string().parse().map(Some).map_err(|_| Stop::Refused),
        _ => Err(Stop::Refused),
    }
}

fn items_obj(current: &ini::Items) -> Value {
    let mut out = Vec::new();
    for (k, v) in current {
        set(&mut out, k, s(v));
    }
    Value::Obj(out)
}

/// Existing route contract: reference exceptions still become its internal-error outcome.
pub(crate) fn analyze(inputs: &Inputs<'_>) -> Result<Value, crate::model_updates::Stop> {
    analyze_typed(inputs).map_err(Into::into)
}

/// `autoconfig.analyze(...)` for the route: the recommendation as `_plain` writes it.
#[allow(clippy::too_many_lines)]
pub(crate) fn analyze_typed(inputs: &Inputs<'_>) -> Result<Value, Stop> {
    let n_sessions = inputs.n_sessions.clamp(1, 8);
    let summary = inputs.summary;
    let model = summary.get("model").cloned().unwrap_or(Value::Null);
    let arch = summary.get("arch").cloned().unwrap_or(Value::Null);
    let file_size = model_files::BigInt::from_ascii_decimal(&inputs.file_size.to_string())
        .ok_or(Stop::Refused)?;
    let prep_in = obj(vec![
        ("n_sessions", int(i128::from(n_sessions))),
        ("arch", arch.clone()),
        ("model", model.clone()),
        ("file_size", Value::Int(file_size)),
        ("backends", inputs.backends.clone()),
    ]);
    let zero = obj(vec![
        ("has_mmproj", Value::Bool(false)),
        ("mmproj_gb", Value::Float(0.0)),
        ("mtp_gb", Value::Float(0.0)),
    ]);
    let first = prepare(&with(&prep_in, "projector", zero))?;
    if !matches!(first, Prep::Ok(_)) {
        return refusal(&first);
    }
    let current = inputs.current.as_ref();
    let get = |key: &str| -> Option<&str> {
        current.and_then(|c| {
            c.iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        })
    };
    // The projector that will actually be loaded: the saved one, else the one beside the model.
    let files = inputs.files.as_ref();
    let mm_saved = match files {
        Some(_) => get("mmproj").unwrap_or_default().to_owned(),
        None => String::new(),
    };
    let mut mm_found = String::new();
    let mut mm_gb = 0.0;
    if let Some(files) = files {
        let mm_path = if ini::strip(&mm_saved).is_empty() {
            mm_found = find_mmproj(files, inputs)?;
            mm_found.clone()
        } else {
            ini::strip(&mm_saved).to_owned()
        };
        if !mm_path.is_empty() {
            mm_gb = stat_gb(files, &mm_path)?;
        }
    }
    let Answer::Projector {
        mmproj_rel,
        mmproj_gb,
        available,
    } = pick(obj(vec![
        ("rule", s("projector")),
        ("files", Value::Bool(files.is_some())),
        ("current", s(&mm_saved)),
        ("found", s(&mm_found)),
        ("stat_gb", Value::Float(mm_gb)),
        ("vision", Value::Bool(inputs.vision)),
        (
            "override",
            inputs.mmproj_gb_override.map_or(Value::Null, Value::Float),
        ),
    ]))?
    else {
        return Err(Stop::Refused);
    };
    // The draft head: the saved one, else the smallest beside the model.
    let mut mtp_found = String::new();
    let mut mtp_gb = 0.0;
    if let Some(files) = files {
        let saved_head = ini::strip(get("spec-draft-model").unwrap_or_default()).to_owned();
        let mtp_rel = if saved_head.is_empty() {
            mtp_found = find_mtp(files.root, files.base, inputs.name, inputs.model_subdir)?;
            mtp_found.clone()
        } else {
            saved_head
        };
        if !mtp_rel.is_empty() {
            mtp_gb = stat_gb(files, &mtp_rel)?;
        }
    }
    let has_mmproj = !mmproj_rel.is_empty();
    let projector = obj(vec![
        ("has_mmproj", Value::Bool(has_mmproj)),
        ("mmproj_gb", mmproj_gb.clone()),
        ("mtp_gb", Value::Float(mtp_gb)),
    ]);
    let prepared = prepare(&with(&prep_in, "projector", projector))?;
    if !matches!(prepared, Prep::Ok(_)) {
        return Err(Stop::Refused);
    }
    let prep = back(&model_autoconfig::prep::prep_json(&prepared))?;
    let sized_backends = sized(&inputs.backends, &prep)?;
    let size_backends = field(&prep, "backends")?.clone();
    let mmproj_vram_gb = field(&prep, "mmproj_vram_gb")?.clone();
    let preset_key = if PRESET_KEYS.contains(&inputs.preset) {
        inputs.preset
    } else {
        ""
    };
    let size_req = obj(vec![
        ("shape", field(&prep, "shape")?.clone()),
        ("layers", field(&prep, "layers")?.clone()),
        ("native_ctx", field(&prep, "native_ctx")?.clone()),
        ("model_gb_raw", field(&prep, "model_gb_raw")?.clone()),
        ("moe_ratio", field(&prep, "moe_ratio")?.clone()),
        ("is_moe", field(&prep, "is_moe")?.clone()),
        ("mmproj_vram_gb", mmproj_vram_gb.clone()),
        ("n_sessions", int(i128::from(n_sessions))),
        ("backends", size_backends.clone()),
        ("preset", s(preset_key)),
        ("prompt_tps", Value::Float(inputs.prompt_tps)),
        ("prompt_budget_s", Value::Float(inputs.prompt_budget_s)),
        ("verified_ctx", int(i128::from(inputs.verified_ctx))),
        (
            "cache_ram_cap_mib",
            int(i128::from(inputs.cache_ram_cap_mib)),
        ),
    ]);
    let request = model_autoconfig::parse_value(&size_req).map_err(rule)?;
    let plan = back(&model_autoconfig::plan_json(
        &model_autoconfig::size_plan(&request).map_err(rule)?,
    ))?;
    let state = Planned {
        prep,
        plan,
        model,
        arch,
        sized_backends,
        size_backends,
        mmproj_vram_gb,
        mmproj_rel,
        mmproj_gb,
        available,
        mtp_found,
        has_mmproj,
        n_sessions,
    };
    finish(inputs, state)
}

/// `[backends[i] for i in prep["sized"]]`.
fn sized(backends: &Value, prep: &Value) -> Result<Vec<Value>, Stop> {
    let Value::Arr(all) = backends else {
        return Err(Stop::Refused);
    };
    let Value::Arr(indexes) = field(prep, "sized")? else {
        return Err(Stop::Refused);
    };
    let mut out = Vec::new();
    for i in indexes {
        let i = as_index(i)?.ok_or(Stop::Refused)?;
        out.push(all.get(i).ok_or(Stop::Refused)?.clone());
    }
    Ok(out)
}

/// What `analyze()` holds once the size plan is made.
struct Planned {
    prep: Value,
    plan: Value,
    model: Value,
    arch: Value,
    sized_backends: Vec<Value>,
    size_backends: Value,
    mmproj_vram_gb: Value,
    mmproj_rel: String,
    mmproj_gb: Value,
    available: String,
    mtp_found: String,
    has_mmproj: bool,
    n_sessions: i64,
}

/// The plans as `BackendPlan`s, and the recommended backend's name.
fn backend_plans(st: &Planned) -> Result<(Vec<Value>, Option<Value>), Stop> {
    let Value::Arr(rows) = field(&st.plan, "plans")? else {
        return Err(Stop::Refused);
    };
    let mut plans = Vec::new();
    for (b, p) in st.sized_backends.iter().zip(rows) {
        let name = b.get("name").ok_or(Stop::Failed)?.clone(); // b["name"]
        let vendor = b.get("vendor").cloned().unwrap_or_else(|| s(""));
        let vram = match b.get("vram_gb") {
            Some(Value::Float(f)) => *f,
            Some(other) => float_or_zero(Some(other))?,
            None => return Err(Stop::Failed),
        };
        let max_ctx = field(p, "max_ctx")?.clone();
        let fits =
            !matches!(&max_ctx, Value::Int(i) if i.is_zero() || i.to_string().starts_with('-'));
        plans.push(obj(vec![
            ("name", name),
            ("vendor", vendor),
            ("vram_gb", Value::Float(vram)),
            ("rows", field(p, "rows")?.clone()),
            ("max_ctx", max_ctx),
            ("fits_at_all", Value::Bool(fits)),
        ]));
    }
    let rec_name = match as_index(field(&st.plan, "recommended")?)? {
        Some(i) => Some(field(plans.get(i).ok_or(Stop::Refused)?, "name")?.clone()),
        None => None,
    };
    Ok((plans, rec_name))
}

fn int_of(v: &Value) -> Result<i128, Stop> {
    match v {
        Value::Int(i) => i.to_string().parse().map_err(|_| Stop::Refused),
        _ => Err(Stop::Refused),
    }
}

/// The speculative-decoding answer (`resolve_spec`) and its values as a dict holds them.
fn spec(
    inputs: &Inputs<'_>,
    st: &Planned,
) -> Result<(model_autoconfig::spec::Resolved, Vec<(String, String)>), Stop> {
    let current = inputs.current.as_ref();
    let spec_in = obj(vec![
        ("section", s(inputs.name)),
        (
            "current",
            current.map_or(Value::Null, |c| subset(c, &SPEC_SECTION_KEYS)),
        ),
        ("spec_profile", s(inputs.spec_profile)),
        ("mode", s(inputs.mode)),
        ("files", Value::Bool(inputs.files.is_some())),
        ("found_mtp", s(&st.mtp_found)),
        (
            "nextn",
            st.model
                .get("nextn_predict_layers")
                .cloned()
                .unwrap_or(Value::Null),
        ),
    ]);
    let mut work = Work::new(model_autoconfig::MAX_PREP_WORK);
    let spec = model_autoconfig::spec::resolve(&spec_in, &mut work).map_err(rule)?;
    let mut values: Vec<(String, String)> = Vec::new();
    for (k, v) in &spec.values.0 {
        match values.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1.clone_from(v),
            None => values.push((k.clone(), v.clone())),
        }
    }
    Ok((spec, values))
}

/// `PresetOption`s of the plan list `key` for the recommended backend.
fn options(plan: &Value, key: &str, name: &Value) -> Result<Vec<Value>, Stop> {
    let Value::Arr(items) = field(plan, key)? else {
        return Err(Stop::Refused);
    };
    items.iter().map(|p| preset(p, name)).collect()
}

/// GPUs of the recommended backend (its first same-named one), or None.
fn rec_gpu_count(st: &Planned, recommended: Option<usize>) -> Result<Value, Stop> {
    let Some(i) = recommended else {
        return Ok(Value::Null);
    };
    let Value::Arr(b) = &st.size_backends else {
        return Err(Stop::Refused);
    };
    let same = as_index(field(b.get(i).ok_or(Stop::Refused)?, "same_as")?)?.ok_or(Stop::Refused)?;
    Ok(field(b.get(same).ok_or(Stop::Refused)?, "gpu_count")?.clone())
}

/// The rest of `analyze()`: the values, the report beside them, the recommendation.
fn finish(inputs: &Inputs<'_>, st: Planned) -> Result<Value, Stop> {
    let (plan, prep, summary) = (&st.plan, &st.prep, inputs.summary);
    let current = inputs.current.as_ref();
    let n = int(i128::from(st.n_sessions));
    let (plans, rec_name) = backend_plans(&st)?;
    let recommended = as_index(field(plan, "recommended")?)?;
    let Value::Str(kind) = field(plan, "cap")? else {
        return Err(Stop::Refused);
    };
    let reason = cap_reason(
        kind,
        inputs.prompt_tps,
        inputs.prompt_budget_s,
        inputs.verified_ctx,
    );
    let rec_backend = rec_name
        .as_ref()
        .and_then(|n| st.sized_backends.iter().find(|b| b.get("name") == Some(n)));
    let (spec, spec_values) = spec(inputs, &st)?;
    let (presets, frontier) = match &rec_name {
        Some(name) => (
            options(plan, "presets", name)?,
            options(plan, "frontier", name)?,
        ),
        None => (Vec::new(), Vec::new()),
    };
    let sized = matches!(field(plan, "sized")?, Value::Bool(true));
    let rec_ctx = field(plan, if sized { "ctx" } else { "initial_ctx" })?.clone();
    let rec_ctx_n = int_of(&rec_ctx)?;
    let chat_template = summary.get("chat_template").is_some_and(Value::truthy);
    let features = summary
        .get("chat_template_features")
        .cloned()
        .unwrap_or(Value::Null);
    let plan_part = Value::Obj(
        ["initial_ctx", "sized", "ctx", "ngl", "fit", "cache_ram"]
            .iter()
            .map(|k| Ok(((*k).to_owned(), field(plan, k)?.clone())))
            .collect::<Result<_, Stop>>()?,
    );
    let native_ctx = field(prep, "native_ctx")?.clone();
    let values_in = obj(vec![
        ("model_rel", s(inputs.model_rel)),
        ("n_sessions", n.clone()),
        ("chat_template", Value::Bool(chat_template)),
        ("features", features.clone()),
        ("section", s(inputs.name)),
        ("vision", Value::Bool(inputs.vision)),
        (
            "current",
            current.map_or(Value::Obj(Vec::new()), |c| {
                subset(c, &["mmproj", "ubatch-size", "batch-size"])
            }),
        ),
        ("mmproj_rel", s(&st.mmproj_rel)),
        ("has_mmproj", Value::Bool(st.has_mmproj)),
        ("spec", pairs_value(&spec_values)),
        ("plan", plan_part),
        ("rope", st.model.clone()),
        ("native_ctx", native_ctx.clone()),
        ("rec_gpu_count", rec_gpu_count(&st, recommended)?),
    ]);
    let values = model_autoconfig::values::assemble(&values_in).map_err(rule)?;
    let presets_short = presets
        .iter()
        .map(|p| {
            Ok(obj(vec![
                ("key", field(p, "key")?.clone()),
                ("ctx", field(p, "ctx")?.clone()),
                ("offload_kind", field(p, "offload_kind")?.clone()),
                ("ngl", field(p, "ngl")?.clone()),
                ("n_cpu_moe", field(p, "n_cpu_moe")?.clone()),
            ]))
        })
        .collect::<Result<Vec<_>, Stop>>()?;
    let rec_backend = rec_backend.map_or(Value::Null, |b| {
        Value::Obj(
            ["baseline", "gpu_count"]
                .iter()
                .filter_map(|k| b.get(k).map(|v| ((*k).to_owned(), v.clone())))
                .collect(),
        )
    });
    let present_in = obj(vec![
        ("values", pairs_value(&values.0)),
        ("current", current.map_or(Value::Null, items_obj)),
        ("recommended", Value::Bool(recommended.is_some())),
        ("rec_name", rec_name.clone().unwrap_or(Value::Null)),
        ("rec_backend", rec_backend),
        ("arch", st.arch.clone()),
        ("chat_template", Value::Bool(chat_template)),
        ("features", features),
        ("n_sessions", n.clone()),
        ("rec_ctx", rec_ctx.clone()),
        ("has_mmproj", Value::Bool(st.has_mmproj)),
        ("mmproj_vram_gb", st.mmproj_vram_gb.clone()),
        ("mmproj_gb", st.mmproj_gb.clone()),
        ("rope", st.model.clone()),
        ("native_ctx", native_ctx.clone()),
        ("layers", field(prep, "layers")?.clone()),
        (
            "experts",
            st.model.get("expert_count").cloned().unwrap_or(Value::Null),
        ),
        ("offload", field(plan, "offload")?.clone()),
        ("presets", Value::Arr(presets_short)),
        ("model_rel", s(inputs.model_rel)),
        (
            "general",
            summary.get("general").cloned().unwrap_or(Value::Null),
        ),
    ]);
    let mut work = Work::new(model_autoconfig::MAX_PREP_WORK);
    let report = model_autoconfig::present::present(&present_in, &mut work).map_err(rule)?;
    let total = if rec_ctx_n > 0 {
        rec_ctx_n * i128::from(st.n_sessions)
    } else {
        0
    };
    Ok(recommendation(vec![
        ("plans", Value::Arr(plans)),
        ("displaced", strings(&report.displaced)),
        ("recommended_backend", rec_name.unwrap_or_else(|| s(""))),
        ("recommended_ctx", rec_ctx),
        ("warnings", strings(&report.warnings)),
        ("estimated_ctx", field(plan, "estimated_ctx")?.clone()),
        ("ctx_cap_reason", s(&reason)),
        ("recommended_total_ctx", int(total)),
        ("n_sessions", n),
        ("values", dict(&values.0)),
        ("values_minimal", dict(&report.minimal.0)),
        ("baseline_redundant", dict(&report.redundant.0)),
        ("quirks", strings(&report.quirks)),
        ("unavailable", strings(&report.unavailable)),
        ("current_diff", strings(&report.current_diff)),
        ("presets", Value::Arr(presets)),
        ("frontier", Value::Arr(frontier)),
        ("fits_full_gpu", field(plan, "fits_full_gpu")?.clone()),
        ("native_ctx", native_ctx),
        ("current_preset", s(&report.current_preset)),
        ("has_unsaved", Value::Bool(!report.current_diff.is_empty())),
        ("active_preset", field(plan, "active_preset")?.clone()),
        // `if section_name`: the search estimates name no section.
        (
            "spec_profiles",
            if inputs.name.is_empty() {
                Value::Arr(Vec::new())
            } else {
                back(SPEC_PROFILES)?
            },
        ),
        (
            "active_spec_profile",
            s(if inputs.name.is_empty() {
                ""
            } else {
                &spec.key
            }),
        ),
        ("current_spec_profile", s(spec.saved)),
        ("spec_head_rel", s(&spec.head)),
        ("vision_available", s(&st.available)),
        ("vision", Value::Bool(inputs.vision)),
    ]))
}
