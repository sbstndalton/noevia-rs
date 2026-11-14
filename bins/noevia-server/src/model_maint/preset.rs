//! Track R11: `PUT /api/models/preset`, the admin preset editor's save (core `routes/models.cjs`,
//! llamacpp-manager.cjs `applyPreset`, `applyUnlocked`, `presetRefusal`), answered by the front
//! behind `NOEVIA_RUST_MODEL_WRITES=rust` (model_writes_routes.rs reads the request; this is the
//! work). In Node's order, under one maintenance hold (`maintenance.exclusive`, no reason):
//!
//! 1. The router's listing: the system routing model (models.roles.json, then the deprecated name
//!    and path rule) is refused with Node's `SYSTEM_MODEL_REASON`. A failed listing skips the check.
//! 2. A dry run of the write (`presets.prepare`, model_maint ini.rs), its failures ignored here: the
//!    section the write would leave is checked (`presetRefusal`) for its size knobs, the micro batch
//!    against the effective batch, and, with a models mount, the inference memory budget (the
//!    estimate is crates/llamacpp-autoconfig's strict operation 3, Node's `estimateFootprint`).
//! 3. `applyUnlocked`: the listing again (a failure is passed on as the router gave it), the model
//!    must be listed and every model unloaded, then the real `prepare` (its 4xx answered as thrown),
//!    the commit (`commitReconciled`), and the router's re-read. A failed re-read puts this write's
//!    own previous text back (compare-and-swap), never an operator's later edit.
//!
//! The success answer is the profile as `presets.get(model)` reads it after the re-read, with
//! `applied` and `qualification`. Whatever Node throws is answered as index.cjs does (http.cjs
//! `errorResponse`: a 4xx keeps its text, anything else is `Internal error`).
//!
//! The router's answers are read from their bytes as `fetchJson` does (`JSON.parse`, or the text),
//! so a passed-on body keeps the router's key order and number text.
//!
//! Named differences (each tested in tests/model_preset.rs):
//! - **No budget.** With a models mount the budget is Core's figure over the facts bridge; when the
//!   bridge gives none, the save is refused (503 `The inference memory budget could not be read.`)
//!   rather than written unchecked. Core always has a budget, so its answer never depends on this.
//! - **No arbiter.** With no gate hold and no answer from Node's maintenance gate (the front's
//!   lease), nothing is done: 503 with the reload's text. Core holds its own gate in that case.
//! - **A lease lost before the re-read.** Core registers a recovery (chat paused until it restores
//!   under a fresh lease) and throws (503 `Internal error`). The front's lease cannot end under a
//!   call in flight, so a lost lease is only ever found before the router is asked: the router never
//!   saw the new text, and the front puts its own previous text back at once (compare-and-swap)
//!   and answers the same 503. Not recorded (Core's lease authority is the front itself).
//! - A router answer the port's JSON reader cannot hold (nested past its depth, a number outside
//!   `f64`) is read as text.

use super::{Hold, HoldError, Maint};
use crate::model_sizing::{parse_presets, read_presets, strict, Presets, Thrown};
use js_json::{wtf16, JValue};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// inference-maintenance.cjs `busy()`.
pub const BUSY: &str =
    "Inference is busy. Wait for requests to finish before changing native presets.";
const CONFIRM: &str =
    "Confirm that other clients and Diary background inference are stopped before reloading.";
const UNCONFIGURED: &str = "Native preset editing; configure LLAMACPP_PRESET_PATH is not exposed by this llama.cpp adapter";
/// model-system.cjs `SYSTEM_MODEL_REASON` (Node's text for every refusal of the system model).
const SYSTEM_MODEL_REASON: &str = "System routing model — not tuned";
const NOT_INSTALLED: &str = "Choose an installed model";
const QUALIFICATION: &str = "unqualified; load and test this profile before relying on its capacity";
const INTERNAL: &str = "Internal error";
const NO_BUDGET: &str = "The inference memory budget could not be read.";
/// llamacpp-autoconfig.cjs `AutoconfigImplError(what)`.
fn impl_error(what: &str) -> String {
    format!("the memory estimate ({what}) could not be confirmed by the Rust sizing port, and the JS estimate is not the larger one.")
}
/// The longest the work may hold the engine: two listings, the estimate's, two commits and two
/// re-reads, with room.
const HOLD_LIMIT: Duration = Duration::from_secs(420);
/// llamacpp-presets.cjs `DEFAULT_BATCH_SIZE`.
const DEFAULT_BATCH: &str = "2048";
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// The answer: status and JSON text (empty for Node's `json(res, status, undefined)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub body: String,
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}

