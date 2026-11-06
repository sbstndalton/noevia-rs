//! `PUT /api/models/preset`: the preset editor (core `routes/models.cjs` + `llamacpp-manager.cjs`
//! `applyPreset`, `llamacpp-presets.cjs` `prepare`, sbstndalton/noevia D2). Core's editor was a Node
//! executor behind a thin route; the whole workflow runs here now, and Core refuses the request
//! (rust-preset-write.cjs).
//!
//! The workflow, in Core's order:
//!
//! 1. the front's session, CSRF and administrator gates (core: the same gates before any route);
//! 2. an engine without a preset store answers 404 (no `applyPreset`); no preset file 501;
//!    `confirmReload` must be `true` (400);
//! 3. the maintenance hold ([`Maint::acquire`], the existing gate/lease client unchanged): the work
//!    runs on its own spawned task, so a client that leaves strands neither it nor the hold;
//! 4. the router's listing: a system routing model is refused (400);
//! 5. a dry run of the same write against the memory budget ([`Maint::preset_refusal`]): settings
//!    whose estimate is above the inference memory budget are refused (409) before anything is
//!    written;
//! 6. the real checks (the model is installed, nothing is loaded), `prepare` (validation, the
//!    explicit bounded prompt cache, the micro batch), the conditional commit (the revision is the
//!    base: a file that changed meanwhile is never overwritten), the reload under the same hold, and,
//!    when the reload fails, the restore of the previous file (again by revision) and a second
//!    reload, with Core's own words for each uncertain outcome.
//!
//! Fail closed: no estimate is invented (a budget Node does not answer is a 503; a refused estimate
//! refuses), and, new here, a lease on Node's gate that is no longer sure to outlast a step stops that
//! step: before the write nothing is changed; after it the previous file is put back without asking
//! the engine to reload.
//!
//! Unchanged from Core on purpose: error statuses and texts (a thrown 4xx keeps its message, other
//! failures are `Internal error`), the order of the checks, and the requests the router sees.

use super::ini::{Candidate, IniError};
use super::reload::Outcome;
use super::router::{Fault, Reply, Router};
use super::{body_text, text, Hold, HoldError, Maint};
use crate::model_sizing::{
    canonical, js_space, parse_presets, read_presets, FootprintFault, Presets, Thrown,
};
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::JValue;
use serde_json::{json, Value};
use server_auth::Role;
use server_stats::compose::Read;
use std::sync::Arc;
use std::time::Duration;

pub const PATH: &str = "/api/models/preset";
/// Node's side of the cache drop after a write (core rust-preset-write-effects.cjs).
const EFFECTS: &str = "/internal/preset-editor/effects";
const CACHE_DROP_TRIES: u32 = 3;
const FACTS: &str = "/internal/models/facts";

/// How long the hold may last: a listing, the write, a reload (120 s), then the restore and a second
/// reload.
const HOLD_FOR: Duration = Duration::from_secs(600);
const RELOAD_LIMIT: Duration = Duration::from_secs(120);
const WRITE_LIMIT: Duration = Duration::from_secs(60);

const SYSTEM_MODEL_REASON: &str = "System routing model \u{2014} not tuned";
const BUSY: &str = "Inference is busy. Wait for requests to finish before changing native presets.";
const NO_ARBITER: &str = "The model service could not confirm that chat is idle, so nothing was changed. Try again shortly.";
const BUDGET_UNAVAILABLE: &str =
    "The inference memory budget is unavailable right now. Try again shortly.";
const NOT_UNLOADED: &str = "Unload all router models and finish downloads before applying a profile. Other clients must remain stopped.";
const STALE: &str = "Presets changed. Reload the profile before saving; your draft is retained.";
const QUALIFICATION: &str =
    "unqualified; load and test this profile before relying on its capacity";
const LEASE_LOST_BEFORE: &str = "The model service stopped confirming that chat is held back, so the preset was not applied and nothing was changed. Try again shortly.";
const LEASE_LOST_UNRESTORED: &str = "The model service stopped confirming that chat is held back after the preset file was saved, so the engine was not asked to reload and the previous preset file was not put back. The current models.ini settings could not be confirmed; inspect native presets, then reload them once chat is idle.";
const LEASE_LOST_RELOAD_ONLY: &str = "Reload failed or timed out. The previous preset file was restored, but the model service stopped confirming that chat is held back, so the engine was not asked to reload again. Reload the presets once chat is idle.";
const RESTORE_UNCERTAIN: &str = "Reload outcome is uncertain and the file changed again. Stop inference and inspect native presets before retrying.";
const RESTORE_RETRYABLE: &str = "Reload failed or timed out, and the previous preset file could not be restored: models.ini still holds the new settings. Check router and Model Loader health before retrying.";
const RESTORE_UNCONFIRMED: &str = "Reload failed or timed out, and restoring the previous preset file could not be confirmed. Stop inference and inspect native presets before retrying.";
const RELOAD_RESTORED: &str =
    "Reload failed or timed out. Previous preset file restored; check router health before retrying.";

/// `PUT` of the path: the methods this front answers (routes.toml).
pub fn owned(method: &Method, path: &str) -> bool {
    path == PATH && method == Method::PUT
}

fn out(status: u16, body: &Value) -> Outcome {
    Outcome {
        status,
        body: body.to_string(),
    }
}

fn error(status: u16, message: &str) -> Outcome {
    out(status, &json!({ "error": message }))
}

fn internal() -> Outcome {
    error(500, "Internal error")
}

/// http.cjs `errorResponse` of an error Core threw: a 4xx keeps its text, anything else is a fixed one.
fn thrown(status: Option<u16>, message: &str) -> Outcome {
    match status {
        Some(s) if (400..500).contains(&s) => error(s, message),
        Some(s) if (500..600).contains(&s) => error(s, "Internal error"),
        _ => internal(),
    }
}

