//! The #697 runtime safety net, moved from Core's `inference-budget-watch.cjs` (`createInferenceBudgetWatch`,
//! wired in `index.cjs`) with its behaviour unchanged. `rust-budget-watch-v2` in `FEATURES`; Core's
//! `rust-budget-watch.cjs` `BUDGET_WATCH_OWNER` says that Core no longer starts its own, and the release
//! assembly and the web supervisor refuse a pair where only one side moved (never two watches, never none).
//!
//! Every interval it reads GPU memory in use (GTT plus the small VRAM carve-out, from sysfs) and the main engine
//! container's anonymous memory (through the model manager), and when the sum stays above the inference budget
//! by more than the allowed overshoot for `strikes_needed` readings, it unloads the loaded models through the
//! router. Its one log line carries numbers and model ids only, under the same `[inference-budget] ` label and
//! in the same JSON shape as Core's.
//!
//! - Serial sampling: one tick at a time; a tick asked for while one runs answers the last state (Node's
//!   `running` flag), and the timer skips the instants a long tick covered (`setInterval` over a busy tick).
//! - The idle baseline (subtracted from the device-wide GPU reading) is taken only from a *known* empty listing
//!   and only while nothing holds the maintenance gate (#873, #874). An unknown listing (the engine failed or
//!   answered no array) skips the tick and keeps the strikes.
//! - The unload is the watchdog's emergency unload (`modelManager.emergencyUnload`): with the gate's admin token
//!   a forced unload that may end a request in progress, without one a plain unload the gate refuses for a model
//!   in use. Like Node's, it takes no maintenance lease and does not wait behind maintenance (memory is running
//!   out); its exclusion is the inference gate's own scheduling, the one every calibration, auto-tune, preset
//!   reload and chat admission of every client goes through. An unload whose request did not settle counts as
//!   done, as in Node (`r?.ok !== false`).
//! - Each model it unloaded goes to Core's quarantine with the budget (`modelManager.quarantine`, over the
//!   loopback bridge `/internal/budget-watch`): the load guard that refuses it until its preset revision or the
//!   budget changes is Core's. A shared fsynced admission fence is installed before unloading, retained
//!   until an explicit revision/budget invalidation, and consulted by both Core and native maintenance
//!   loads. Quarantine IPC must answer ok:true; failures are retried with 250 ms–30 s capped backoff.
//!   The tick keeps its strikes and does not start cooldown or publish completion until acknowledged.
//!
//! The budget is the one Core enforces (`/internal/models/facts` `{want:'budget'}`, as the model state reads use).
//! Node read it in process; here the last answer stands while Node does not answer, so a Node restart does not
//! blind the watchdog (before the first answer there is no budget, the tick is `no-budget`).

pub(super) mod journal;

use super::{gate::Gate, router::Router, Maint};
use crate::project_store::TOKEN_HEADER;
use js_json::JValue;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The log label (`log('[inference-budget] ' + JSON.stringify(...))`).
pub const LOG_PREFIX: &str = "[inference-budget] ";
/// The Core bridge for what only Core has.
pub const BRIDGE_PATH: &str = "/internal/budget-watch";
/// The Core facts call the budget comes from.
pub const FACTS_PATH: &str = "/internal/models/facts";
const GIB: f64 = 1_073_741_824.0;
/// `engineReaderFromModelLoader`: `fetchJson(..., 4000)`.
const LOADER_TIMEOUT: Duration = Duration::from_millis(4000);
/// `emergencyUnload`: `forcedUnload(model, 60000)` / `post('/models/unload', { model }, 60000)`.
const UNLOAD_TIMEOUT: Duration = Duration::from_millis(60_000);
/// Each loopback call to Core (the model state reads' budget deadline).
const NODE_TIMEOUT: Duration = Duration::from_secs(5);
/// Node's longest timer (`TIMEOUT_MAX`).
const MAX_INTERVAL_MS: f64 = 2_147_483_647.0;

// ── JS semantics the watch depends on ──────────────────────────────────────────

/// `String.prototype.trim`: WhiteSpace and LineTerminator (U+FEFF yes, U+0085 no).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// `Number(string)`.
fn js_number_str(s: &str) -> f64 {
    llamacpp_autoconfig::string_to_number(&s.encode_utf16().collect::<Vec<_>>()).unwrap_or(f64::NAN)
}

/// `Number(value)` over a parsed JSON value (`undefined` is a missing key: NaN).
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        None => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => js_number_str(s),
        // `Number([])` is 0, `Number([x])` is `Number(String(x))`; anything longer, or an object, is NaN.
        Some(v @ Value::Array(_)) => js_number_str(&js_string(v)),
        Some(Value::Object(_)) => f64::NAN,
    }
}

