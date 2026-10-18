//! noevia#1318: native read-only `GET /api/v1/sections/{name}/autoconfig`.
//!
//! The Python route (`model-manager/app/api.py`, `section_autoconfig`) checks the system routing
//! role, finds the section's model file, reads its header and size, ingests the backend logs'
//! telemetry and reads the section's measurements, lists the backends, holds them to the memory
//! budget, runs `autoconfig.analyze`, and answers with the file's facts, the recommendation, the
//! measurements and the models.ini revision.
//!
//! Native here: the query and its 422, the routing role check (`models.roles.json` and the
//! name/path rule), the file (the contained models-folder reader of the draft-head read), its
//! header (the `gguf` crate) and size, `budget_backends`, the whole of `analyze()` (every rule
//! through the `model-autoconfig` crate, the projector and draft head beside the model read
//! here), and the JSON, byte for byte.
//!
//! Asked of the Python child, never re-implemented: the live inputs, through the private
//! `POST /api/v1/internal/section-autoconfig-runtime` (only under
//! `NOEVIA_NATIVE_MODEL_MANAGER_SECTION_AUTOCONFIG_CONFIRMED=1`, which also makes the child refuse
//! the public route): the telemetry ingest from the backend logs and the section's measurements
//! (`measured`, `history`, placed verbatim, and the prompt rate the analysis reads), and the
//! backend inventory (`helpers._backend_list`: the GPU sampler, Docker image tags and commands).
//! The child is asked only once the file part found a model, as Python ingests only then.
//!
//! Deliberate refusals (the native 503) where Python cannot be followed exactly are listed in
//! `docs/native-section-autoconfig.md`.
pub(crate) mod analyze;
mod files;
pub mod query;
mod render;

use crate::model_updates::{read_ini, Stop};
use crate::sections::{access::Root, Error as WalkError};
use futures_util::StreamExt;
use model_files::json::{Limits, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// Body of the native refusal (503).
pub const REFUSAL: &str = "Native section autoconfig is unavailable";
/// The child's private call.
pub const RUNTIME_PATH: &str = "/api/v1/internal/section-autoconfig-runtime";
/// Largest child answer read.
pub const RUNTIME_CAP: usize = 16 * 1024 * 1024;
/// Largest answer written (a recommendation is tens of kilobytes).
pub const RESPONSE_CAP: usize = 8 * 1024 * 1024;
/// Concurrent blocking parts (each may hold one model header while it is read).
pub const CONCURRENT_READS: usize = 2;
/// One blocking part, waiting for its slot included.
const DEADLINE: Duration = Duration::from_secs(60);
/// The child's ingest reads up to 20,000 log lines per backend.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A finished answer: status and JSON body.
    Json(u16, Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// The native reader could not answer exactly: the native 503 refusal.
    Refused,
    /// The child could not be asked: the forwarded route's 502.
    Unavailable,
}

/// The settings the route reads, named as the child reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `MODELS_DIR`, absolute and in pathlib's normal form.
    pub models: String,
    /// `MODELS_INI_PATH`.
    pub ini: String,
    /// `settings.cache_ram_limits[0]`, in MiB.
    pub cache_ram_cap_mib: i64,
}

/// How the parent reaches its child.
pub struct Child<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub token: &'a str,
}

pub struct SectionAutoconfig {
    settings: Settings,
    reads: Arc<tokio::sync::Semaphore>,
    open: fn(&Path) -> Result<Root, WalkError>,
}

/// `GET /api/v1/sections/{name}/autoconfig`, the name segment percent-decoded as uvicorn does;
/// a name that decodes to a slash is not this route (FastAPI would not match it).
pub fn route(method: &axum::http::Method, path: &str) -> Option<String> {
    if *method != axum::http::Method::GET {
        return None;
    }
    let seg = path
        .strip_prefix("/api/v1/sections/")?
        .strip_suffix("/autoconfig")?;
    if seg.is_empty() || seg.contains('/') {
        return None;
    }
    let name = crate::lossy_unquote(seg);
    (!name.contains('/')).then_some(name)
}

fn stop(s: Stop) -> Outcome {
    match s {
        Stop::Failed => Outcome::Failed,
        Stop::Refused => Outcome::Refused,
    }
}

/// The child's answer, as the route uses it.
struct Runtime {
    /// `_plain(measured)` and `_plain(history)`, as the route renders them.
    measured: String,
    history: String,
    prompt_tps: f64,
    backends: Value,
}

impl SectionAutoconfig {
    pub fn new(settings: Settings) -> Self {
        Self::with(settings, Root::new)
    }

    /// Lib tests open the models root with the synthetic reader.
    pub(crate) fn with(settings: Settings, open: fn(&Path) -> Result<Root, WalkError>) -> Self {
        Self {
            settings,
            reads: Arc::new(tokio::sync::Semaphore::new(CONCURRENT_READS)),
            open,
        }
    }