fn error(status: u16, message: &str) -> Answer {
    Answer {
        status,
        body: text(&JValue::obj([("error", JValue::from(message))])),
    }
}

/// http.cjs `errorResponse(err)` for an error with `status` and `message`.
pub fn thrown(status: Option<u16>, message: &str) -> Answer {
    match status {
        Some(s) if (400..500).contains(&s) => error(
            s,
            if message.is_empty() {
                "Request failed"
            } else {
                message
            },
        ),
        Some(s) if (500..600).contains(&s) => error(s, INTERNAL),
        _ => error(500, INTERNAL),
    }
}

fn internal() -> Answer {
    thrown(None, "")
}

fn refused(v: &JValue) -> Answer {
    Answer {
        status: 409,
        body: text(v),
    }
}

/// `fetchJson`'s body: `JSON.parse(text)`, or the text.
fn body_of(bytes: &[u8]) -> JValue {
    let t = String::from_utf8_lossy(bytes);
    wtf16::parse(&t).unwrap_or_else(|_| JValue::Str(t.into_owned()))
}

/// `m.id` (and the like) for an element of the listing: `null` throws, a primitive has none.
fn field<'a>(m: &'a JValue, key: &str) -> Result<&'a JValue, ()> {
    match m {
        JValue::Null => Err(()),
        _ => Ok(m.get(key)),
    }
}

/// `a === b` for JSON values.
fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `String(id)` as isSystemModel reads a row's id (`''` for undefined and null).
fn id_text(v: &JValue) -> String {
    match v {
        JValue::Undefined | JValue::Null => String::new(),
        JValue::Str(s) => s.clone(),
        JValue::Num(n) => js_json::number_to_string(*n),
        JValue::Bool(b) => b.to_string(),
        other => js_json::to_js_string(other).unwrap_or_default(),
    }
}

/// model-system.cjs `modelPathFromArgs(args)`.
fn model_path(args: &JValue) -> String {
    let JValue::Arr(a) = args else {
        return String::new();
    };
    a.iter()
        .position(|v| matches!(v.as_str(), Some("--model" | "-m")))
        .and_then(|i| a.get(i + 1))
        .and_then(JValue::as_str)
        .unwrap_or("")
        .to_owned()
}

/// ECMAScript `String.prototype.trim`.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| u16::try_from(u32::from(c)).is_ok_and(server_stats::js::is_space))
}