fn thrown_ini(e: &IniError) -> Outcome {
    thrown(Some(e.status), &e.message)
}

/// Core's `error(status, message)` of `prepare`.
struct Fail(u16, String);

impl Fail {
    fn bad(message: impl Into<String>) -> Fail {
        Fail(400, message.into())
    }

    fn outcome(&self) -> Outcome {
        thrown(Some(self.0), &self.1)
    }
}

impl From<Thrown> for Fail {
    fn from(t: Thrown) -> Fail {
        Fail(t.status.unwrap_or(500), t.message)
    }
}

// ── prepare (llamacpp-presets.cjs) ───────────────────────────────────────────────────────────

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// One strict policy call of the presets port: the reply object, or `None` when it refused.
fn presets_port(op: u8, args: &Value) -> Option<Value> {
    let mut input = vec![op];
    input.extend_from_slice(args.to_string().as_bytes());
    let (status, reply) = llamacpp_presets::call(&input);
    if status != 0 {
        return None;
    }
    serde_json::from_slice(&reply).ok()
}

fn model_name_ok(model: &str) -> bool {
    presets_port(4, &json!([model]))
        .and_then(|r| r.get("ok").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn hard_max(limits: (f64, f64)) -> Value {
    serde_json::from_str(&js_json::number_to_string(limits.0)).unwrap_or(Value::Null)
}

/// `optionValue(key, value, limits)`: the value as it would be written, or `None` where it is refused.
fn option_value(key: &str, value: &str, limits: (f64, f64)) -> Option<String> {
    let r = presets_port(1, &json!([[[key, value]], hard_max(limits)]))?;
    let values = r.get("values")?.as_array()?;
    match values.as_slice() {
        [Value::String(s)] => Some(s.clone()),
        _ => None,
    }
}

/// `microBatchExceeds(ubatch, batch)`: `None` where the port does not decide.
fn micro_batch_exceeds(ubatch: Option<&str>, batch: Option<&str>) -> Option<bool> {
    let r = presets_port(3, &json!([ubatch, batch]))?;
    r.get("exceeds").and_then(Value::as_bool)
}

/// `^\s*([^=\s]+)\s*=`: the key of an option line (without the value checks of `parse`).
fn line_key(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches(js_space);
    let end = rest.find(|c: char| c == '=' || js_space(c))?;
    if end == 0 {
        return None;
    }
    let after = rest.get(end..)?.trim_start_matches(js_space);
    after.starts_with('=').then(|| rest.get(..end)).flatten()
}

/// `new RegExp('^\\s*-{0,2}' + k + '\\s*=\\s*(true|1|on)\\s*$', 'i').test(line)`.
fn mode_flag(line: &str, name: &str) -> bool {
    let mut rest = line.trim_start_matches(js_space);
    for _ in 0..2 {
        if let Some(r) = rest.strip_prefix('-') {
            rest = r;
        }
    }
    let Some(head) = rest.get(..name.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(name) {
        return false;
    }
    let Some(rest) = rest.get(name.len()..) else {
        return false;
    };
    let Some(rest) = rest.trim_start_matches(js_space).strip_prefix('=') else {
        return false;
    };
    let value = rest.trim_matches(js_space);
    ["true", "1", "on"]
        .iter()
        .any(|v| value.eq_ignore_ascii_case(v))
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(needle)
}

/// `Object.hasOwn(updates, key)` / `updates[key]`.
fn lookup<'a>(updates: &'a [(String, String)], key: &str) -> Option<&'a str> {
    updates
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn assign(updates: &mut Vec<(String, String)>, key: &str, value: String) {
    match updates.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => updates.push((key.to_owned(), value)),
    }
}

fn section_option<'a>(data: &'a Presets, name: &str, key: &str) -> Option<&'a str> {
    data.section(name)?
        .options
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