/// `String(value)` for the primitives a name can be.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_json::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(_) | Value::Object(_) => {
            js_json::to_js_string(&to_jvalue(v)).unwrap_or_default()
        }
    }
}

/// JS truthiness of a parsed JSON value (`undefined` is a missing key).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `v?.[key]` (a primitive has no such key; `null`/`undefined` stop the chain).
fn field<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v?.as_object()?.get(key)
}

/// `a ?? b`.
fn nullish_or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        None | Some(Value::Null) => b,
        some => some,
    }
}

/// `Math.round(n * 100) / 100` (ties toward +Infinity, as `Math.round`).
pub fn round2(n: f64) -> f64 {
    let x = n * 100.0;
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    r / 100.0
}

/// A parsed JSON value as `JSON.stringify` writes it.
pub fn to_jvalue(v: &Value) -> JValue {
    match v {
        Value::Null => JValue::Null,
        Value::Bool(b) => JValue::Bool(*b),
        Value::Number(n) => JValue::Num(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => JValue::Str(s.clone()),
        Value::Array(a) => JValue::Arr(a.iter().map(to_jvalue).collect()),
        Value::Object(o) => JValue::obj(o.iter().map(|(k, v)| (k.clone(), to_jvalue(v)))),
    }
}

// ── Settings ───────────────────────────────────────────────────────────────────

/// What `createInferenceBudgetWatch` reads from the environment and its index.cjs defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `!off(INFERENCE_BUDGET_WATCH)`.
    pub enabled: bool,
    /// `max(2000, INFERENCE_BUDGET_WATCH_INTERVAL_MS || 15000)`.
    pub every: Duration,
    /// `min(100, INFERENCE_BUDGET_OVERSHOOT_PCT || 10) / 100`.
    pub overshoot: f64,
    pub strikes_needed: u32,
    pub cooldown_ms: f64,
}

/// `num(v, d)`: `Number(v)` when finite and not negative, else `d` (`undefined` is NaN).
fn num(v: Option<String>, d: f64) -> f64 {
    match v {
        None => d,
        Some(s) => {
            let n = js_number_str(&s);
            if n.is_finite() && n >= 0.0 {
                n
            } else {
                d
            }
        }
    }
}

/// `off(v)`: `/^(0|false|off|no)$/i` over the trimmed text.
fn off(v: Option<&str>) -> bool {
    let t = js_trim(v.unwrap_or("")).to_ascii_lowercase();
    matches!(t.as_str(), "0" | "false" | "off" | "no")
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        let every = num(get("INFERENCE_BUDGET_WATCH_INTERVAL_MS"), 15000.0).max(2000.0);
        let overshoot = num(get("INFERENCE_BUDGET_OVERSHOOT_PCT"), 10.0).min(100.0) / 100.0;
        Settings {
            enabled: !off(get("INFERENCE_BUDGET_WATCH").as_deref()),
            // Node truncates timer milliseconds; overflow becomes a one-millisecond interval.
            every: Duration::from_millis(if every > MAX_INTERVAL_MS {
                1
            } else {
                every.trunc() as u64
            }),
            overshoot,
            strikes_needed: 2,
            cooldown_ms: 60000.0,
        }
    }
}

// ── Readings ───────────────────────────────────────────────────────────────────

/// `readGpuMemory`'s answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gpu {
    pub gtt_gib: f64,
    pub vram_gib: f64,
}

/// `engineReaderFromModelLoader`'s answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Engine {
    pub container_gib: f64,
    pub gtt_gib: f64,
    pub vram_gib: f64,
}

/// The GPU memory files to sum (`gpuMemoryFiles`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuFiles {
    pub gtt: Vec<PathBuf>,
    pub vram: Vec<PathBuf>,
}

/// `gpuMemoryFiles(env, { drmRoot })`: `INFERENCE_GTT_USED_PATH` (comma-separated) or every DRM card that has a
/// GTT counter, each with the VRAM counter beside it when present. Taken once, at start, as Node does.
pub fn gpu_memory_files(get: &impl Fn(&str) -> Option<String>, drm_root: &Path) -> GpuFiles {
    let explicit: Vec<PathBuf> = get("INFERENCE_GTT_USED_PATH")
        .unwrap_or_default()
        .split(',')
        .map(js_trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    let gtt = if explicit.is_empty() {
        match std::fs::read_dir(drm_root) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| {
                        n.strip_prefix("card")
                            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
                    })
                    .collect();
                names.sort();
                names
                    .into_iter()
                    .map(|n| drm_root.join(n).join("device").join("mem_info_gtt_used"))
                    .filter(|f| f.exists())
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    } else {
        explicit
    };
    let vram = gtt
        .iter()
        .map(|f| {
            f.parent()
                .unwrap_or_else(|| Path::new(""))
                .join("mem_info_vram_used")
        })
        .filter(|f| f.exists())
        .collect();
    GpuFiles { gtt, vram }
}