fn get<'a>(opts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    opts.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// `{ ...a, ...b }` over string options.
fn merged(a: &[(String, String)], b: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = a.to_vec();
    for (k, v) in b {
        match out.iter_mut().find(|(o, _)| o == k) {
            Some(slot) => slot.1.clone_from(v),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out
}

fn options_value(opts: &[(String, String)]) -> JValue {
    JValue::Obj(
        opts.iter()
            .map(|(k, v)| (k.clone(), JValue::Str(v.clone())))
            .collect(),
    )
}

fn owned(opts: &[(&'static str, String)]) -> Vec<(String, String)> {
    opts.iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// llamacpp-manager.cjs `sizeKnobProblem(options)`: the reason, or `None`.
pub fn size_knob_problem(options: &[(String, String)]) -> Option<String> {
    for key in ["ctx-size", "c", "ubatch-size", "batch-size"] {
        let Some(raw) = get(options, key) else {
            continue;
        };
        let t = js_trim(raw);
        if t.is_empty() {
            continue;
        }
        let ctx = key == "ctx-size" || key == "c";
        let digits = t.strip_prefix('+').unwrap_or(t);
        let shaped = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        let n = if shaped {
            digits.parse::<f64>().unwrap_or(f64::NAN)
        } else {
            f64::NAN
        };
        if !shaped || !(n <= MAX_SAFE) || (n == 0.0 && !ctx) {
            return Some(format!(
                "{key} must be {} (got {}).",
                if ctx {
                    "a whole number (0 for the native context)"
                } else {
                    "a positive whole number"
                },
                text(&JValue::Str(wtf16::slice(raw, 40)))
            ));
        }
    }
    None
}

/// `${value}` in a template literal.
fn interp(v: &JValue) -> String {
    match v {
        JValue::Undefined => "undefined".into(),
        JValue::Null => "null".into(),
        JValue::Num(n) => super::calibration::plain(*n),
        other => js_json::to_js_string(other).unwrap_or_default(),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// The request body's `options` as `prepare` iterates them (`Object.entries`), `None` when they
/// are not a plain object; a value that is not a string is `None`.
fn options_of(v: &JValue) -> Option<Vec<(String, Option<String>)>> {
    let JValue::Obj(items) = v else {
        return None;
    };
    Some(
        items
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned)))
            .collect(),
    )
}

/// `presets.get(model)` with the save's `applied` and `qualification`.
fn saved_profile(p: &Presets, model: &str) -> String {
    let section = p.span(model);
    let options = section.map(|(_, _, o)| owned(o)).unwrap_or_default();
    let defaults = p.span("*").map(|(_, _, o)| owned(o)).unwrap_or_default();
    text(&JValue::obj([
        ("model", JValue::from(model)),
        ("revision", JValue::Str(p.revision.clone())),
        ("exists", JValue::Bool(section.is_some())),
        ("options", options_value(&options)),
        ("defaults", options_value(&defaults)),
        (
            "fields",
            JValue::Arr(
                Presets::field_names()
                    .into_iter()
                    .map(JValue::from)
                    .collect(),
            ),
        ),
        ("applied", JValue::Bool(true)),
        ("qualification", JValue::from(QUALIFICATION)),
    ]))
}

/// The `[*]` options as `presets.get(model).defaults` reads them now (`{}` when the read throws).
async fn defaults_now(file: PathBuf) -> Vec<(String, String)> {
    blocking(move || {
        read_presets(&file)
            .ok()
            .and_then(|p| p.span("*").map(|(_, _, o)| owned(o)))
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

/// What a listing settled to, as its bytes.
struct Listing {
    ok: bool,
    status: u16,
    /// `None`: too large (fetchJson's `{ ok: false }` with no body).
    body: Option<JValue>,
}

impl Maint {
    async fn listing(&self, hold: Option<&str>) -> Result<Listing, ()> {
        let router = self.router.as_ref().ok_or(())?;
        let raw = router.models_raw(hold).await.map_err(|_| ())?;
        let body = raw.bytes.as_deref().map(body_of);
        Ok(Listing {
            ok: raw.ok && body.is_some(),
            status: raw.status,
            body,
        })
    }

    /// `applyPreset(body)`: the whole save, run to its end whatever happens to the client (the
    /// caller spawns it). `budget` reads Core's inference memory budget, asked only when an estimate
    /// is due.
    pub async fn apply_preset<F, Fut>(&self, body: &JValue, budget: F) -> Answer
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<f64>>,
    {
        let (Some(_), Some(_)) = (self.router.as_ref(), self.ini.as_ref()) else {
            return error(501, UNCONFIGURED);
        };
        if body.get("confirmReload") != &JValue::Bool(true) {
            return error(400, CONFIRM);
        }
        let hold = match self.acquire("", HOLD_LIMIT, false).await {
            Ok(h) => h,
            Err(HoldError::Busy | HoldError::Gate { status: 409, .. }) => return error(409, BUSY),
            Err(HoldError::Gate { status, message }) => return thrown(Some(status), &message),
            Err(HoldError::NeedsGate | HoldError::NodeUnavailable) => {
                return error(503, super::reload::NO_ARBITER)
            }
        };
        let answer = self.apply_held(&hold, body, budget).await;
        hold.release().await;
        answer
    }

    async fn apply_held<F, Fut>(&self, hold: &Hold, body: &JValue, budget: F) -> Answer
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<f64>>,
    {
        let (Some(router), Some(ini)) = (self.router.as_ref(), self.ini.clone()) else {
            return error(501, UNCONFIGURED);
        };
        let token = hold.token();
        let h = token.as_deref();
        let wanted = body.get("model");
        // The system routing model is never edited here.
        let Ok(first) = self.listing(h).await else {
            return internal();
        };
        if first.ok {
            let data = first.body.as_ref().map_or(&JValue::Undefined, |b| b.get("data"));
            let rows: &[JValue] = match data {
                JValue::Arr(rows) => rows,
                d if !d.truthy() => &[],
                // `(…).find` is not a function.
                _ => return internal(),
            };
            for m in rows {
                let Ok(id) = field(m, "id") else {
                    return internal();
                };
                if !strict_eq(id, wanted) {
                    continue;
                }
                let path = model_path(m.get("status").get("args"));
                let roles = crate::model_kind::roles_beside(self.sizing.presets_path());
                if crate::model_kind::is_system_model_at(
                    &roles,
                    &id_text(id),
                    (!path.is_empty()).then_some(path.as_str()),
                ) {
                    return error(400, SYSTEM_MODEL_REASON);
                }
                break;
            }
        }
        let model = wanted.as_str().map(str::to_owned);
        let options = options_of(body.get("options"));
        let base = body
            .get("baseRevision")
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let limits = self.sizing.cache_ram_limits();
        // A dry run of the same write, estimated before anything is committed.
        if let (Some(m), Some(o)) = (model.clone(), options.clone()) {
            let (file, b) = (ini.file().to_path_buf(), base.clone());
            let section = blocking(move || {
                let c = super::ini::prepare(&file, &m, &b, &o, limits).ok()?;
                let p = parse_presets(c.text).ok()?;
                p.span(&m).map(|(_, _, opts)| owned(opts))
            })
            .await
            .flatten();
            if let (Some(m), Some(section)) = (model.as_deref(), section) {
                if let Some(answer) = self.preset_refusal(h, m, &section, budget).await {
                    return answer;
                }
            }
        }
        // applyUnlocked.
        let Ok(second) = self.listing(h).await else {
            return internal();
        };
        if !second.ok {
            return Answer {
                status: second.status,
                body: second.body.as_ref().map(text).unwrap_or_default(),
            };
        }
        let Some(JValue::Arr(rows)) = second.body.as_ref().map(|b| b.get("data")) else {
            return internal();
        };
        let mut listed = false;
        for m in rows {
            let Ok(id) = field(m, "id") else {
                return internal();
            };
            if strict_eq(id, wanted) {
                listed = true;
                break;
            }
        }
        if !listed {
            return error(404, NOT_INSTALLED);
        }
        for m in rows {
            let Ok(status) = field(m, "status") else {
                return internal();
            };
            if status.get("value").as_str() != Some("unloaded") {
                return error(409, super::calibration::UNLOAD_FIRST);
            }
        }
        let Some(model) = model.filter(|m| {
            llamacpp_presets::model_name_ok(&m.encode_utf16().collect::<Vec<u16>>())
        }) else {
            return thrown(Some(400), "Invalid preset model name");
        };
        let Some(options) = options else {
            return thrown(Some(400), "Preset options are required");
        };
        let (file, m) = (ini.file().to_path_buf(), model.clone());
        let candidate = match blocking(move || {
            super::ini::prepare(&file, &m, &base, &options, limits)
        })
        .await
        {
            Some(Ok(c)) => c,
            Some(Err(t)) => return thrown(t.status, &t.message),
            None => return internal(),
        };
        // The write and its re-read are one step; it starts only while the hold covers it.
        if !hold.may_start(super::reload::PRESET_STEP) {
            return thrown(Some(503), "");
        }
        if let Err(e) = ini.commit_reconciled(&candidate, true).await {
            // A settled Model Loader outcome carries its own message; nothing was reloaded.
            if e.status >= 500 {
                let mut out = vec![("error".to_owned(), JValue::Str(e.message))];
                if e.retryable {
                    out.push(("retryable".to_owned(), JValue::Bool(true)));
                }
                return Answer {
                    status: e.status,
                    body: text(&JValue::Obj(out)),
                };
            }
            return thrown(Some(e.status), &e.message);
        }
        let restore = super::ini::Candidate {
            base_revision: super::ini::sha(&candidate.text),
            text: candidate.before.clone(),
            before: candidate.text.clone(),
        };
        match self.router_reload(router, hold, None).await {
            Ok(Ok(reply)) if reply.ok => {
                let (file, m) = (ini.file().to_path_buf(), model.clone());
                let read = blocking(move || read_presets(&file).map(|p| saved_profile(&p, &m)))
                    .await;
                if let Some(Ok(body)) = read {
                    return Answer { status: 200, body };
                }
                // `presets.get` threw inside the JS's try: its catch restores, as below.
            }
            // The lease ended before the router was asked (see the module documentation).
            Ok(Err(_)) => {
                let _ = ini.commit_reconciled(&restore, true).await;
                return thrown(Some(503), "");
            }
            _ => {}
        }
        // Restore only our own version; never overwrite an operator's later edit.
        match ini.commit_reconciled(&restore, true).await {
            Err(e) if e.status == 409 => error(503, super::calibration::RELOAD_UNCERTAIN),
            Err(e) if e.retryable => error(503, super::calibration::RELOAD_STUCK),
            Err(_) => error(503, super::calibration::RELOAD_UNCONFIRMED),
            Ok(()) => {
                let _ = self.router_reload(router, hold, None).await;
                error(503, super::calibration::RELOAD_RESTORED)
            }
        }
    }

    /// `presetRefusal(model, section)`: the 409 body for a section that must not be saved, an
    /// answer for what Node throws (an undecided micro batch), or `None`.
    async fn preset_refusal<F, Fut>(
        &self,
        hold: Option<&str>,
        model: &str,
        section: &[(String, String)],
        budget: F,
    ) -> Option<Answer>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<f64>>,
    {
        if let Some(problem) = size_knob_problem(section) {
            return Some(refused(&JValue::obj([
                ("error", JValue::Str(format!("Not saved: {problem}"))),
                ("code", JValue::from("invalid_size")),
            ])));
        }
        let file = self.sizing.presets_path()?.to_path_buf();
        // The micro batch is split out of the batch: checked over the effective values.
        let eff = merged(&defaults_now(file.clone()).await, section);
        let units = |k: &str| {
            get(&eff, k)
                .map(js_trim)
                .filter(|v| !v.is_empty())
                .map(|v| v.encode_utf16().collect::<Vec<u16>>())
        };
        let batch = units("batch-size").unwrap_or_else(|| DEFAULT_BATCH.encode_utf16().collect());
        match llamacpp_presets::micro_batch_exceeds(units("ubatch-size").as_deref(), Some(&batch))
        {
            Some(false) => {}
            Some(true) => {
                return Some(refused(&JValue::obj([
                    (
                        "error",
                        JValue::from("Not saved: Micro batch cannot exceed batch size"),
                    ),
                    ("code", JValue::from("invalid_size")),
                ])))
            }
            None => return Some(thrown(Some(400), "Invalid preset option: ubatch-size")),
        }
        if !self.sizing.has_models_mount() {
            return None;
        }
        let Some(budget_gib) = budget().await else {
            return Some(error(503, NO_BUDGET));
        };
        // readModel: the router's listing names the model's files.
        let listing = match self.router.as_ref()?.models_raw(hold).await {
            Ok(r) => match r.bytes {
                Some(bytes) => server_stats::compose::Read::Done {
                    ok: r.ok,
                    body: server_stats::js::body_of(&bytes),
                },
                None => server_stats::compose::Read::Done {
                    ok: false,
                    body: prompt_framing::json::Value::Null,
                },
            },
            Err(_) => server_stats::compose::Read::Rejected,
        };
        let (sizing, m) = (Arc::clone(&self.sizing), model.to_owned());
        let facts = blocking(move || sizing.read_facts(&m, &listing))
            .await
            .flatten()?;
        let mut opts = merged(&defaults_now(file).await, section);
        let impl_refusal = |what: &str| {
            Some(refused(&JValue::obj([
                (
                    "error",
                    JValue::Str(format!("Not saved: {}", impl_error(what))),
                ),
                ("code", JValue::from("autoconfig_impl")),
                ("budgetGib", JValue::Num(budget_gib)),
            ])))
        };
        if get(&opts, "cache-ram").is_none_or(str::is_empty) {
            let args = text(&JValue::obj([
                ("model", JValue::from(model)),
                ("options", options_value(&opts)),
            ]));
            let Some(free) = strict(5, &args)
                .and_then(|t| wtf16::parse(&t).ok())
                .and_then(|v| v.get("free").as_bool())
            else {
                return impl_refusal("isPromptCacheFree");
            };
            let value = if free {
                "0".to_owned()
            } else {
                super::calibration::plain(self.sizing.cache_ram_limits().0)
            };
            opts = merged(&opts, &[("cache-ram".to_owned(), value)]);
        }
        let args = format!(
            "{{{facts},\"options\":{},\"model\":{}}}",
            text(&options_value(&opts)),
            text(&JValue::from(model))
        );
        let Some(est) = strict(3, &args).and_then(|t| wtf16::parse(&t).ok()) else {
            return impl_refusal("load footprint");
        };
        let unbounded = est.get("cacheRamUnbounded").truthy();
        let total = est
            .get("totalGib")
            .as_num()
            .filter(|n| n.is_finite());
        if !unbounded && total.is_some_and(|t| t <= budget_gib) {
            return None;
        }
        let b = super::calibration::plain(budget_gib);
        let message = if unbounded {
            format!("Not saved: an unbounded prompt cache (cache-ram = -1) cannot fit the {b} GiB inference memory budget.")
        } else if total.is_none() {
            format!("Not saved: with these settings the memory need of {model} cannot be estimated as a finite figure, so it cannot be checked against the {b} GiB inference memory budget. Set a whole-number context.")
        } else {
            let ctx = match est.get("ctx") {
                JValue::Num(n) => super::calibration::grouped(*n),
                other => interp(other),
            };
            format!(
                "Not saved: with these settings {model} needs about {} GiB (weights {}, context {} at {ctx} tokens, prompt cache {}, runtime {}), above the {b} GiB inference memory budget. Lower the context or prompt cache, or raise the budget.",
                interp(est.get("totalGib")),
                interp(est.get("modelGib")),
                interp(est.get("kvGib")),
                interp(est.get("cacheRamGib")),
                interp(est.get("extraGib")),
            )
        };
        Some(refused(&JValue::obj([
            ("error", JValue::Str(message)),
            ("code", JValue::from("inference_budget")),
            ("budgetGib", JValue::Num(budget_gib)),
            ("estimate", est),
        ])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn size_knobs_are_judged_as_node_does() {
        assert_eq!(size_knob_problem(&o(&[("ctx-size", "0")])), None);
        assert_eq!(size_knob_problem(&o(&[("ctx-size", "+4096")])), None);
        assert_eq!(size_knob_problem(&o(&[("ctx-size", " ")])), None);
        assert_eq!(
            size_knob_problem(&o(&[("ctx-size", "-1")])).as_deref(),
            Some(r#"ctx-size must be a whole number (0 for the native context) (got "-1")."#)
        );
        assert_eq!(
            size_knob_problem(&o(&[("batch-size", "0")])).as_deref(),
            Some(r#"batch-size must be a positive whole number (got "0")."#)
        );
        assert!(size_knob_problem(&o(&[("ctx-size", "9007199254740992")])).is_some());
        assert!(size_knob_problem(&o(&[("ctx-size", "9007199254740991")])).is_none());
        // The first failing key in Node's order wins.
        assert!(size_knob_problem(&o(&[("batch-size", "x"), ("ctx-size", "y")]))
            .is_some_and(|p| p.starts_with("ctx-size")));
        // Cut at 40 UTF-16 units: an emoji straddling the cut leaves a lone surrogate.
        let long = format!("{}\u{1F600}", "x".repeat(39));
        assert_eq!(
            size_knob_problem(&o(&[("ctx-size", &long)])).as_deref(),
            Some(
                format!(
                    "ctx-size must be a whole number (0 for the native context) (got \"{}\\ud83d\").",
                    "x".repeat(39)
                )
                .as_str()
            )
        );
    }

    #[test]
    fn thrown_errors_answer_as_index_cjs() {
        assert_eq!(
            thrown(Some(409), "x"),
            Answer {
                status: 409,
                body: r#"{"error":"x"}"#.into()
            }
        );
        assert_eq!(thrown(Some(502), "x").body, r#"{"error":"Internal error"}"#);
        assert_eq!(thrown(Some(502), "x").status, 502);
        assert_eq!(thrown(None, "x").status, 500);
        assert_eq!(thrown(Some(400), "").body, r#"{"error":"Request failed"}"#);
    }

    #[test]
    fn strict_equality_and_ids() {
        assert!(strict_eq(&JValue::Undefined, &JValue::Undefined));
        assert!(!strict_eq(&JValue::Num(f64::NAN), &JValue::Num(f64::NAN)));
        assert!(!strict_eq(&JValue::from("5"), &JValue::Num(5.0)));
        assert_eq!(id_text(&JValue::Num(5.0)), "5");
        assert_eq!(id_text(&JValue::Null), "");
        assert_eq!(
            model_path(&JValue::Arr(vec![
                JValue::from("-m"),
                JValue::from("/models/x.gguf")
            ])),
            "/models/x.gguf"
        );
        assert!(field(&JValue::Null, "id").is_err());
        assert_eq!(field(&JValue::Num(7.0), "id"), Ok(&JValue::Undefined));
    }
}
