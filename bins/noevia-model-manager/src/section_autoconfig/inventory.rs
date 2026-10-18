//! noevia#1318 step 3b: the section autoconfig's backend inventory natively.
//!
//! `helpers._backend_list()` came from the child's `section-autoconfig-runtime` answer. While
//! this process samples the backends, the same list is built here: the native descriptors
//! (`fit::descriptors`, differential-tested against `_backend_list` without `baseline`), each with
//! `baseline` as `autoconfig.parse_baseline(cmd) if cmd else {}` (the `model-autoconfig`
//! crate's `parse_baseline` over [`KNOWN_KEYS`]).
//!
//! The child still answers the telemetry (and still lists the backends itself, so its own
//! failures stay Python's). A descriptor list this side cannot render, or a baseline the rule
//! refuses or on which Python raises, keeps the child's list for that answer, with one log line
//! per reason.
use crate::backend_stats::{fit::Descriptor, Num};
use model_autoconfig::Work;
use model_files::json::Value;

/// `sorted(ini.ALL_KNOWN_KEYS)` (the long options `parse_baseline` maps to ini keys); the fit
/// oracle records Python's set and the tests compare it.
pub const KNOWN_KEYS: &[&str] = &[
    "alias",
    "backend-sampling",
    "batch-size",
    "cache-ram",
    "cache-reuse",
    "cache-type-k",
    "cache-type-k-draft",
    "cache-type-v",
    "cache-type-v-draft",
    "chat-template",
    "chat-template-file",
    "chat-template-kwargs",
    "check-tensors",
    "checkpoint-min-step",
    "cont-batching",
    "context-shift",
    "control-vector",
    "control-vector-layer-range",
    "control-vector-scaled",
    "cpu-mask",
    "cpu-moe",
    "cpu-range",
    "cpu-strict",
    "ctx-checkpoints",
    "ctx-size",
    "device",
    "embd-normalize",
    "embedding",
    "fit",
    "fit-ctx",
    "fit-target",
    "flash-attn",
    "image-max-tokens",
    "image-min-tokens",
    "jinja",
    "keep",
    "kv-offload",
    "kv-unified",
    "load-mode",
    "lookup-cache-dynamic",
    "lookup-cache-static",
    "lora",
    "lora-scaled",
    "main-gpu",
    "mmproj",
    "mmproj-auto",
    "mmproj-offload",
    "mmproj-url",
    "model",
    "mtmd-batch-max-tokens",
    "n-cpu-moe",
    "ngl",
    "no-host",
    "numa",
    "op-offload",
    "override-kv",
    "override-tensor",
    "parallel",
    "poll",
    "pooling",
    "prio",
    "reasoning",
    "reasoning-budget",
    "reasoning-budget-message",
    "reasoning-format",
    "reasoning-preserve",
    "repack",
    "rerank",
    "rope-freq-base",
    "rope-freq-scale",
    "rope-scale",
    "rope-scaling",
    "spec-draft-cpu-moe",
    "spec-draft-device",
    "spec-draft-hf",
    "spec-draft-model",
    "spec-draft-n-cpu-moe",
    "spec-draft-n-max",
    "spec-draft-n-min",
    "spec-draft-ngl",
    "spec-draft-p-min",
    "spec-draft-p-split",
    "spec-type",
    "special",
    "split-mode",
    "swa-full",
    "tags",
    "tensor-split",
    "threads",
    "threads-batch",
    "ubatch-size",
    "verbosity",
    "warmup",
    "yarn-attn-factor",
    "yarn-beta-fast",
    "yarn-beta-slow",
    "yarn-ext-factor",
    "yarn-orig-ctx",
];

fn s(text: &str) -> Value {
    Value::Str(text.to_owned())
}

/// `parse_baseline(args)` as the dict Python holds; `None` when the rule refuses or raises.
fn baseline(args: &[String]) -> Option<Value> {
    if args.is_empty() {
        return Some(Value::Obj(Vec::new()));
    }
    let request = Value::Obj(vec![
        (
            "args".to_owned(),
            Value::Arr(args.iter().map(|a| s(a)).collect()),
        ),
        (
            "known".to_owned(),
            Value::Arr(KNOWN_KEYS.iter().map(|k| s(k)).collect()),
        ),
    ]);
    let mut work = Work::new(model_autoconfig::MAX_PREP_WORK);
    let values = model_autoconfig::baseline::parse(&request, &mut work).ok()?;
    Some(Value::Obj(
        values
            .0
            .into_iter()
            .map(|(k, v)| (k, Value::Str(v)))
            .collect(),
    ))
}

/// `helpers._backend_list()` of the native descriptors.
pub(crate) fn backend_list(descriptors: &[Descriptor]) -> Option<Value> {
    let mut out = Vec::with_capacity(descriptors.len());
    for d in descriptors {
        out.push(Value::Obj(vec![
            ("name".to_owned(), s(&d.name)),
            ("vendor".to_owned(), s(d.vendor)),
            ("vram_gb".to_owned(), Value::Float(d.vram_gb)),
            ("gpu_count".to_owned(), super::analyze::int(d.gpu_count)),
            (
                "card_vram_gb".to_owned(),
                Value::Arr(
                    d.card_vram_gb
                        .iter()
                        .map(|c| match c {
                            Num::Int(n) => super::analyze::int(*n),
                            Num::Float(f) => Value::Float(*f),
                        })
                        .collect(),
                ),
            ),
            ("host_ram_gb".to_owned(), Value::Float(d.host_ram_gb)),
            ("baseline".to_owned(), baseline(&d.baseline_args)?),
            (
                "baseline_args".to_owned(),
                Value::Arr(d.baseline_args.iter().map(|a| s(a)).collect()),
            ),
        ]));
    }
    Some(Value::Arr(out))
}

/// Why the native side left one answer to the child: logged once per class, never silently.
/// `true` when this call logged it.
pub(super) fn fell_back(class: &'static str) -> bool {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.contains(&class) {
        return false;
    }
    seen.push(class);
    eprintln!("{PREFIX} ({class}); logged once per reason");
    true
}

const PREFIX: &str =
    "native-section-autoconfig: the backend inventory was taken from the python child";

/// The descriptors' Docker reads (discovery, version, inspect, image) for one answer.
pub(super) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
mod tests;