    /// Runs `work` on a blocking thread once a read slot is free (waiting counts against the
    /// deadline). The slot and the request's admission are held as long as the work runs.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, Stop> + Send + 'static,
    ) -> Result<T, Outcome> {
        let deadline = Instant::now() + DEADLINE;
        let Ok(Ok(permit)) =
            tokio::time::timeout(DEADLINE, self.reads.clone().acquire_owned()).await
        else {
            return Err(Outcome::Refused);
        };
        let task = crate::docker::spawn_blocking_admitted(move || {
            let _permit = permit;
            work()
        });
        match tokio::time::timeout_at(deadline.into(), task).await {
            Ok(Ok(result)) => result.map_err(stop),
            Ok(Err(_)) => Err(Outcome::Failed),
            Err(_) => Err(Outcome::Refused),
        }
    }

    /// The answer for one (percent-decoded) section name and the raw query string.
    pub async fn get(&self, name: &str, raw_query: &str, child: Child<'_>) -> Outcome {
        // FastAPI validates the query before the handler runs.
        let params = match query::parse(raw_query) {
            Ok(params) => params,
            Err(query::Rejection::Invalid(body)) => return Outcome::Json(422, body.into_bytes()),
            Err(query::Rejection::Unknown) => return Outcome::Refused,
        };
        let (models, ini, open, section) = (
            PathBuf::from(&self.settings.models),
            PathBuf::from(&self.settings.ini),
            self.open,
            name.to_owned(),
        );
        let located = match self
            .blocking(move || files::locate(&models, &ini, &section, open))
            .await
        {
            Ok(files::Part::Answer(body)) => return Outcome::Json(200, body),
            Ok(files::Part::Located(located)) => located,
            Err(outcome) => return outcome,
        };
        let runtime = match ask(&child, name, &located.model_rel).await {
            Ok(runtime) => runtime,
            Err(outcome) => return outcome,
        };
        let (settings, open, section) = (self.settings.clone(), self.open, name.to_owned());
        match self
            .blocking(move || answer(&settings, open, &section, &params, &located, runtime))
            .await
        {
            Ok(body) => Outcome::Json(200, body),
            Err(outcome) => outcome,
        }
    }
}

/// `{"section": name, "model": model_rel}`.
pub(crate) fn runtime_request(name: &str, model_rel: &str) -> String {
    format!(
        "{{\"section\":{},\"model\":{}}}",
        crate::model_updates::json_string(name),
        crate::model_updates::json_string(model_rel)
    )
}

/// The child's live inputs. Its own unhandled error is Python's 500; anything else it cannot
/// answer is the forwarded route's 502, and an answer that is not the expected shape refuses.
async fn ask(child: &Child<'_>, name: &str, model_rel: &str) -> Result<Runtime, Outcome> {
    let mut request = child
        .client
        .post(format!("{}{RUNTIME_PATH}", child.base))
        .header("content-type", "application/json")
        .body(runtime_request(name, model_rel))
        .timeout(CHILD_TIMEOUT);
    if !child.token.is_empty() {
        request = request.header("x-model-loader-token", child.token);
    }
    let response = request.send().await.map_err(|_| Outcome::Unavailable)?;
    let status = response.status();
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| Outcome::Unavailable)?;
        if bytes.len().saturating_add(chunk.len()) > RUNTIME_CAP {
            return Err(Outcome::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    match status.as_u16() {
        200 => parse_runtime(&bytes).ok_or(Outcome::Refused),
        500 => Err(Outcome::Failed),
        _ => Err(Outcome::Unavailable),
    }
}

/// `{"measured": {...}, "history": [...], "promptTps": "<repr>", "backends": "<json>"}`.
fn parse_runtime(bytes: &[u8]) -> Option<Runtime> {
    use serde_json::value::RawValue;
    let fields: std::collections::BTreeMap<String, Box<RawValue>> =
        serde_json::from_slice(bytes).ok()?;
    if fields.len() != 4 {
        return None;
    }
    let measured = fields.get("measured")?.get().to_owned();
    let history = fields.get("history")?.get().to_owned();
    if !measured.starts_with('{') || !history.starts_with('[') {
        return None;
    }
    let rate: String = serde_json::from_str(fields.get("promptTps")?.get()).ok()?;
    let prompt_tps = parse_repr(&rate)?;
    let text: String = serde_json::from_str(fields.get("backends")?.get()).ok()?;
    let limits = Limits {
        string_chars: 1 << 20,
        ..Limits::default()
    };
    let backends = model_files::json::parse_with_limits(&text, limits).ok()?;
    if !matches!(backends, Value::Arr(_)) {
        return None;
    }
    Some(Runtime {
        measured,
        history,
        prompt_tps,
        backends,
    })
}

/// A float's `repr`: `inf`, `-inf`, `nan`, or the shortest round-trip decimal.
fn parse_repr(text: &str) -> Option<f64> {
    match text {
        "inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        "nan" => Some(f64::NAN),
        _ if text.len() <= 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || b"+-.e".contains(&b)) =>
        {
            text.parse().ok()
        }
        _ => None,
    }
}