/// `createPresetStore().prepare(body)`. `read` is the store's `read()`, called where Core reads
/// the file (after the options were checked).
fn prepare(
    read: impl FnOnce() -> Result<Presets, Thrown>,
    body: &JValue,
    limits: (f64, f64),
) -> Result<(Candidate, Presets), Fail> {
    let Some(model) = body.get("model").as_str() else {
        return Err(Fail::bad("Invalid preset model name"));
    };
    if !model_name_ok(model) {
        return Err(Fail::bad("Invalid preset model name"));
    }
    let JValue::Obj(options) = body.get("options") else {
        return Err(Fail::bad("Preset options are required"));
    };
    let mut updates: Vec<(String, String)> = Vec::new();
    for (key, raw) in options {
        let value = match raw {
            JValue::Str(raw) => option_value(key, raw, limits),
            _ => None,
        };
        let Some(value) = value else {
            return Err(Fail::bad(format!("Invalid preset option: {key}")));
        };
        assign(&mut updates, key, value);
    }
    if updates.is_empty() {
        return Err(Fail::bad("No preset changes supplied"));
    }
    let data = read()?;
    if body.get("baseRevision").as_str() != Some(data.revision.as_str()) {
        return Err(Fail(409, STALE.into()));
    }
    let section = data.section(model);
    let mut lines = data.lines.clone();
    {
        let cap = js_json::number_to_string(limits.1);
        let own = match lookup(&updates, "cache-ram") {
            Some(v) => Some(v.to_owned()),
            None => section_option(&data, model, "cache-ram").map(str::to_owned),
        };
        let inherited = section_option(&data, "*", "cache-ram").map(str::to_owned);
        let flag = |name: &str| {
            section.is_some_and(|s| {
                lines
                    .get(s.start + 1..s.end)
                    .is_some_and(|body| body.iter().any(|l| mode_flag(l, name)))
            })
        };
        let no_cache = flag("embedding")
            || flag("embeddings")
            || flag("reranking")
            || flag("rerank")
            || contains_ignore_case(model, "embed")
            || contains_ignore_case(model, "rerank");
        let own_empty = own.as_deref().is_none_or(str::is_empty);
        let effective = if own_empty { inherited } else { own.clone() };
        let checked = |value: &str| {
            option_value("cache-ram", value, limits)
                .ok_or_else(|| Fail::bad("Invalid preset option: cache-ram"))
        };
        if no_cache && own_empty {
            assign(&mut updates, "cache-ram", checked("0")?);
        } else if effective.as_deref().is_none_or(str::is_empty) {
            assign(&mut updates, "cache-ram", checked(&cap)?);
        } else if let Some(effective) = effective {
            let answer = checked(&effective)?;
            if answer != js_trim(&effective) {
                assign(&mut updates, "cache-ram", answer);
            }
        }
    }
    let additions: Vec<String> = updates
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k} = {v}"))
        .collect();
    if let Some(section) = section {
        let content: Vec<String> = lines
            .get(section.start + 1..section.end)
            .unwrap_or_default()
            .iter()
            .filter(|line| {
                line_key(line)
                    .is_none_or(|key| canonical(key).is_none_or(|c| lookup(&updates, c).is_none()))
            })
            .cloned()
            .collect();
        lines.splice(
            section.start + 1..section.end,
            content.into_iter().chain(additions),
        );
    } else {
        lines.push(String::new());
        lines.push(format!("[{model}]"));
        lines.extend(additions);
    }
    let joined = lines.join("\n");
    let text = format!("{}\n", joined.trim_end_matches('\n'));
    let mut local: Vec<(&str, String)> = section
        .map(|s| s.options.iter().map(|(k, v)| (*k, v.clone())).collect())
        .unwrap_or_default();
    for (key, value) in &updates {
        let key = canonical(key).unwrap_or("");
        local.retain(|(k, _)| *k != key);
        if !value.is_empty() {
            local.push((key, value.clone()));
        }
    }
    let value_of = |key: &str| {
        local
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
            .or_else(|| section_option(&data, "*", key).map(str::to_owned))
    };
    let batch = value_of("batch-size");
    let batch = match batch.as_deref() {
        None | Some("") => "2048".to_owned(),
        Some(b) => b.to_owned(),
    };
    match micro_batch_exceeds(value_of("ubatch-size").as_deref(), Some(&batch)) {
        Some(true) => return Err(Fail::bad("Micro batch cannot exceed batch size")),
        Some(false) => {}
        None => return Err(Fail::bad("Invalid preset option: ubatch-size")),
    }
    let candidate = Candidate {
        base_revision: data.revision.clone(),
        before: data.text.clone(),
        text,
    };
    Ok((candidate, data))
}

// ── the budget (llamacpp-manager.cjs presetRefusal) ──────────────────────────────────────────