/// One counter: `Number(String(text).trim())` when finite and not negative.
fn counter(text: &str) -> Option<f64> {
    let n = js_number_str(js_trim(text));
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// `readGpuMemory(files)`: bytes in use per counter, or `None` when no GTT counter can be read.
pub async fn read_gpu_memory(files: &GpuFiles) -> Option<Gpu> {
    async fn read(list: &[PathBuf]) -> Vec<f64> {
        let mut out = Vec::new();
        for f in list {
            if let Ok(bytes) = tokio::fs::read(f).await {
                if let Some(n) = counter(&String::from_utf8_lossy(&bytes)) {
                    out.push(n);
                }
            }
        }
        out
    }
    let gtt = read(&files.gtt).await;
    if gtt.is_empty() {
        return None;
    }
    let vram = read(&files.vram).await;
    Some(Gpu {
        gtt_gib: gtt.iter().fold(0.0, |a, b| a + b) / GIB,
        vram_gib: vram.iter().fold(0.0, |a, b| a + b) / GIB,
    })
}

/// `isEngineContainer(name, env)`: one of `INFERENCE_ENGINE_CONTAINER`, or the compose `llama` service's
/// container (`llama`, `<project>-llama-1`, `<project>_llama_1`), not embed/rerank.
pub fn is_engine_container(name: &str, configured: &[String]) -> bool {
    if !configured.is_empty() {
        return configured.iter().any(|c| c == name);
    }
    // /(^|[-_])llama([-_]\d+)?$/
    let tail = |rest: &str| -> bool {
        rest.is_empty()
            || rest
                .strip_prefix(['-', '_'])
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    name.match_indices("llama").any(|(i, _)| {
        let before_ok =
            i == 0 || matches!(name.as_bytes().get(i.wrapping_sub(1)), Some(b'-' | b'_'));
        before_ok && tail(&name[i + 5..])
    })
}

/// The engine reading from a model-manager `/api/v1/backends` answer (`engineReaderFromModelLoader`'s body).
pub fn engine_from_backends(
    ok: bool,
    body: Option<&Value>,
    configured: &[String],
) -> Option<Engine> {
    let list = if ok {
        field(body, "backends").and_then(Value::as_array)
    } else {
        None
    };
    let backends: Vec<&Value> = list
        .map(|l| {
            l.iter()
                .filter(|b| truthy(field(field(Some(*b), "stats"), "ok")))
                .filter(|b| {
                    let name = field(Some(*b), "name");
                    let name = if truthy(name) {
                        name.map(js_string).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    is_engine_container(&name, configured)
                })
                .collect()
        })
        .unwrap_or_default();
    if backends.is_empty() {
        return None;
    }
    let sum = |pick: &dyn Fn(Option<&Value>) -> Option<&Value>| -> f64 {
        backends.iter().fold(0.0, |a, b| {
            let n = js_number(pick(field(Some(*b), "stats")));
            a + if n == 0.0 || n.is_nan() { 0.0 } else { n }
        })
    };
    Some(Engine {
        container_gib: sum(&|s| {
            let c = field(s, "container");
            nullish_or(field(c, "mem_anon_gb"), field(c, "mem_used_gb"))
        }),
        gtt_gib: sum(&|s| field(field(s, "gpu"), "shared_used_gb")),
        vram_gib: sum(&|s| field(field(s, "gpu"), "vram_used_gb")),
    })
}

/// `loadedFromListing(await modelManager.listModels())`: the loaded (or loading) model ids, `None` when the
/// listing failed, was no array, or had a row the manager's mapping throws on (unknown, #874).
pub fn loaded_from_listing(ok: bool, body: Option<&Value>) -> Option<Vec<Value>> {
    if !ok {
        return None;
    }
    let rows = field(body, "data")?.as_array()?;
    // `model.status?.value` on a null row throws inside listModels: the whole listing is unknown.
    if rows.iter().any(Value::is_null) {
        return None;
    }
    Some(
        rows.iter()
            .filter(|m| {
                matches!(
                    field(field(Some(*m), "status"), "value").and_then(Value::as_str),
                    Some("loaded" | "loading")
                )
            })
            .map(|m| field(Some(m), "id").cloned().unwrap_or(Value::Null))
            .collect(),
    )
}

// ── The watch ──────────────────────────────────────────────────────────────────

/// What a tick measured (`sample` in Node, numbers rounded to two places but the budget).
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub used_gib: f64,
    pub gtt_gib: f64,
    pub vram_gib: f64,
    pub baseline_gib: f64,
    pub container_gib: f64,
    pub budget_gib: f64,
    pub limit_gib: f64,
}

impl Sample {
    fn pairs(&self) -> Vec<(&'static str, JValue)> {
        vec![
            ("usedGib", JValue::Num(self.used_gib)),
            ("gttGib", JValue::Num(self.gtt_gib)),
            ("vramGib", JValue::Num(self.vram_gib)),
            ("baselineGib", JValue::Num(self.baseline_gib)),
            ("containerGib", JValue::Num(self.container_gib)),
            ("budgetGib", JValue::Num(self.budget_gib)),
            ("limitGib", JValue::Num(self.limit_gib)),
        ]
    }
}

/// A tick's answer (`last`).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    NoBudget,
    Unavailable,
    ModelsUnknown { strikes: u32 },
    Ok(Sample),
    Over { strikes: u32, sample: Sample },
    Unloaded { models: Vec<Value>, sample: Sample },
}

impl Status {
    pub fn state(&self) -> &'static str {
        match self {
            Status::NoBudget => "no-budget",
            Status::Unavailable => "unavailable",
            Status::ModelsUnknown { .. } => "models-unknown",
            Status::Ok(_) => "ok",
            Status::Over { .. } => "over",
            Status::Unloaded { .. } => "unloaded",
        }
    }

    /// The object Node's `tick()` resolves to, keys in its order.
    pub fn to_json(&self) -> JValue {
        let mut pairs: Vec<(&str, JValue)> = vec![("state", JValue::Str(self.state().into()))];
        match self {
            Status::NoBudget | Status::Unavailable => {}
            Status::ModelsUnknown { strikes } => {
                pairs.push(("strikes", JValue::Num(f64::from(*strikes))))
            }
            Status::Ok(s) => pairs.extend(s.pairs()),
            Status::Over { strikes, sample } => {
                pairs.push(("strikes", JValue::Num(f64::from(*strikes))));
                pairs.extend(sample.pairs());
            }
            Status::Unloaded { models, sample } => {
                pairs.push((
                    "models",
                    JValue::Arr(models.iter().map(to_jvalue).collect()),
                ));
                pairs.extend(sample.pairs());
            }
        }
        JValue::obj(pairs)
    }
}

