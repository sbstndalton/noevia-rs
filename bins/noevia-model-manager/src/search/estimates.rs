//! noevia#1318 step 3a: the repository read's fit chips and context estimates natively.
//!
//! `GET /api/v1/search/repo` asked the child's `POST /api/v1/internal/search-repo-runtime` for
//! each file group's `services.vram_fit_chips(bytes)` and, for a model file with a header summary,
//! `helpers._preset_estimates_with_reason(summary, bytes, mmproj_gb)` and the model's native
//! context. While this process samples the backends and runs the section autoconfig itself
//! (`MODEL_MANAGER_SECTION_AUTOCONFIG_IMPL=rust`, which fixes the cache-ram cap the analysis
//! reads), the same values are computed here:
//!
//! - the chips: [`fit::chips`] over one fit inventory per answer, rendered as the child's
//!   `JSONResponse` renders them;
//! - the estimates: `analyze()` (the section autoconfig's port, every rule through the
//!   `model-autoconfig` crate) once per preset, with no models folder (no file beside the model
//!   is read) and the smallest projector's size as `mmproj_gb_override`, against the backends
//!   `_preset_estimates_with_reason` builds (`_fit_backends()`, each with vendor `cuda`, the
//!   sampled GPU count and per-card VRAM, the host RAM and an empty baseline);
//! - `nativeCtx`: `(summary.get("model") or {}).get("context_length") or 0`.
//!
//! Python rediscovers the containers for every chip list and every estimate; this side reads one
//! inventory per answer, as the other fit readers do.
//!
//! Reference analysis exceptions are caught as the helper's exact HTTP200 empty-estimate
//! reason, while reference JSON-render failures and explicit native resource refusals remain
//! typed errors. Installed strict routing never retries a companion RPC. Optional compatibility
//! routing may still ask its configured companion after a bounded native refusal.
use super::Live;

/// Unsupported native resource/shape limits are separate from Python's JSON-render errors.
#[derive(Debug)]
pub(super) enum Error {
    Unsupported,
    ReferenceFailure,
}
impl From<Error> for super::Stop {
    fn from(error: Error) -> Self {
        match error {
            Error::Unsupported => Self::Refused,
            Error::ReferenceFailure => Self::Failed,
        }
    }
}
fn need<T>(value: Option<T>) -> Result<T, Error> {
    value.ok_or(Error::Unsupported)
}

use crate::backend_stats::{
    fit::{self, Descriptor, Floats, Inventory},
    Num,
};
use crate::model_updates::json_string;
use crate::section_autoconfig::analyze::{self, Inputs};
use model_files::json::Value;
use serde_json::value::RawValue;

/// `_preset_estimates_with_reason`'s presets, in order.
const PRESETS: [(&str, &str); 3] = [
    ("fast", "Fast"),
    ("balanced", "Balanced"),
    ("long-ctx", "Long ctx"),
];
/// `helpers._REASON_MAX`.
const REASON_MAX: usize = 600;
const NO_GPU: &str = "No GPU backend reports its VRAM, so there is nothing to size a context \
against. CPU-only backends are not sized here.";
const NO_FIT: &str = "Autoconfig found no context size that fits.";

/// One group of the request.
pub(super) struct Ask {
    pub bytes: u128,
    pub estimate: bool,
}

fn num(n: &Num) -> Value {
    match n {
        Num::Int(i) => analyze::int(*i),
        Num::Float(f) => Value::Float(*f),
    }
}

/// The backend list `_preset_estimates_with_reason` sizes against.
pub(super) fn backends(descriptors: &[Descriptor]) -> Value {
    Value::Arr(
        descriptors
            .iter()
            .map(|d| {
                Value::Obj(vec![
                    ("name".to_owned(), Value::Str(d.name.clone())),
                    ("vendor".to_owned(), Value::Str("cuda".to_owned())),
                    ("vram_gb".to_owned(), Value::Float(d.vram_gb)),
                    ("gpu_count".to_owned(), analyze::int(d.gpu_count)),
                    (
                        "card_vram_gb".to_owned(),
                        Value::Arr(d.card_vram_gb.iter().map(num).collect()),
                    ),
                    ("host_ram_gb".to_owned(), Value::Float(d.host_ram_gb)),
                    ("baseline".to_owned(), Value::Obj(Vec::new())),
                ])
            })
            .collect(),
    )
}

fn int_of(v: Option<&Value>) -> Option<i128> {
    model_autoconfig::pyval::big(v?, "estimate").ok()
}