/// `sizeKnobProblem(options)`.
fn size_knob_problem(options: &[(&'static str, String)]) -> Option<String> {
    for key in ["ctx-size", "c", "ubatch-size", "batch-size"] {
        let Some((_, raw)) = options.iter().find(|(k, _)| *k == key) else {
            continue;
        };
        let text = js_trim(raw);
        if text.is_empty() {
            continue;
        }
        let ctx = key == "ctx-size" || key == "c";
        let digits = text.strip_prefix('+').unwrap_or(text);
        let numeric = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        let n: f64 = if numeric {
            text.parse().unwrap_or(f64::INFINITY)
        } else {
            f64::NAN
        };
        let safe = n.is_finite() && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0;
        if !numeric || !safe || n < 0.0 || (n == 0.0 && !ctx) {
            let mut shown = String::new();
            js_json::quote(&js_json::js_slice(raw, 40), &mut shown);
            return Some(format!(
                "{key} must be {} (got {shown}).",
                if ctx {
                    "a whole number (0 for the native context)"
                } else {
                    "a positive whole number"
                }
            ));
        }
    }
    None
}

fn js_text(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `n.toLocaleString('en-US')` for the whole numbers the estimate holds.
fn locale_en_us(v: &JValue) -> Option<String> {
    let n = v.as_num()?;
    if !n.is_finite() || n.fract() != 0.0 || n.abs() >= 1e21 {
        return None;
    }
    let digits = format!("{:.0}", n.abs());
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    Some(if n < 0.0 {
        format!("-{grouped}")
    } else {
        grouped
    })
}

/// A refusal as Core's `presetRefusal` returned it, with the body text in Core's key order.
struct Refusal(String);

impl Refusal {
    fn of(error: &str, code: &str, budget: Option<f64>, estimate: Option<&JValue>) -> Refusal {
        let mut pairs = vec![
            ("error", JValue::Str(error.to_owned())),
            ("code", JValue::Str(code.to_owned())),
        ];
        if let Some(b) = budget {
            pairs.push(("budgetGib", JValue::Num(b)));
        }
        if let Some(e) = estimate {
            pairs.push(("estimate", e.clone()));
        }
        Refusal(js_json::stringify(&JValue::obj(pairs)).unwrap_or_default())
    }
}

const IMPL_FOOTPRINT: &str = "Not saved: the memory estimate (load footprint) could not be confirmed by the Rust sizing port, and the JS estimate is not the larger one.";
const IMPL_CACHE_FREE: &str = "Not saved: the memory estimate (isPromptCacheFree) could not be confirmed by the Rust sizing port, and the JS estimate is not the larger one.";

/// Why the dry run cannot answer: the answer to give instead.
type Stop = Outcome;

fn as_read(reply: &Result<Reply, Fault>) -> Read {
    match reply {
        Err(_) => Read::Rejected,
        Ok(r) => Read::Done {
            ok: r.ok,
            body: match &r.body {
                Some(v) => server_stats::js::body_of(v.to_string().as_bytes()),
                None => prompt_framing::json::Value::Null,
            },
        },
    }
}

/// `isPromptCacheFree(model, options)`: `None` when the port refuses.
fn prompt_cache_free(model: &str, options: &[(&'static str, String)]) -> Option<bool> {
    let opts: serde_json::Map<String, Value> = options
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Value::String(v.clone())))
        .collect();
    let mut input = vec![5u8];
    input.extend_from_slice(
        json!({ "model": model, "options": opts })
            .to_string()
            .as_bytes(),
    );
    let (status, text) = llamacpp_autoconfig::call(&input);
    if status != 0 {
        return None;
    }
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get("free")?
        .as_bool()
}

fn merged(
    globals: &[(&'static str, String)],
    own: &[(&'static str, String)],
) -> Vec<(&'static str, String)> {
    let mut out = globals.to_vec();
    for (k, v) in own {
        match out.iter_mut().find(|(ok, _)| ok == k) {
            Some(slot) => slot.1 = v.clone(),
            None => out.push((k, v.clone())),
        }
    }
    out
}

impl Maint {
    /// The inference memory budget Core enforces (`inferenceBudget.budgetGib()`), asked of Node as
    /// the sizing reads do. `Err`: Node did not answer, so no estimate can be judged.
    async fn budget_gib(&self, app: &App) -> Result<f64, Stop> {
        let answer = app
            .config
            .research
            .node_call(FACTS, &json!({ "want": "budget" }), Duration::from_secs(5))
            .await;
        let Some((200, bytes)) = answer else {
            return Err(error(503, BUDGET_UNAVAILABLE));
        };
        let figure = js_json::wtf16::parse(&String::from_utf8_lossy(&bytes))
            .ok()
            .and_then(|v| v.get("budget").get("budgetGib").as_num());
        Ok(figure.unwrap_or(f64::NAN))
    }

    /// `presetRefusal(model, options)`: the 409 body to refuse a save with, or `None` when it fits.
    #[allow(clippy::too_many_lines)]
    async fn preset_refusal(
        &self,
        app: &App,
        router: &Router,
        token: Option<&str>,
        model: &str,
        globals: &[(&'static str, String)],
        options: &[(&'static str, String)],
    ) -> Result<Option<Refusal>, Stop> {
        if let Some(problem) = size_knob_problem(options) {
            return Ok(Some(Refusal::of(
                &format!("Not saved: {problem}"),
                "invalid_size",
                None,
                None,
            )));
        }
        // #1234: the micro batch is split out of the batch, so it cannot be larger.
        let effective = merged(globals, options);
        let get = |key: &str| {
            effective
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| js_trim(v).to_owned())
                .filter(|v| !v.is_empty())
        };
        match micro_batch_exceeds(
            get("ubatch-size").as_deref(),
            get("batch-size").as_deref().or(Some("2048")),
        ) {
            Some(true) => {
                return Ok(Some(Refusal::of(
                    "Not saved: Micro batch cannot exceed batch size",
                    "invalid_size",
                    None,
                    None,
                )))
            }
            Some(false) => {}
            None => {
                return Err(thrown(Some(400), "Invalid preset option: ubatch-size"));
            }
        }
        if self.ini.is_none() || !self.sizing.has_models_mount() {
            return Ok(None);
        }
        let budget = self.budget_gib(app).await?;
        if budget.is_nan() || budget <= 0.0 {
            return Ok(None);
        }
        let listing = router.models(token).await;
        let read = as_read(&listing);
        let Ok(readable) = self.sizing.readable(model, &read) else {
            return Ok(None);
        };
        let mut opts = effective;
        let unset = opts
            .iter()
            .find(|(k, _)| *k == "cache-ram")
            .is_none_or(|(_, v)| v.is_empty());
        if unset {
            let Some(free) = prompt_cache_free(model, &opts) else {
                return Ok(Some(Refusal::of(
                    IMPL_CACHE_FREE,
                    "autoconfig_impl",
                    Some(budget),
                    None,
                )));
            };
            let value = if free {
                "0".to_owned()
            } else {
                js_json::number_to_string(self.sizing.preset_cache_ram_limits().1)
            };
            match opts.iter_mut().find(|(k, _)| *k == "cache-ram") {
                Some(slot) => slot.1 = value,
                None => opts.push(("cache-ram", value)),
            }
        }
        let estimate = match self.sizing.footprint_of(&readable, model, &opts) {
            Ok(text) => text,
            Err(FootprintFault::Refused | FootprintFault::Unreadable) => {
                return Ok(Some(Refusal::of(
                    IMPL_FOOTPRINT,
                    "autoconfig_impl",
                    Some(budget),
                    None,
                )))
            }
        };
        let Ok(est) = js_json::wtf16::parse(&estimate) else {
            return Ok(Some(Refusal::of(
                IMPL_FOOTPRINT,
                "autoconfig_impl",
                Some(budget),
                None,
            )));
        };
        let budget_text = js_json::number_to_string(budget);
        let total = est.get("totalGib").as_num().filter(|n| n.is_finite());
        let unbounded = est.get("cacheRamUnbounded").truthy();
        if !unbounded && total.is_some_and(|t| t <= budget) {
            return Ok(None);
        }
        let message = if unbounded {
            format!("Not saved: an unbounded prompt cache (cache-ram = -1) cannot fit the {budget_text} GiB inference memory budget.")
        } else if total.is_none() {
            format!("Not saved: with these settings the memory need of {model} cannot be estimated as a finite figure, so it cannot be checked against the {budget_text} GiB inference memory budget. Set a whole-number context.")
        } else {
            // `est.ctx.toLocaleString('en-US')` throws on anything but a number.
            let Some(ctx) = locale_en_us(est.get("ctx")) else {
                return Err(internal());
            };
            format!(
                "Not saved: with these settings {model} needs about {} GiB (weights {}, context {} at {ctx} tokens, prompt cache {}, runtime {}), above the {budget_text} GiB inference memory budget. Lower the context or prompt cache, or raise the budget.",
                js_text(est.get("totalGib")),
                js_text(est.get("modelGib")),
                js_text(est.get("kvGib")),
                js_text(est.get("cacheRamGib")),
                js_text(est.get("extraGib")),
            )
        };
        Ok(Some(Refusal::of(
            &message,
            "inference_budget",
            Some(budget),
            Some(&est),
        )))
    }
}

// ── the workflow (llamacpp-manager.cjs applyPreset / applyUnlocked) ───────────────────────────

/// `listing.body?.data`'s rows; a throwing `find`/`some` is `Err`.
enum Rows<'a> {
    List(&'a [Value]),
    /// `data` is falsy: `|| []`.
    Empty,
    /// `data` is truthy and not an array.
    NotAnArray,
}

fn rows_of(body: &Option<Value>) -> Rows<'_> {
    match body.as_ref().and_then(|b| b.get("data")) {
        Some(Value::Array(rows)) => Rows::List(rows),
        None | Some(Value::Null | Value::Bool(false)) => Rows::Empty,
        Some(Value::String(s)) if s.is_empty() => Rows::Empty,
        Some(Value::Number(n)) if n.as_f64().is_some_and(|f| f == 0.0 || f.is_nan()) => Rows::Empty,
        Some(_) => Rows::NotAnArray,
    }
}

/// `m.id === model` for a row that is not null (a null row throws).
fn id_is(row: &Value, model: &JValue) -> bool {
    match (row.get("id"), model) {
        (Some(Value::String(a)), JValue::Str(b)) => a == b,
        (Some(Value::Number(a)), JValue::Num(b)) => a.as_f64() == Some(*b),
        (Some(Value::Bool(a)), JValue::Bool(b)) => a == b,
        (Some(Value::Null), JValue::Null) => true,
        _ => false,
    }
}

/// `rows.some(pred)`, where a null row throws (`Err`).
fn some(rows: &[Value], pred: impl Fn(&Value) -> bool) -> Result<bool, ()> {
    for row in rows {
        if row.is_null() {
            return Err(());
        }
        if pred(row) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn model_path_from_args(row: &Value) -> Option<String> {
    let args = row.get("status")?.get("args")?.as_array()?;
    let i = args
        .iter()
        .position(|a| matches!(a.as_str(), Some("--model" | "-m")))?;
    args.get(i + 1)?.as_str().map(str::to_owned)
}

/// `applyPreset`'s system model check on the first listing.
fn system_model(ini: &super::ini::Store, listing: &Reply, model: &JValue) -> Result<bool, Stop> {
    let rows = match rows_of(&listing.body) {
        Rows::List(rows) => rows,
        Rows::Empty => &[][..],
        Rows::NotAnArray => return Err(internal()),
    };
    // `find` stops at the first match; a null row before it throws.
    let mut found: Option<&Value> = None;
    for row in rows {
        if row.is_null() {
            return Err(internal());
        }
        if id_is(row, model) {
            found = Some(row);
            break;
        }
    }
    let Some(row) = found else {
        return Ok(false);
    };
    let id = row.get("id").and_then(Value::as_str).unwrap_or_default();
    let path = model_path_from_args(row).filter(|p| !p.is_empty());
    let roles = crate::model_kind::roles_beside(Some(ini.file()));
    Ok(crate::model_kind::is_system_model_at(
        &roles,
        id,
        path.as_deref(),
    ))
}

impl Maint {
    async fn read_ini(&self, ini: &super::ini::Store) -> Result<Presets, Thrown> {
        let file = ini.file().to_owned();
        tokio::task::spawn_blocking(move || read_presets(&file))
            .await
            .unwrap_or_else(|_| {
                Err(Thrown {
                    status: None,
                    message: "task".into(),
                })
            })
    }

    /// The dry run: `prepare` against the current file, then `parse` of the candidate text.
    /// Anything that throws there is no dry run (Core's `try {...} catch {}`).
    async fn dry_run(
        &self,
        ini: &super::ini::Store,
        body: &JValue,
        limits: (f64, f64),
    ) -> Option<(Vec<(&'static str, String)>, Vec<(&'static str, String)>)> {
        let file = ini.file().to_owned();
        let body = body.clone();
        tokio::task::spawn_blocking(move || {
            let (candidate, data) = prepare(|| read_presets(&file), &body, limits).ok()?;
            let model = body.get("model").as_str()?.to_owned();
            let parsed = parse_presets(candidate.text).ok()?;
            let options = parsed.section(&model)?.options.clone();
            let globals = data
                .section("*")
                .map(|s| s.options.clone())
                .unwrap_or_default();
            Some((globals, options))
        })
        .await
        .ok()
        .flatten()
    }

    /// `applyPreset(body)` under the hold.
    #[allow(clippy::too_many_lines)]
    async fn apply_held(
        &self,
        app: &App,
        router: &Router,
        hold: &Hold,
        ini: &super::ini::Store,
        body: &JValue,
    ) -> Outcome {
        let token = hold.token();
        let t = token.as_deref();
        let limits = self.sizing.preset_cache_ram_limits();
        // The system model check.
        let first = match router.models(t).await {
            Ok(l) => l,
            Err(Fault) => return internal(),
        };
        if first.ok {
            match system_model(ini, &first, body.get("model")) {
                Err(stop) => return stop,
                Ok(true) => return error(400, SYSTEM_MODEL_REASON),
                Ok(false) => {}
            }
        }
        // #697: a dry run of the same write, estimated before anything is committed.
        if let Some((globals, options)) = self.dry_run(ini, body, limits).await {
            let model = body.get("model").as_str().unwrap_or_default();
            match self
                .preset_refusal(app, router, t, model, &globals, &options)
                .await
            {
                Err(stop) => return stop,
                Ok(Some(refusal)) => {
                    return Outcome {
                        status: 409,
                        body: refusal.0,
                    }
                }
                Ok(None) => {}
            }
        }
        self.apply_preset_unlocked(router, hold, ini, body, limits)
            .await
    }

    /// `applyUnlocked(body)`: the caller holds the maintenance gate.
    #[allow(clippy::too_many_lines)]
    async fn apply_preset_unlocked(
        &self,
        router: &Router,
        hold: &Hold,
        ini: &super::ini::Store,
        body: &JValue,
        limits: (f64, f64),
    ) -> Outcome {
        let token = hold.token();
        let t = token.as_deref();
        let listing = match router.models(t).await {
            Ok(l) => l,
            Err(Fault) => return internal(),
        };
        if !listing.ok {
            return Outcome {
                status: listing.status,
                body: listing.body.clone().unwrap_or(Value::Null).to_string(),
            };
        }
        let Rows::List(models) = rows_of(&listing.body) else {
            return internal();
        };
        let model = body.get("model");
        match some(models, |m| id_is(m, model)) {
            Err(()) => return internal(),
            Ok(false) => return error(404, "Choose an installed model"),
            Ok(true) => {}
        }
        let busy = some(models, |m| {
            m.get("status")
                .and_then(|s| s.get("value"))
                .and_then(Value::as_str)
                != Some("unloaded")
        });
        match busy {
            Err(()) => return internal(),
            Ok(true) => return error(409, NOT_UNLOADED),
            Ok(false) => {}
        }
        let file = ini.file().to_owned();
        let task_body = body.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            prepare(|| read_presets(&file), &task_body, limits).map(|(c, _)| c)
        })
        .await;
        let candidate = match prepared {
            Ok(Ok(c)) => c,
            Ok(Err(fail)) => return fail.outcome(),
            Err(_) => return internal(),
        };
        if !hold.may_start(WRITE_LIMIT) {
            return error(503, LEASE_LOST_BEFORE);
        }
        if let Err(e) = ini
            .commit_reconciled_if(&candidate, true, hold.write_fence())
            .await
        {
            // A settled Model Loader outcome carries its own message; nothing was reloaded.
            return if e.status >= 500 {
                // `error` first, then `retryable`: serde_json sorts keys alphabetically, which is
                // also this order.
                let o = if e.retryable {
                    json!({ "error": e.message, "retryable": true })
                } else {
                    json!({ "error": e.message })
                };
                out(e.status, &o)
            } else {
                thrown_ini(&e)
            };
        }
        let model_name = body.get("model").as_str().unwrap_or_default().to_owned();
        match self.reload_applied(router, hold, ini, &model_name).await {
            Ok(applied) => Outcome {
                status: 200,
                body: applied,
            },
            Err(Reloaded::LeaseLost) => self.restore(router, hold, ini, &candidate, true).await,
            Err(Reloaded::Failed) => self.restore(router, hold, ini, &candidate, false).await,
        }
    }

    /// The reload and, once it worked, the answer: `{...presets.get(model), applied, qualification}`.
    async fn reload_applied(
        &self,
        router: &Router,
        hold: &Hold,
        ini: &super::ini::Store,
        model: &str,
    ) -> Result<String, Reloaded> {
        if !hold.may_start(RELOAD_LIMIT) {
            return Err(Reloaded::LeaseLost);
        }
        match self.router_reload(router, hold, None).await {
            Ok(Ok(reply)) if reply.ok => {}
            Err(()) | Ok(Ok(_)) => return Err(Reloaded::Failed),
            Ok(Err("lease_lost")) => return Err(Reloaded::LeaseLost),
            Ok(Err(_)) => return Err(Reloaded::Failed),
        }
        let data = self.read_ini(ini).await.map_err(|_| Reloaded::Failed)?;
        Ok(applied_body(&data, model))
    }

    /// The reload did not work: put the previous file back, only if it still is ours, and ask for a
    /// second reload (its outcome is not judged).
    async fn restore(
        &self,
        router: &Router,
        hold: &Hold,
        ini: &super::ini::Store,
        candidate: &Candidate,
        lease_lost: bool,
    ) -> Outcome {
        // Compensation is a write like any other: without exclusion that is still confirmed it does not
        // start, and the state is reported as uncertain for a separately fenced recovery (D2 review P1).
        // A restore the hold covers is a compare-and-swap against the revision this job committed.
        if lease_lost || !hold.may_start(WRITE_LIMIT) {
            return error(
                503,
                if lease_lost {
                    LEASE_LOST_UNRESTORED
                } else {
                    RESTORE_UNCONFIRMED
                },
            );
        }
        let revision = super::ini::sha(&candidate.text);
        let back = Candidate {
            base_revision: revision,
            text: candidate.before.clone(),
            before: candidate.text.clone(),
        };
        if let Err(e) = ini
            .commit_reconciled_if(&back, true, hold.write_fence())
            .await
        {
            let message = if e.status == 409 {
                RESTORE_UNCERTAIN
            } else if e.retryable {
                RESTORE_RETRYABLE
            } else {
                RESTORE_UNCONFIRMED
            };
            return error(503, message);
        }
        if !hold.may_start(RELOAD_LIMIT) {
            return error(503, LEASE_LOST_RELOAD_ONLY);
        }
        let _ = self.router_reload(router, hold, None).await;
        error(503, RELOAD_RESTORED)
    }
}

enum Reloaded {
    /// The reload failed, refused or timed out.
    Failed,
    /// The lease on Node's gate was not sure to outlast the reload: the engine was not asked.
    LeaseLost,
}

/// `{...presets.get(model), applied: true, qualification}`, with `presets.get`'s key order.
fn applied_body(data: &Presets, model: &str) -> String {
    let obj = |pairs: &[(&'static str, String)]| {
        JValue::Obj(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), JValue::Str(v.clone())))
                .collect(),
        )
    };
    let section = data.section(model);
    let defaults = data.section("*");
    let fields = JValue::Arr(
        crate::model_sizing::field_names()
            .iter()
            .map(|n| JValue::Str((*n).to_owned()))
            .collect(),
    );
    let body = JValue::obj([
        ("model", JValue::Str(model.to_owned())),
        ("revision", JValue::Str(data.revision.clone())),
        ("exists", JValue::Bool(section.is_some())),
        (
            "options",
            section.map_or_else(|| JValue::Obj(Vec::new()), |s| obj(&s.options)),
        ),
        (
            "defaults",
            defaults.map_or_else(|| JValue::Obj(Vec::new()), |s| obj(&s.options)),
        ),
        ("fields", fields),
        ("applied", JValue::Bool(true)),
        ("qualification", JValue::Str(QUALIFICATION.to_owned())),
    ]);
    js_json::stringify(&body).unwrap_or_default()
}

impl Maint {
    /// `applyPreset(body)` end to end: everything but the HTTP request. Runs to its end whatever
    /// happens to the client: callers spawn it.
    pub async fn apply_preset(&self, app: &App, body: JValue) -> Outcome {
        let Some(router) = self.router.as_ref() else {
            return error(404, "Native presets are unavailable");
        };
        let Some(ini) = self.ini.as_ref() else {
            return error(
                501,
                "Native preset editing; configure LLAMACPP_PRESET_PATH is not exposed by this llama.cpp adapter",
            );
        };
        if body.get("confirmReload").as_bool() != Some(true) {
            return error(
                400,
                "Confirm that other clients and Diary background inference are stopped before reloading.",
            );
        }
        let mut hold = match self.acquire("", HOLD_FOR, false).await {
            Ok(h) => h,
            Err(HoldError::Busy) => return error(409, BUSY),
            Err(HoldError::Gate { status, message }) => return thrown(Some(status), &message),
            Err(HoldError::NeedsGate) => return error(503, NO_ARBITER),
            Err(HoldError::NodeUnavailable) => return error(503, NO_ARBITER),
        };
        let outcome = self.apply_held(app, router, &hold, ini, &body).await;
        let admission = self
            .authority
            .recovery_pending()
            .then(|| hold.defer_local_release());
        hold.release().await;
        if let Some(admission) = admission {
            let maint = Arc::clone(&app.config.maint);
            tokio::spawn(async move {
                let _admission = admission;
                maint.reconcile_preset_incident().await;
            });
        }
        outcome
    }
    pub fn spawn_preset_recovery(self: &Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.authority.recovery_pending() || self.ini.is_none() || self.router.is_none() {
            return None;
        }
        let reserved = super::Reservation::take(&self.local)?;
        let maint = Arc::clone(self);
        Some(tokio::spawn(async move {
            let _reserved = reserved;
            maint.reconcile_preset_incident().await;
        }))
    }

    async fn reconcile_preset_incident(&self) {
        // Other operation owners (managed services, etc.) owe their own covered readback.
        if self
            .authority
            .pending_operation_kinds()
            .map_or(true, |kinds| {
                kinds.iter().any(|k| {
                    !matches!(
                        k.as_str(),
                        "preset-write"
                            | "restore"
                            | "reload"
                            | "reread"
                            | "control"
                            | "acquire"
                            | "pull"
                            | "register"
                            | "delete"
                            | "prune"
                    )
                })
            })
        {
            return;
        }
        let mut delay = Duration::from_millis(100);
        loop {
            let ticket = self.authority.require_recovery();
            if let Ok(hold) = self
                .acquire_recovery("Reconciling interrupted native preset editing", HOLD_FOR)
                .await
            {
                let mut reconciled = false;
                if let Some(router) = &self.router {
                    let token = hold.token();
                    if let Ok(listing) = router.models(token.as_deref()).await {
                        let mut empty = listing.ok && super::router::rows(&listing.body).is_some();
                        if let Some(rows) = super::router::rows(&listing.body) {
                            if rows.iter().any(|row| {
                                matches!(
                                    row.get("status")
                                        .and_then(|s| s.get("value"))
                                        .and_then(Value::as_str),
                                    Some("downloading" | "loading" | "unloading")
                                )
                            }) {
                                empty = false;
                            }
                            for model in super::reload::live_ids(rows) {
                                if !hold.may_start(Duration::from_secs(60)) {
                                    empty = false;
                                    break;
                                }
                                let Some(mut operation) = hold.operation() else {
                                    empty = false;
                                    break;
                                };
                                let scoped = router.with_operation(&operation.fence);
                                match scoped.unload(&model, token.as_deref()).await {
                                    Ok(reply) if reply.ok && !reply.uncertain => {
                                        operation.settled =
                                            operation.authority.settle_operation(&operation.fence);
                                    }
                                    _ => {
                                        empty = false;
                                        break;
                                    }
                                }
                            }
                        }
                        if empty && hold.may_start(RELOAD_LIMIT) {
                            reconciled = self
                                .router_reload(router, &hold, None)
                                .await
                                .is_ok_and(|r| r.is_ok_and(|reply| reply.ok))
                                && hold.excluded();
                        }
                    }
                }
                let node_completed = hold.gated_now()
                    || match hold.recovery_proof() {
                        Some((id, generation)) if reconciled && hold.excluded() => {
                            self.node.complete_recovery(&ticket, id, generation).await
                        }
                        _ => false,
                    };
                if reconciled
                    && node_completed
                    && hold.excluded()
                    && self.authority.complete_recovery(&ticket)
                {
                    hold.release().await;
                    return;
                }
                hold.release().await;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }
}

/// The request. Core's gates in Core's order, then the work on its own task.
pub async fn serve(app: &Arc<App>, req: Request<Body>, path: String) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    // routes/models.cjs: every write under /api/models/ is administrator-only, before anything else.
    if id.user.role != Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "administrator required", true);
    }
    let maint = Arc::clone(&app.config.maint);
    if maint.router.is_none() {
        return reply::error(
            StatusCode::NOT_FOUND,
            "Native presets are unavailable",
            true,
        );
    }
    let raw = match body_text(body, 1024 * 1024, Duration::from_secs(10)).await {
        Ok(t) => t,
        Err(413) => {
            return reply::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds size limit",
                true,
            )
        }
        Err(_) => return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true),
    };
    let parsed = if raw.is_empty() {
        Ok(JValue::Obj(Vec::new()))
    } else {
        js_json::wtf16::parse(&raw)
    };
    let Ok(parsed) = parsed else {
        return reply::error(StatusCode::BAD_REQUEST, "invalid JSON", true);
    };
    let job_app = Arc::clone(app);
    let job = tokio::spawn(async move {
        let outcome = job_app.config.maint.apply_preset(&job_app, parsed).await;
        // Core dropped its cached model reads around every write under /api/models/. Node is told once
        // the write ended, with a bounded retry; a notification that never arrived is not hidden: the
        // answer says so in a header (the cached reads expire on their own within seconds).
        let mut dropped = false;
        for attempt in 0..CACHE_DROP_TRIES {
            let call = job_app
                .config
                .research
                .node_call(EFFECTS, &json!({ "op": "applied" }), Duration::from_secs(3))
                .await;
            if matches!(call, Some((200, _))) {
                dropped = true;
                break;
            }
            if attempt + 1 < CACHE_DROP_TRIES {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        if !dropped {
            eprintln!("[models] the preset editor could not tell Node to drop its cached model reads; they expire on their own");
        }
        (outcome, dropped)
    });
    match job.await {
        Ok((o, dropped)) => {
            let mut response = text(o.status, o.body);
            if !dropped {
                response.headers_mut().insert(
                    "x-noevia-cache-drop",
                    axum::http::HeaderValue::from_static("failed"),
                );
            }
            response
        }
        Err(_) => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn mode_flags_follow_the_javascript_regex() {
        for (line, name, want) in [
            ("embedding = true", "embedding", true),
            ("  --embedding=ON  ", "embedding", true),
            ("-embeddings = 1", "embeddings", true),
            ("---embedding = true", "embedding", false),
            ("embedding = false", "embedding", false),
            ("embeddings = true", "embedding", false),
            ("embedding = true # x", "embedding", false),
            ("rerank = On\r", "rerank", true),
            ("embedding", "embedding", false),
        ] {
            assert_eq!(mode_flag(line, name), want, "{line:?} {name}");
        }
    }

    #[test]
    fn option_lines_are_keyed_like_the_regex() {
        assert_eq!(line_key("ctx-size = 4096"), Some("ctx-size"));
        assert_eq!(line_key("  c=1"), Some("c"));
        assert_eq!(line_key("= 1"), None);
        assert_eq!(line_key("key value"), None);
        assert_eq!(line_key("; comment"), None);
        assert_eq!(line_key(""), None);
    }

    #[test]
    fn size_knobs_name_the_first_bad_value() {
        let opts = |kv: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            kv.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
        };
        assert_eq!(size_knob_problem(&opts(&[("ctx-size", "0")])), None);
        assert_eq!(size_knob_problem(&opts(&[("ctx-size", "+4096")])), None);
        assert_eq!(size_knob_problem(&opts(&[("batch-size", "")])), None);
        assert_eq!(
            size_knob_problem(&opts(&[("batch-size", "0")])).unwrap(),
            "batch-size must be a positive whole number (got \"0\")."
        );
        assert_eq!(
            size_knob_problem(&opts(&[("ctx-size", "-1"), ("batch-size", "0")])).unwrap(),
            "ctx-size must be a whole number (0 for the native context) (got \"-1\")."
        );
        assert!(
            size_knob_problem(&opts(&[("ubatch-size", "99999999999999999999")]))
                .unwrap()
                .starts_with("ubatch-size must be a positive whole number")
        );
    }

    #[test]
    fn contexts_are_grouped_like_to_locale_string() {
        for (n, want) in [
            (262_144.0, "262,144"),
            (999.0, "999"),
            (1_048_576.0, "1,048,576"),
            (0.0, "0"),
        ] {
            assert_eq!(locale_en_us(&JValue::Num(n)).unwrap(), want);
        }
        assert_eq!(locale_en_us(&JValue::Null), None);
        assert_eq!(locale_en_us(&JValue::Num(1.5)), None);
    }

    #[test]
    fn thrown_errors_keep_a_4xx_text_and_hide_the_rest() {
        assert_eq!(thrown(Some(409), "x").body, r#"{"error":"x"}"#);
        assert_eq!(thrown(Some(503), "x").body, r#"{"error":"Internal error"}"#);
        assert_eq!(thrown(Some(503), "x").status, 503);
        assert_eq!(thrown(None, "x").status, 500);
        assert_eq!(thrown(Some(302), "x").status, 500);
    }
}