/// What the watch reads and does; [`NativeIo`] in production, a fake in tests. Each read that fails answers
/// `None` (Node's `.catch(() => null)`).
pub trait Io: Send + Sync {
    /// `Number(budgetGib())`; `None` when there is none to read.
    fn budget(&self) -> impl Future<Output = Option<f64>> + Send;
    fn read_gpu(&self) -> impl Future<Output = Option<Gpu>> + Send;
    fn read_engine(&self) -> impl Future<Output = Option<Engine>> + Send;
    fn list_loaded(&self) -> impl Future<Output = Option<Vec<Value>>> + Send;
    /// `baselineAllowed() !== false`, false when it cannot be told.
    fn baseline_allowed(&self) -> impl Future<Output = bool> + Send;
    /// The emergency unload: `Some(ok)` when the request settled, `None` when it did not.
    fn unload(&self, model: &Value, budget_gib: f64) -> impl Future<Output = Option<bool>> + Send;
    /// Require acknowledgement of the exact revision/budget obligation before completing a tick.
    fn quarantine(&self, model: &Value, budget_gib: f64) -> impl Future<Output = bool> + Send;
    fn retry_pending(&self, _budget: f64) -> impl Future<Output = bool> + Send {
        async { true }
    }
    fn log(&self, line: &str);
    /// Milliseconds since the epoch (`Date.now()` for the cooldown).
    fn now_ms(&self) -> f64;
}

#[derive(Debug, Default)]
struct State {
    strikes: u32,
    cooldown_until: f64,
    baseline_gib: Option<f64>,
    pending: Option<(Vec<Value>, Vec<Value>, Sample)>,
}

/// `createInferenceBudgetWatch`: the strikes, the idle baseline and the cooldown live here, in memory, as in Node
/// (a restart starts again from none).
pub struct Watch<I: Io> {
    pub settings: Settings,
    io: I,
    /// Held for the whole tick: the `running` flag, released however the tick ends (a dropped tick too).
    state: tokio::sync::Mutex<State>,
    last: Mutex<Option<Status>>,
}

impl<I: Io> Watch<I> {
    pub fn new(settings: Settings, io: I) -> Watch<I> {
        Watch {
            settings,
            io,
            state: tokio::sync::Mutex::new(State::default()),
            last: Mutex::new(None),
        }
    }

    pub fn io(&self) -> &I {
        &self.io
    }