/// An int as `json.dumps` writes it; `None` for anything else.
fn int_text(v: Option<&Value>) -> Option<String> {
    int_of(v).map(|n| n.to_string())
}

/// `_preset_estimates_with_reason(summary, size_bytes, mmproj_gb)` as the child renders it:
/// `(estimates JSON, reason)`.
pub(super) fn estimates(
    summary: &Value,
    size_bytes: u128,
    mmproj_gb: f64,
    backends: &Value,
    cache_ram_cap_mib: i64,
) -> Result<(String, String), Error> {
    if matches!(backends, Value::Arr(items) if items.is_empty()) {
        return Ok(("[]".to_owned(), NO_GPU.to_owned()));
    }
    struct Row {
        text: String,
        ctx: i128,
        offload: bool,
    }
    let mut out: Vec<Row> = Vec::new();
    let mut reason = String::new();
    for (key, label) in PRESETS {
        let inputs = Inputs {
            name: "",
            summary,
            file_size: size_bytes,
            backends: backends.clone(),
            model_rel: "",
            current: None,
            preset: key,
            n_sessions: 1,
            model_subdir: "",
            spec_profile: "",
            vision: true,
            prompt_tps: 0.0,
            prompt_budget_s: 120.0,
            verified_ctx: 0,
            mode: "",
            cache_ram_cap_mib,
            files: None,
            // `mmproj_gb or None`.
            mmproj_gb_override: (mmproj_gb != 0.0).then_some(mmproj_gb),
        };
        // The reference helper catches analysis exceptions, but not JSON serialization.
        let rec = match analyze::analyze_typed(&inputs) {
            Ok(rec) => rec,
            Err(analyze::AnalysisError::Python(class)) => {
                return Ok((
                    "[]".to_owned(),
                    format!("Autoconfig failed on this file's metadata ({class})."),
                ))
            }
            Err(analyze::AnalysisError::Failed) => return Err(Error::ReferenceFailure),
            Err(analyze::AnalysisError::Refused) => return Err(Error::Unsupported),
        };
        let error = match rec.get("error") {
            Some(Value::Str(e)) => e.clone(),
            _ => return Err(Error::Unsupported),
        };
        let ctx_value = rec.get("recommended_ctx");
        if !error.is_empty() || !ctx_value.is_some_and(Value::truthy) {
            if reason.is_empty() {
                let text = if error.is_empty() { NO_FIT } else { &error };
                reason = text.chars().take(REASON_MAX).collect();
            }
            continue;
        }
        let ctx = need(int_of(ctx_value))?;
        let active = need(rec.get("active_preset"))?;
        let chosen = match rec.get("presets") {
            Some(Value::Arr(items)) => items.iter().find(|p| p.get("key") == Some(active)),
            _ => return Err(Error::Unsupported),
        };
        let (gpu_layers, total_layers, speed_pct, offload) = match chosen {
            Some(p) => {
                let speed = match need(p.get("speed_score"))? {
                    // `round(x * 100)`: Python raises for a non-finite product.
                    Value::Float(f) => model_autoconfig::pyfloat::round_int(f * 100.0).map_err(
                        |error| match error {
                            model_autoconfig::Error::Python(_) => Error::ReferenceFailure,
                            _ => Error::Unsupported,
                        },
                    )?,
                    Value::Int(_) => need(need(int_of(p.get("speed_score")))?.checked_mul(100))?,
                    _ => return Err(Error::Unsupported),
                };
                (
                    need(int_text(p.get("gpu_layers")))?,
                    need(int_text(p.get("total_layers")))?,
                    speed,
                    p.get("offload_kind").is_some_and(Value::truthy),
                )
            }
            None => ("0".to_owned(), "0".to_owned(), 100, false),
        };
        out.push(Row {
            text: format!(
                "{{\"key\":{},\"label\":{},\"ctx\":{ctx},\"ctx_h\":{},\"gpu_layers\":{gpu_layers},\"total_layers\":{total_layers},\"speed_pct\":{speed_pct},\"offload\":{offload}}}",
                json_string(key),
                json_string(label),
                json_string(&model_autoconfig::pystr::fmt_ctx(ctx)),
            ),
            ctx,
            offload,
        });
        // One real answer at native context: three identical chips would imply choices.
        if out.len() > 1
            && out.last().map(|r| r.ctx) == out.first().map(|r| r.ctx)
            && out.last().is_some_and(|r| !r.offload)
        {
            out.pop();
        }
    }
    let rows: Vec<&str> = out.iter().map(|r| r.text.as_str()).collect();
    let reason = if out.is_empty() {
        reason
    } else {
        String::new()
    };
    Ok((format!("[{}]", rows.join(",")), reason))
}