/// `ini.get_section(name)` and `revision()` (sha256 of the file's bytes, or of nothing).
fn read_current(
    ini_path: &Path,
    name: &str,
) -> Result<(Option<crate::sections::ini::Items>, String), Stop> {
    let current = match read_ini(ini_path)? {
        Some((_, parsed)) => parsed
            .items(name, &mut crate::sections::Budget::default())
            .map_err(|_| Stop::Refused)?,
        None => None,
    };
    let bytes = match std::fs::metadata(ini_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        _ => {
            use std::io::Read;
            let file = std::fs::File::open(ini_path).map_err(|_| Stop::Failed)?;
            let mut bytes = Vec::new();
            file.take(crate::sections::INI_CAP as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| Stop::Failed)?;
            if bytes.len() > crate::sections::INI_CAP {
                return Err(Stop::Failed);
            }
            bytes
        }
    };
    Ok((current, format!("{:x}", Sha256::digest(&bytes))))
}

/// `max(lo, min(x, hi))` as Python's builtins pick between floats.
fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    let low = if hi < x { hi } else { x };
    if low > lo {
        low
    } else {
        lo
    }
}

/// The analysis and the answer, once the child has answered.
fn answer(
    settings: &Settings,
    open: fn(&Path) -> Result<Root, WalkError>,
    name: &str,
    params: &query::Params,
    located: &files::Located,
    runtime: Runtime,
) -> Result<Vec<u8>, Stop> {
    let backends = analyze::budget_backends(
        runtime.backends,
        params.budget_gib,
        settings.cache_ram_cap_mib,
    )?;
    let (current, revision) = read_current(Path::new(&settings.ini), name)?;
    let root = open(&located.base).map_err(|_| Stop::Refused)?;
    let subdir = located
        .rel
        .as_deref()
        .and_then(|r| r.rsplit_once('/'))
        .map_or("", |(d, _)| d);
    let mode = crate::sections::ini::strip(&params.mode).to_lowercase();
    let budget_s = if params.prompt_budget_s == 0.0 {
        120.0
    } else {
        params.prompt_budget_s
    };
    let summary_value = analyze::from_summary(&located.summary);
    let inputs = analyze::Inputs {
        name,
        summary: &summary_value,
        file_size: located.file_size,
        backends,
        model_rel: &located.model_rel,
        current,
        preset: &params.preset,
        n_sessions: params.sessions.clamp(1, 8),
        model_subdir: subdir,
        spec_profile: &params.spec,
        vision: params.vision,
        prompt_tps: runtime.prompt_tps,
        prompt_budget_s: clamp(budget_s, 10.0, 3600.0),
        verified_ctx: params.verified_ctx.max(0),
        mode: &mode,
        cache_ram_cap_mib: settings.cache_ram_cap_mib,
        files: Some(analyze::Files {
            models_text: &settings.models,
            root: &root,
            base: &located.base,
        }),
        mmproj_gb_override: None,
    };
    let recommendation = analyze::analyze(&inputs)?;
    let summary = &located.summary;
    let general = summary.get("general");
    let model_shown = if !located.model_rel.is_empty() {
        located.model_rel.clone()
    } else if let Some(rel) = located.rel.as_deref().filter(|r| !r.is_empty()) {
        rel.to_owned()
    } else {
        format!("{name}.gguf")
    };
    let mut out = String::from("{\"section\":");
    render::string(&mut out, name);
    out.push_str(",\"arch\":");
    render::write(
        &mut out,
        &summary
            .get("arch")
            .map_or(Value::Null, analyze::from_summary),
    );
    out.push_str(",\"params\":");
    let params_value = if analyze::truthy(general) {
        general
            .and_then(|g| g.get("params"))
            .map_or(Value::Null, analyze::from_summary)
    } else {
        Value::Null
    };
    render::write(&mut out, &params_value);
    out.push_str(&format!(",\"fileBytes\":{},\"model\":", located.file_size));
    render::string(&mut out, &model_shown);
    out.push_str(",\"recommendation\":");
    render::write(&mut out, &recommendation);
    out.push_str(",\"measured\":");
    out.push_str(&runtime.measured);
    out.push_str(",\"history\":");
    out.push_str(&runtime.history);
    out.push_str(",\"revision\":");
    render::string(&mut out, &revision);
    out.push('}');
    if out.len() > RESPONSE_CAP {
        return Err(Stop::Refused);
    }
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests;