    /// `status()`.
    pub fn status(&self) -> Option<Status> {
        self.last.lock().ok().and_then(|l| l.clone())
    }

    fn remember(&self, status: Status) -> Option<Status> {
        if let Ok(mut l) = self.last.lock() {
            *l = Some(status.clone());
        }
        Some(status)
    }

    /// One reading. While another runs, the last answer (`None` before the first), as `if (running) return last`.
    pub async fn tick(&self) -> Option<Status> {
        let Ok(mut st) = self.state.try_lock() else {
            return self.status();
        };
        let budget = self.io.budget().await.unwrap_or(f64::NAN);
        let reconciled = self.io.retry_pending(budget).await;
        if !reconciled {
            return self.status();
        }
        if let Some((unloaded, failed, sample)) = st.pending.take() {
            return self.complete(&mut st, unloaded, failed, sample).await;
        }
        if budget.is_nan() || budget <= 0.0 {
            return self.remember(Status::NoBudget);
        }
        let (gpu, engine, loaded) = tokio::join!(
            self.io.read_gpu(),
            self.io.read_engine(),
            self.io.list_loaded()
        );
        // Neither source readable: nothing measured, nothing to act on.
        if gpu.is_none() && engine.is_none() {
            st.strikes = 0;
            return self.remember(Status::Unavailable);
        }
        // #874: an unknown model list is not "idle"; the tick is skipped and the strikes carry over.
        let Some(loaded) = loaded else {
            return self.remember(Status::ModelsUnknown {
                strikes: st.strikes,
            });
        };
        let gtt_gib = gpu
            .map(|g| g.gtt_gib)
            .or(engine.map(|e| e.gtt_gib))
            .unwrap_or(0.0);
        let vram_gib = gpu
            .map(|g| g.vram_gib)
            .or(engine.map(|e| e.vram_gib))
            .unwrap_or(0.0);
        // The idle baseline: every reading with no model loaded, while nothing holds the maintenance gate.
        if loaded.is_empty() && self.io.baseline_allowed().await {
            st.baseline_gib = Some(gtt_gib + vram_gib);
        }
        let gpu_gib = (gtt_gib + vram_gib - st.baseline_gib.unwrap_or(0.0)).max(0.0);
        let container_gib = engine.map(|e| e.container_gib).unwrap_or(0.0);
        let used_gib = gpu_gib + container_gib;
        let limit_gib = budget * (1.0 + self.settings.overshoot);
        let sample = Sample {
            used_gib: round2(used_gib),
            gtt_gib: round2(gtt_gib),
            vram_gib: round2(vram_gib),
            baseline_gib: round2(st.baseline_gib.unwrap_or(0.0)),
            container_gib: round2(container_gib),
            budget_gib: budget,
            limit_gib: round2(limit_gib),
        };
        if used_gib <= limit_gib || loaded.is_empty() {
            st.strikes = 0;
            return self.remember(Status::Ok(sample));
        }
        st.strikes += 1;
        if st.strikes < self.settings.strikes_needed || self.io.now_ms() < st.cooldown_until {
            return self.remember(Status::Over {
                strikes: st.strikes,
                sample,
            });
        }
        let mut unloaded: Vec<Value> = Vec::new();
        for model in &loaded {
            if self.io.unload(model, budget).await != Some(false) {
                unloaded.push(model.clone());
            }
        }
        let failed = loaded
            .iter()
            .filter(|m| !unloaded.contains(m))
            .cloned()
            .collect();
        self.complete(&mut st, unloaded, failed, sample).await
    }

    async fn complete(
        &self,
        st: &mut State,
        unloaded: Vec<Value>,
        failed: Vec<Value>,
        sample: Sample,
    ) -> Option<Status> {
        let mut acknowledged = true;
        for model in &unloaded {
            acknowledged &= self.io.quarantine(model, sample.budget_gib).await;
        }
        if !acknowledged {
            st.pending = Some((unloaded, failed, sample.clone()));
            return self.remember(Status::Over {
                strikes: st.strikes,
                sample,
            });
        }
        st.strikes = 0;
        st.cooldown_until = self.io.now_ms() + self.settings.cooldown_ms;
        let mut pairs: Vec<(&str, JValue)> = vec![(
            "event",
            JValue::Str(
                if unloaded.is_empty() && failed.is_empty() {
                    "over-budget-nothing-loaded"
                } else {
                    "unload"
                }
                .into(),
            ),
        )];
        pairs.extend(sample.pairs());
        pairs.push((
            "models",
            JValue::Arr(unloaded.iter().map(to_jvalue).collect()),
        ));
        pairs.push((
            "failed",
            JValue::Arr(failed.iter().map(to_jvalue).collect()),
        ));
        let line = js_json::stringify(&JValue::obj(pairs)).unwrap_or_default();
        self.io.log(&format!("{LOG_PREFIX}{line}"));
        self.remember(Status::Unloaded {
            models: unloaded,
            sample,
        })
    }