/// `(summary.get("model") or {}).get("context_length") or 0` as `json.dumps` writes it.
fn native_ctx(summary: &Value) -> Result<String, Error> {
    let model = match summary.get("model") {
        None => return Ok("0".to_owned()),
        Some(model) if !model.truthy() => return Ok("0".to_owned()),
        Some(model @ Value::Obj(_)) => model,
        Some(_) => return Err(Error::ReferenceFailure), // Python .get raises.
    };
    let value = model.get("context_length").filter(|v| v.truthy());
    let mut out = String::new();
    json_value(
        &mut out,
        value.unwrap_or(&Value::Int(model_files::BigInt::zero())),
    )?;
    Ok(out)
}

/// JSONResponse's finite JSON, retaining full nested nativeCtx values and arbitrary JSON ints.
fn json_value(out: &mut String, value: &Value) -> Result<(), Error> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Float(f) if f.is_finite() => out.push_str(&gguf::py_float_repr(*f)),
        Value::Float(_) => return Err(Error::ReferenceFailure),
        Value::Str(s) => out.push_str(&json_string(s)),
        Value::Arr(values) => {
            out.push('[');
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_value(out, value)?;
            }
            out.push(']');
        }
        Value::Obj(values) => {
            out.push('{');
            for (i, (key, value)) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(key));
                out.push(':');
                json_value(out, value)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// The header summary as the dict the child's `json.loads` makes of it. A real summary carries
/// the whole chat template (often past the reader's default 4096-character string cap), so
/// strings are bounded by the summary itself (a summary of at most the first MiB of a header).
pub(super) fn summary(text: &str) -> Option<Value> {
    let limits = model_files::json::Limits {
        string_chars: SUMMARY_CAP,
        bytes: SUMMARY_CAP,
        nodes: SUMMARY_NODES,
        ..model_files::json::Limits::default()
    };
    model_files::json::parse_with_limits(text, limits).ok()
}

/// Largest summary read (the header read is one MiB; its JSON escapes can at most sextuple it).
const SUMMARY_CAP: usize = 8 * 1024 * 1024;
/// Most JSON values in a summary (a header's arrays are capped well below this by `gguf`).
const SUMMARY_NODES: usize = 1 << 20;

/// Why the native side left an answer to the child: counted at 1, 10, 100 ... per class.
pub(super) fn fell_back(class: &'static str) -> bool {
    static LOG: std::sync::OnceLock<crate::backend_stats::fallback::Log> =
        std::sync::OnceLock::new();
    if let Some(line) = LOG.get_or_init(Default::default).record(PREFIX, class) {
        eprintln!("{line}");
        true
    } else {
        false
    }
}

const PREFIX: &str = "native-search: the repository estimates were asked of the python child";

fn raw(text: String) -> Option<Box<RawValue>> {
    RawValue::from_string(text).ok()
}

/// `main._search_repo_runtime`'s groups: the chips of every group and, when asked, its estimates.
pub(super) fn groups(
    asks: &[Ask],
    mmproj_bytes: Option<u128>,
    summary: Option<&Value>,
    inventory: &Inventory,
    descriptors: &[Descriptor],
    cache_ram_cap_mib: i64,
) -> Result<Vec<Live>, Error> {
    // `mm_bytes / 1024 ** 3`: an int divided by an int is one correctly rounded division.
    // The divisor is a power of two, so integer rounding followed by scaling is exact.
    let mm_gb = mmproj_bytes.map_or(0.0, |bytes| bytes as f64 / 1_073_741_824.0);
    let backends = backends(descriptors);
    let mut out = Vec::with_capacity(asks.len());
    for ask in asks {
        let fit = need(raw(fit::chips(ask.bytes, inventory, Floats::Raise)
            .map_err(|error| match error {
                crate::backend_stats::Fail::Raised => Error::ReferenceFailure,
                _ => Error::Unsupported,
            })?))?;
        let mut live = Live {
            fit,
            estimates: None,
            reason: String::new(),
            native_ctx: None,
        };
        if let Some(summary) = summary.filter(|s| s.truthy()) {
            if ask.estimate {
                // Serialization / model.get errors always fail the reference response,
                // even if analysis itself catches a metadata exception.
                let context = native_ctx(summary)?;
                let (estimates, reason) =
                    estimates(summary, ask.bytes, mm_gb, &backends, cache_ram_cap_mib)?;
                live.estimates = Some(need(raw(estimates))?);
                live.reason = reason;
                live.native_ctx = Some(need(raw(context))?);
            }
        }
        out.push(live);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
