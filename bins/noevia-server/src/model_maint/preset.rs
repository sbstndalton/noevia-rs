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
//! A hold that cannot cover the write and its re-read refuses before the commit (503 `Internal
//! error`, nothing written), where Node's `assertCovered(PRESET_STEP_MS)` refuses too (see the
//! named difference on how much cover is asked for).
//!
//! Named differences (tests/model_preset.rs):
//! - **No budget.** With a models mount the budget is Core's figure over the facts bridge; when the
//!   bridge gives none, the save is refused (503 `The inference memory budget could not be read.`)
//!   rather than written unchecked. Core always has a budget, so its answer never depends on this.
//!   Tested: `no_budget_from_core_refuses_the_save_with_a_mount`.
//! - **No arbiter.** With no gate hold and no answer from Node's maintenance gate (the front's
//!   lease), nothing is done: 503 with the reload's text. Core holds its own gate in that case.
//!   Tested: `no_arbiter_changes_nothing`.
//! - **A router answer the port's JSON reader cannot hold** (nested past its depth, a number
//!   outside `f64`) is read as text, so a passed-on one is answered as a JSON string. Tested:
//!   `a_router_answer_too_deep_to_hold_is_passed_on_as_text`.
//! - **Stricter cover.** The check before the commit is the hold's `may_start(PRESET_STEP)`: for a
//!   lease of Node's gate, Node's last confirmation plus the TTL and the lease's hard end must both
//!   outlast the 150 s step plus a margin. Node's `assertCovered` asks the TTL only to outlast its
//!   margin (and the hard end the step), so a lease with a short TTL that Node would let write is
//!   refused here (fail closed). Likewise a lease that is still held but cannot cover the 120 s
//!   re-read is not asked to re-read: the write is put back under the hold (503 with the reload's
//!   restore text) where Node would re-read and answer 200. Tested:
//!   `a_lease_that_cannot_cover_the_write_changes_nothing`.
//! - **A hold lost after the write, before the re-read.** Both keep chat paused and recover under a
//!   fresh hold: settle the engine, put this write's previous text back if the file still holds it
//!   (compare-and-swap), have the router re-read; retried until done. Node answers its thrown 503
//!   `Internal error` at once; the front answers 503 with the reload's text for the first attempt's
//!   outcome (restored, the file changed again, still the new text, unconfirmed), each attempt
//!   logged. Tested: `a_lease_lost_during_the_commit_is_recovered_before_chat_resumes`.

use super::{Hold, HoldError, Maint};
use crate::model_sizing::{parse_presets, read_presets, strict, Presets};
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
const QUALIFICATION: &str =
    "unqualified; load and test this profile before relying on its capacity";
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
static UNDEFINED: JValue = JValue::Undefined;
/// The recovery's hold (calibration's) and reason, its settle step's limit and poll, and how long
/// the answer waits for its first attempt.
const RECOVERY_LEASE: Duration = Duration::from_secs(15 * 60);
const RECOVERY_REASON: &str =
    "Chat is paused while noevia restores the presets an interrupted change left behind.";
const SETTLE_LIMIT: Duration = Duration::from_secs(120);
const SETTLE_POLL: Duration = Duration::from_millis(500);
const RECOVERY_ANSWER: Duration = Duration::from_secs(150);

/// How one recovery attempt after a lost hold ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovered {
    /// The previous text is back and the router re-read it.
    Restored,
    /// The file had changed again (not this write's text): left as it is, the router re-read it.
    NotOurs,
    /// Not finished, and models.ini still holds this write's text.
    Stuck,
    /// Not finished; whether the previous text is back is not confirmed.
    Unconfirmed,
}

impl Recovered {
    fn message(self) -> &'static str {
        match self {
            Recovered::Restored => super::calibration::RELOAD_RESTORED,
            Recovered::NotOurs => super::calibration::RELOAD_UNCERTAIN,
            Recovered::Stuck => super::calibration::RELOAD_STUCK,
            Recovered::Unconfirmed => super::calibration::RELOAD_UNCONFIRMED,
        }
    }
}