    /// The timer (`start()`): the first reading one interval after start, then on every multiple of the
    /// interval, skipping the instants a long reading covered (as `setInterval` over a busy tick). Ends when `stop` turns true (or its sender is gone); a reading in
    /// progress then is dropped where it stands, as Node's ends with its process.
    pub async fn run(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) {
        if !self.settings.enabled || *stop.borrow() {
            return;
        }
        let start = tokio::time::Instant::now();
        let every = self.settings.every;
        let mut k: u32 = 1;
        loop {
            let at = start + every.saturating_mul(k);
            tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => return,
                () = tokio::time::sleep_until(at) => {}
            }
            tokio::select! {
                biased;
                _ = stop.wait_for(|s| *s) => return,
                _ = self.tick() => {}
            }
            // setInterval kept firing while the reading ran, and each of those calls answered the last state
            // without reading: the next reading is at the first instant of the schedule after now.
            let elapsed = start.elapsed().as_nanos();
            let next = elapsed / every.as_nanos().max(1) + 1;
            k = u32::try_from(next)
                .unwrap_or(u32::MAX)
                .max(k.saturating_add(1));
        }
    }
}

// ── The production readings and actions ────────────────────────────────────────

/// The front's loopback calls to Core (the per-start secret, as the maintenance lease's).
#[derive(Clone)]
pub struct NodeBridge {
    client: reqwest::Client,
    base: String,
    token: Option<HeaderValue>,
}

impl std::fmt::Debug for NodeBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeBridge")
            .field("base", &self.base)
            .field("token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl NodeBridge {
    pub fn new(base: Option<&str>, token: Option<&str>) -> Result<NodeBridge, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("budget watch bridge client: {e}"))?;
        let token = token
            .filter(|t| !t.is_empty())
            .and_then(|t| HeaderValue::from_str(t).ok())
            .map(|mut t| {
                t.set_sensitive(true);
                t
            });
        Ok(NodeBridge {
            client,
            base: base.unwrap_or("").trim_end_matches('/').to_owned(),
            token,
        })
    }

    /// A 200 answer's object, or `None`.
    async fn call(&self, path: &str, body: &Value) -> Option<serde_json::Map<String, Value>> {
        if self.base.is_empty() {
            return None;
        }
        let token = self.token.clone()?;
        let res = self
            .client
            .post(format!("{}{path}", self.base))
            .header(HeaderName::from_static(TOKEN_HEADER), token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(NODE_TIMEOUT)
            .body(js_json::stringify(&to_jvalue(body))?)
            .send()
            .await
            .ok()?;
        if res.status().as_u16() != 200 {
            return None;
        }
        let bytes = res.bytes().await.ok()?;
        if bytes.len() > 64 * 1024 {
            return None;
        }
        match serde_json::from_slice::<Value>(&bytes).ok()? {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// `inferenceBudget.budgetGib()`, as Core enforces it.
    pub async fn budget(&self) -> Option<f64> {
        let o = self
            .call(FACTS_PATH, &serde_json::json!({ "want": "budget" }))
            .await?;
        Some(js_number(field(o.get("budget"), "budgetGib")))
    }

    /// `modelManager.maintenanceHeld()`.
    pub async fn held(&self) -> Option<bool> {
        let o = self
            .call(BRIDGE_PATH, &serde_json::json!({ "want": "held" }))
            .await?;
        o.get("held").and_then(Value::as_bool)
    }

    /// `modelManager.quarantine(model, budgetGib)`. True when Core recorded it.
    pub async fn quarantine(&self, entry: &journal::Entry) -> Option<bool> {
        self.call(
            BRIDGE_PATH,
            &serde_json::json!({ "want": "quarantine", "model": entry.model, "budgetGib": entry.budget_gib, "revision": entry.revision }),
        )
        .await
        .filter(|o| o.get("ok").and_then(Value::as_bool) == Some(true))
        .map(|o| o.get("invalidated").and_then(Value::as_bool) == Some(true))
    }
}

/// The model manager's backend telemetry (`MODEL_LOADER_URL` `/api/v1/backends`).
#[derive(Debug, Clone)]
pub struct Loader {
    router: Router,
    token: Option<HeaderValue>,
    containers: Vec<String>,
}

/// The production readings: sysfs, the model manager, the router through the gate, and Core for the budget,
/// the maintenance state and the quarantine.
pub struct NativeIo {
    maint: Arc<Maint>,
    router: Router,
    gate: Option<Arc<Gate>>,
    files: GpuFiles,
    loader: Option<Loader>,
    node: NodeBridge,
    budget: Mutex<Option<f64>>,
    journal_dir: Option<PathBuf>,
    retries: Mutex<std::collections::HashMap<String, (tokio::time::Instant, u32)>>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    clock: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl NativeIo {
    /// `None` unless the engine is llama.cpp with a router (index.cjs: `modelManager.kind === 'llamacpp'`).
    pub fn from_lookup(
        maint: Arc<Maint>,
        get: &impl Fn(&str) -> Option<String>,
        drm_root: &Path,
    ) -> Result<Option<NativeIo>, String> {
        if maint.kind != server_stats::Kind::LlamaCpp {
            return Ok(None);
        }
        let Some(router) = maint.router.clone() else {
            return Ok(None);
        };
        let containers: Vec<String> = get("INFERENCE_ENGINE_CONTAINER")
            .unwrap_or_default()
            .split(',')
            .map(js_trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        // `if (!env.MODEL_LOADER_URL) return null`.
        let loader = match get("MODEL_LOADER_URL").filter(|u| !u.is_empty()) {
            Some(url) => Some(Loader {
                router: Router::new(url.trim_end_matches('/'), None)?,
                token: get("MODEL_LOADER_TOKEN")
                    .filter(|t| !t.is_empty())
                    .and_then(|t| HeaderValue::from_str(&t).ok())
                    .map(|mut t| {
                        t.set_sensitive(true);
                        t
                    }),
                containers,
            }),
            None => None,
        };
        let node = NodeBridge::new(
            get("NOEVIA_LEGACY_UPSTREAM")
                .filter(|v| !v.is_empty())
                .as_deref(),
            get("NOEVIA_PROJECT_STORE_TOKEN").as_deref(),
        )?;
        Ok(Some(NativeIo {
            gate: maint.gate.clone(),
            maint: Arc::clone(&maint),
            router,
            files: gpu_memory_files(get, drm_root),
            loader,
            node,
            budget: Mutex::new(None),
            journal_dir: maint
                .data_dir
                .as_ref()
                .map(|d| d.join(format!("budget-watch-{}", maint.suffix))),
            retries: Mutex::new(std::collections::HashMap::new()),
            // console.warn.
            log: Arc::new(|line: &str| eprintln!("{line}")),
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0)
            }),
        }))
    }

    pub fn files(&self) -> &GpuFiles {
        &self.files
    }

    /// Where the log line goes (stderr by default; a test's sink).
    pub fn with_log(mut self, log: Arc<dyn Fn(&str) + Send + Sync>) -> NativeIo {
        self.log = log;
        self
    }

    /// The cooldown's clock in milliseconds (wall clock by default, matching Node).
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> f64 + Send + Sync>) -> NativeIo {
        self.clock = clock;
        self
    }
}

impl NativeIo {
    fn revision(&self) -> Option<Option<String>> {
        match &self.maint.ini {
            Some(ini) => Some(Some(ini.snapshot().ok()?.revision)),
            None => Some(None),
        }
    }

    async fn acknowledge(&self, mut entry: journal::Entry) -> bool {
        let Some(dir) = &self.journal_dir else {
            return false;
        };
        let was_recorded = entry.recorded;
        let now = tokio::time::Instant::now();
        if self
            .retries
            .lock()
            .ok()
            .and_then(|r| r.get(&entry.model).copied())
            .is_some_and(|(at, _)| now < at)
        {
            return entry.recorded;
        }
        if let Some(invalidated) = self.node.quarantine(&entry).await {
            let saved = if invalidated {
                journal::remove(dir, &entry.model)
            } else {
                entry.recorded = true;
                journal::save(dir, &entry)
            };
            if saved.is_ok() {
                if let Ok(mut r) = self.retries.lock() {
                    r.remove(&entry.model);
                }
                return true;
            }
        }
        if let Ok(mut retries) = self.retries.lock() {
            let attempt = retries.get(&entry.model).map_or(0, |(_, n)| *n);
            let delay =
                Duration::from_millis((250u64.saturating_mul(1u64 << attempt.min(7))).min(30_000));
            retries.insert(
                entry.model.clone(),
                (
                    tokio::time::Instant::now() + delay,
                    attempt.saturating_add(1),
                ),
            );
        }
        was_recorded
    }
}

impl Io for NativeIo {
    async fn budget(&self) -> Option<f64> {
        let fresh = self.node.budget().await;
        let mut cached = self.budget.lock().ok()?;
        if fresh.is_some() {
            *cached = fresh;
        }
        *cached
    }

    async fn read_gpu(&self) -> Option<Gpu> {
        read_gpu_memory(&self.files).await
    }