fn log_recovery(outcome: Recovered) {
    let entry = JValue::obj([
        ("event", JValue::from("preset.recovery")),
        (
            "outcome",
            JValue::from(format!("{outcome:?}").to_lowercase()),
        ),
        ("error", JValue::from(outcome.message())),
    ]);
    println!("[models] {}", text(&entry));
}

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
    opts.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
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
        if !shaped || n > MAX_SAFE || (n == 0.0 && !ctx) {
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
    async fn preset_listing(&self, hold: Option<&str>) -> Result<Listing, ()> {
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
    pub async fn apply_preset<F, Fut>(self: &Arc<Self>, body: &JValue, budget: F) -> Answer
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
        let mut lost = None;
        let answer = self.apply_held(&hold, body, budget, &mut lost).await;
        match lost {
            Some(restore) => self.recover_after_loss(hold, restore).await,
            None => {
                hold.release().await;
                answer
            }
        }
    }

    /// The hold was lost after the write, before the router was asked to re-read it: local
    /// admission stays closed (and the authority's recovery, which the lost lease's release requires,
    /// keeps chat paused) until a recovery under a fresh hold has settled the engine, put this write's
    /// previous text back if the file still holds it, and had the router re-read the file. Retried
    /// with backoff until it completes; the answer is its first attempt's outcome.
    async fn recover_after_loss(
        self: &Arc<Self>,
        mut hold: Hold,
        restore: super::ini::Candidate,
    ) -> Answer {
        let fence = hold.defer_local_release();
        hold.release().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut tx = Some(tx);
            let mut wait = Duration::from_secs(1);
            loop {
                let outcome = me.recover_once(&restore).await;
                log_recovery(outcome);
                if let Some(t) = tx.take() {
                    let _ = t.send(outcome);
                }
                if matches!(outcome, Recovered::Restored | Recovered::NotOurs) {
                    break;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(60));
            }
            fence.release();
        });
        match tokio::time::timeout(RECOVERY_ANSWER, rx).await {
            Ok(Ok(outcome)) => error(503, outcome.message()),
            _ => error(503, super::calibration::RELOAD_UNCONFIRMED),
        }
    }

    /// The file still holds the text `restore` replaces (this write's).
    async fn still_ours(&self, restore: &super::ini::Candidate) -> bool {
        let Some(ini) = self.ini.clone() else {
            return false;
        };
        let base = restore.base_revision.clone();
        blocking(move || ini.snapshot().is_ok_and(|s| s.revision == base))
            .await
            .unwrap_or(false)
    }

    /// One attempt of the recovery (inference-maintenance.cjs `runRecovery` with the manager's
    /// settle step and `applyUnlocked`'s repair).
    async fn recover_once(&self, restore: &super::ini::Candidate) -> Recovered {
        let (Some(router), Some(ini)) = (self.router.as_ref(), self.ini.clone()) else {
            return Recovered::Unconfirmed;
        };
        let ticket = self.authority.recovery_ticket();
        let Ok(hold) = self.acquire_recovery(RECOVERY_REASON, RECOVERY_LEASE).await else {
            return if self.still_ours(restore).await {
                Recovered::Stuck
            } else {
                Recovered::Unconfirmed
            };
        };
        let token = hold.token();
        let mut done = self.settle_engine(&hold, token.as_deref()).await;
        let mut not_ours = false;
        if done {
            if self.still_ours(restore).await {
                done = ini
                    .settle_restore(restore, Duration::from_secs(1), hold.write_fence())
                    .await;
            } else {
                not_ours = true;
            }
        }
        if done {
            done = matches!(self.router_reload(router, &hold, None).await, Ok(Ok(r)) if r.ok)
                && hold.excluded();
        }
        if done {
            if let Some(t) = ticket.as_deref() {
                done = (hold.gated_now()
                    || match hold.recovery_proof() {
                        Some((id, generation)) => {
                            self.node.complete_recovery(t, id, generation).await
                        }
                        None => false,
                    })
                    && hold.excluded()
                    && self.authority.complete_recovery(t);
            }
        }
        if !done {
            let t = self.authority.require_recovery();
            if !hold.gated_now() {
                let _ = self.node.recovery_barrier(true, &t).await;
            }
        }
        hold.release().await;
        match (done, not_ours) {
            (true, false) => Recovered::Restored,
            (true, true) => Recovered::NotOurs,
            (false, _) if self.still_ours(restore).await => Recovered::Stuck,
            (false, _) => Recovered::Unconfirmed,
        }
    }

    /// The recovery's settle step (llamacpp-manager.cjs `setRecoverySettle`): every model the router
    /// lists as running is asked to unload, until it lists none, within [`SETTLE_LIMIT`].
    async fn settle_engine(&self, hold: &Hold, token: Option<&str>) -> bool {
        let Some(router) = self.router.as_ref() else {
            return false;
        };
        let deadline = tokio::time::Instant::now() + SETTLE_LIMIT;
        loop {
            if !hold.excluded() {
                return false;
            }
            let rows = match router.models(token).await {
                Ok(r) if r.ok => super::router::rows(&r.body).cloned(),
                _ => None,
            };
            let Some(rows) = rows else {
                return false;
            };
            let live: Vec<(String, String)> = rows
                .iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_owned();
                    let status = m.get("status")?.get("value")?.as_str()?.to_owned();
                    matches!(
                        status.as_str(),
                        "loaded" | "loading" | "sleeping" | "unloading"
                    )
                    .then_some((id, status))
                })
                .collect();
            if live.is_empty() {
                return true;
            }
            for (id, status) in &live {
                if matches!(status.as_str(), "loaded" | "sleeping") {
                    let _ = router.unload(id, token).await;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    async fn apply_held<F, Fut>(
        &self,
        hold: &Hold,
        body: &JValue,
        budget: F,
        lost: &mut Option<super::ini::Candidate>,
    ) -> Answer
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
        let Ok(first) = self.preset_listing(h).await else {
            return internal();
        };
        if first.ok {
            let data = first.body.as_ref().map_or(&UNDEFINED, |b| b.get("data"));
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
        let Ok(second) = self.preset_listing(h).await else {
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
        let Some(model) = model
            .filter(|m| llamacpp_presets::model_name_ok(&m.encode_utf16().collect::<Vec<u16>>()))
        else {
            return thrown(Some(400), "Invalid preset model name");
        };
        let Some(options) = options else {
            return thrown(Some(400), "Preset options are required");
        };
        let (file, m) = (ini.file().to_path_buf(), model.clone());
        let candidate =
            match blocking(move || super::ini::prepare(&file, &m, &base, &options, limits)).await {
                Some(Ok(c)) => c,
                Some(Err(t)) => return thrown(t.status, &t.message),
                None => return internal(),
            };
        // The write and its re-read are one step; it starts only while the hold covers it.
        if !hold.may_start(super::calibration::PRESET_STEP) {
            return thrown(Some(503), "");
        }
        if let Err(e) = ini.commit_reconciled(&candidate, true).await {
            // A settled Model Loader outcome carries its own message (Node's `publicMessage`);
            // nothing was reloaded. Any other failure is answered as Node's thrown error.
            if e.status >= 500 && e.public() {
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
                let read =
                    blocking(move || read_presets(&file).map(|p| saved_profile(&p, &m))).await;
                if let Some(Ok(body)) = read {
                    return Answer { status: 200, body };
                }
                // `presets.get` threw inside the JS's try: its catch restores, as below.
            }
            // The router was not asked: the hold can no longer cover the re-read. Lost (it excludes
            // no one now): the recovery (see the module documentation). Still held: the restore
            // below, under the hold.
            Ok(Err(_)) if !hold.excluded() => {
                *lost = Some(restore);
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
    pub(crate) async fn preset_refusal<F, Fut>(
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
        match llamacpp_presets::micro_batch_exceeds(
            units("ubatch-size").as_deref(),
            Some(batch.as_slice()),
        ) {
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
        let total = est.get("totalGib").as_num().filter(|n| n.is_finite());
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
        assert!(
            size_knob_problem(&o(&[("batch-size", "x"), ("ctx-size", "y")]))
                .is_some_and(|p| p.starts_with("ctx-size"))
        );
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
    fn only_a_settled_loader_outcome_is_shown_on_a_5xx() {
        let e = |status: u16, message: &str| super::super::ini::IniError {
            status,
            message: message.to_owned(),
            uncertain: false,
            retryable: false,
        };
        // A read error of the file inside the commit (the OS text): Node's bare fs error.
        assert!(!e(500, "No such file or directory (os error 2)").public());
        assert!(!e(500, "Internal error").public());
        assert!(e(
            503,
            "Model Loader rejected the token; check MODEL_LOADER_TOKEN. Nothing was changed."
        )
        .public());
        assert!(e(400, "Model Loader rejected the preset file: x").public());
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