    async fn read_engine(&self) -> Option<Engine> {
        let loader = self.loader.as_ref()?;
        let extra: Vec<(HeaderName, HeaderValue)> = loader
            .token
            .iter()
            .map(|t| (HeaderName::from_static("x-model-loader-token"), t.clone()))
            .collect();
        let r = loader
            .router
            .request_with(
                reqwest::Method::GET,
                "/api/v1/backends",
                None,
                None,
                &extra,
                LOADER_TIMEOUT,
            )
            .await
            .ok()?;
        engine_from_backends(r.ok, r.body.as_ref(), &loader.containers)
    }

    async fn list_loaded(&self) -> Option<Vec<Value>> {
        let r = self.router.models(None).await.ok()?;
        loaded_from_listing(r.ok, r.body.as_ref())
    }

    async fn baseline_allowed(&self) -> bool {
        // A job of this front holds the engine (its lease or hold Core sees too, a gate hold only after Core's
        // next 3 s look): no baseline either way.
        if self.maint.local_held() {
            return false;
        }
        self.node.held().await == Some(false)
    }

    async fn unload(&self, model: &Value, budget_gib: f64) -> Option<bool> {
        let Some(model_name) = model.as_str() else {
            return Some(false);
        };
        let Some(dir) = &self.journal_dir else {
            return Some(false);
        };
        let Some(revision) = self.revision() else {
            return Some(false);
        };
        let entry = journal::Entry {
            model: model_name.to_owned(),
            revision,
            budget_gib,
            at: self.now_ms(),
            recorded: false,
        };
        // Synchronous atomic replace + file/directory fsync precedes any await that may send the unload.
        // An accepted-but-unanswered unload or a cancelled tick therefore leaves a durable admission fence.
        if journal::save(dir, &entry).is_err() {
            return Some(false);
        }
        let admin = self.gate.as_ref().and_then(|g| g.admin_header());
        let reply = match admin {
            // Deliberate Node emergencyUnload parity: this can preempt another maintenance owner.
            // Coordinated preemption: https://github.com/sbstndalton/noevia/issues/2356.
            // `forcedUnload(model, 60000)`: the admin header and `force` for the gate.
            Some(header) => {
                self.router
                    .request_with(
                        reqwest::Method::POST,
                        "/models/unload",
                        Some(&serde_json::json!({ "model": model, "force": true })),
                        None,
                        &[header],
                        UNLOAD_TIMEOUT,
                    )
                    .await
            }
            // `post('/models/unload', { model }, 60000)`: the gate unloads only an idle model.
            None => {
                self.router
                    .request(
                        reqwest::Method::POST,
                        "/models/unload",
                        Some(&serde_json::json!({ "model": model })),
                        None,
                        UNLOAD_TIMEOUT,
                    )
                    .await
            }
        };
        let result = reply.ok().map(|r| r.ok);
        if result == Some(false) {
            let _ = journal::remove(dir, model_name);
        }
        result
    }

    async fn quarantine(&self, model: &Value, _budget_gib: f64) -> bool {
        let Some(dir) = &self.journal_dir else {
            return false;
        };
        let Some(model) = model.as_str() else {
            return false;
        };
        match journal::read(dir, model) {
            Ok(entry) if entry.recorded => true,
            Ok(entry) => self.acknowledge(entry).await,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        }
    }

    async fn retry_pending(&self, budget: f64) -> bool {
        let Some(dir) = &self.journal_dir else {
            return false;
        };
        let Ok(entries) = journal::entries(dir) else {
            return false;
        };
        let revision = self.revision();
        let mut done = true;
        for entry in entries {
            if !entry.recorded
                || (budget.is_finite() && entry.budget_gib != budget)
                || revision.as_ref().is_some_and(|r| r != &entry.revision)
            {
                done &= self.acknowledge(entry).await;
            }
        }
        done
    }

    fn log(&self, line: &str) {
        (self.log)(line);
    }

    fn now_ms(&self) -> f64 {
        (self.clock)()
    }
}

/// The production watch, when this front runs it: llama.cpp with a router, and not turned off. The timer stops
/// with `stop`. `None` otherwise (Node's `start()` answering false).
pub fn spawn(
    maint: &Arc<Maint>,
    get: &impl Fn(&str) -> Option<String>,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let settings = Settings::from_lookup(get);
    if !settings.enabled {
        return None;
    }
    let io = match NativeIo::from_lookup(Arc::clone(maint), get, Path::new("/sys/class/drm")) {
        Ok(Some(io)) => io,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("noevia-server: the memory watchdog could not start: {e}");
            return None;
        }
    };
    let watch = Arc::new(Watch::new(settings, io));
    Some(tokio::spawn(watch.run(stop)))
}

#[cfg(test)]
mod tests;
