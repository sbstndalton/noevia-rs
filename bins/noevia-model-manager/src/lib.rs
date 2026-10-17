#![forbid(unsafe_code)]
#[cfg(feature = "native-backends")]
pub mod backend_stats;
#[cfg(feature = "native-backend-control")]
pub mod backends;
#[cfg(feature = "native-badges")]
pub mod badges;
#[cfg(feature = "native-benchmark-runner")]
pub mod bench_runner;
#[cfg(feature = "native-bench-runs")]
pub mod bench_runs;
#[cfg(feature = "native-benchmark-overview")]
pub mod benchmark_overview;
#[cfg(feature = "native-check-updates")]
pub mod check_updates;
pub mod diagnose;
#[cfg(feature = "native-diagnose")]
pub mod diagnose_docker;
pub mod docker;
#[cfg(feature = "native-download-targets")]
pub mod download_targets;
#[cfg(feature = "native-downloads")]
pub mod downloads;
#[cfg(feature = "native-draft-heads")]
pub mod draft_heads;
pub mod embedding;
pub mod host;
pub mod idle_unload;
#[cfg(feature = "native-model-delete")]
pub mod model_delete;
#[cfg(feature = "native-model-detail")]
pub mod model_detail;
#[cfg(feature = "native-model-updates")]
pub mod model_updates;
#[cfg(feature = "native-prompts")]
pub mod prompts;
#[cfg(feature = "native-section-writes")]
pub mod section_writes;
pub mod sections;
#[cfg(feature = "native-settings")]
pub mod settings;
#[cfg(feature = "native-settings-write")]
pub mod settings_write;
#[cfg(any(
    feature = "native-settings",
    feature = "native-model-updates",
    feature = "native-bench-runs"
))]
pub mod sqlite_read;
pub mod validation;

use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use futures_util::StreamExt;
use regex::Regex;
use serde_json::json;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub const LOG_CAP: usize = 1024 * 1024;
const INVENTORY_CAP: usize = 1024;
const REQUEST_CAP: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
pub const LEGACY_BASE: &str = "http://127.0.0.1:8091";
pub const LEGACY_TIMEOUT: Duration = Duration::from_secs(600);
pub fn legacy_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(LEGACY_TIMEOUT)
        .build()
}
pub type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

#[derive(Clone, Debug)]
pub struct Container {
    pub name: String,
    pub first_image_tag: String,
}

/// Production uses bounded native Unix ingress and Bollard model/query types.
/// Synthetic adapters are confined to tests.
pub trait DockerRead: Send + Sync {
    fn containers(&self) -> Job<'_, Vec<Container>>;
    fn logs<'a>(&'a self, name: &'a str, tail: usize) -> Job<'a, Vec<u8>>;
    /// noevia#1318: what the backend test needs from `docker inspect` (`Ok(None)`: not found).
    fn backend_spec<'a>(&'a self, _name: &'a str) -> Job<'a, Option<BackendSpec>> {
        Box::pin(async { Err("docker unreachable") })
    }
    /// noevia#1318: `POST /containers/{name}/restart?t=15` for one llama.cpp backend.
    fn restart_backend<'a>(&'a self, _name: &'a str) -> Job<'a, ()> {
        Box::pin(async { Err("docker unreachable") })
    }
    /// noevia#1318: `docker.from_env().containers.list()` with each container's inspect data
    /// (`Ok(None)`: no Docker socket at all). The error is the exception class Python names.
    #[cfg(feature = "native-model-delete")]
    fn running_containers(&self) -> Job<'_, Option<Vec<model_delete::Running>>> {
        Box::pin(async { Err("ConnectionError") })
    }
}

/// The parts of `docker inspect` a backend's port is resolved from (services
/// `_resolve_internal_port`): published ports (`"8080/tcp"` with their host ports), Cmd,
/// Entrypoint and Env.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendSpec {
    pub ports: Vec<(String, Vec<String>)>,
    pub cmd: Vec<String>,
    pub entrypoint: Vec<String>,
    pub env: Vec<String>,
}

pub struct StateData {
    #[cfg(feature = "native-diagnose")]
    pub diagnosis: Option<Arc<diagnose_docker::Diagnosis>>,
    #[cfg(feature = "native-prompts")]
    pub prompts: Option<Arc<prompts::Prompts>>,
    /// noevia#1318: native backend restart and test prompt; restart errors stay in the child.
    #[cfg(feature = "native-backend-control")]
    pub backends: Option<Arc<backends::Backends>>,
    /// noevia#1318: native `POST /api/v1/models/check-updates` (asks the Hub, writes update_check).
    #[cfg(feature = "native-check-updates")]
    pub check_updates: Option<Arc<check_updates::CheckUpdates>>,
    /// noevia#1318: the native download job table and transfers; a start is planned by the child.
    #[cfg(feature = "native-downloads")]
    pub downloads: Option<Arc<downloads::Downloads>>,
    /// noevia#1318: native `POST /api/v1/models/delete`.
    #[cfg(feature = "native-model-delete")]
    pub model_delete: Option<Arc<model_delete::ModelDelete>>,
    #[cfg(feature = "native-draft-heads")]
    pub draft_heads: Option<Arc<draft_heads::DraftHeads>>,
    /// noevia#1318 slice 13: the native backend sampler and `GET /api/v1/backends`.
    #[cfg(feature = "native-backends")]
    pub backend_stats: Option<Arc<backend_stats::BackendStats>>,
    /// noevia#1318: the native benchmark runner (the one job slot): start, progress, cancel.
    #[cfg(feature = "native-benchmark-runner")]
    pub bench_runner: Option<Arc<bench_runner::Runner>>,
    /// noevia#1318: native `GET /api/v1/benchmark`; its live `job` is asked of the child.
    #[cfg(feature = "native-benchmark-overview")]
    pub benchmark_overview: Option<Arc<benchmark_overview::BenchmarkOverview>>,
    /// noevia#1318: native `PUT /api/v1/settings`; the token test is asked of the child.
    #[cfg(feature = "native-settings-write")]
    pub settings_write: Option<Arc<settings_write::SettingsWrite>>,
    /// noevia#1318: native read-only `GET /api/v1/settings`; PUT stays with Python.
    #[cfg(feature = "native-settings")]
    pub settings: Option<Arc<settings::Settings>>,
    /// noevia#1318: native read-only `GET /api/v1/models/updates`; Python keeps every writer.
    #[cfg(feature = "native-model-updates")]
    pub model_updates: Option<Arc<model_updates::ModelUpdates>>,
    /// noevia#1318: native read-only `GET /api/v1/models/detail`; its live fields (`loadedOn`,
    /// `fit`) are asked of the Python child, never re-implemented.
    #[cfg(feature = "native-model-detail")]
    pub model_detail: Option<Arc<model_detail::ModelDetail>>,
    /// noevia#1318: native read-only `GET /api/v1/benchmark/runs/{id}`; its live chart values
    /// (`capacity_gb`, `vram_predicted`) are asked of the Python child.
    #[cfg(feature = "native-bench-runs")]
    pub bench_runs: Option<Arc<bench_runs::BenchRuns>>,
    /// noevia#1318: native models.ini writers (section save/rename/delete, safe defaults, the
    /// raw editor); safe defaults ask the child for their values.
    #[cfg(feature = "native-section-writes")]
    pub section_writes: Option<Arc<section_writes::SectionWrites>>,
    /// noevia#1318: native `PUT`/`DELETE /api/v1/badges`.
    #[cfg(feature = "native-badges")]
    pub badges: Option<Arc<badges::Badges>>,
    /// noevia#1318: native read-only `GET /api/v1/download-targets`.
    #[cfg(feature = "native-download-targets")]
    pub download_targets: Option<Arc<download_targets::DownloadTargets>>,
    pub sections: Option<Arc<sections::Sections>>,
    /// #1668: present only when EMBED_CONTAINER names the embedding service.
    pub embedding: Option<Arc<embedding::EmbeddingService>>,
    pub host: Option<Arc<tokio::sync::Mutex<host::HostHistory>>>,
    pub token: String,
    pub configured: Vec<String>,
    pub docker: Arc<dyn DockerRead>,
    pub redactor: Redactor,
    pub permits: Semaphore,
    pub legacy: reqwest::Client,
    /// The legacy child's address (production: LEGACY_BASE); tests use an ephemeral port.
    pub legacy_base: String,
    /// #1747: keeps the chat preset's global `sleep-idle-seconds` in line with the setting.
    pub idle_unload: Option<Arc<idle_unload::Reconciler>>,
}

/// #1668: what the Python child is told about the guarded embedding service: its container and
/// address, so it skips only env names that point at it. `None` clears them all.
pub fn embed_guard_env(service: Option<(&str, &str)>) -> [(&'static str, Option<String>); 3] {
    [
        (
            "NOEVIA_EMBED_SERVICE_GUARDED",
            service.map(|_| "1".to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_CONTAINER",
            service.map(|(c, _)| c.to_owned()),
        ),
        (
            "NOEVIA_EMBED_SERVICE_URL",
            service.map(|(_, u)| u.to_owned()),
        ),
    ]
}

/// #1668: PUT /api/v1/embedding-service restarts and stops a container, so it is never left open.
pub fn require_embed_token(container: &str, token: &str) -> Result<(), &'static str> {
    if !container.is_empty() && token.is_empty() {
        return Err("EMBED_CONTAINER needs MODEL_LOADER_TOKEN: the embedding service route must not be open");
    }
    Ok(())
}

pub fn validate_token(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let token =
        raw.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}'));
    if token.chars().count() < 32 || token.len() > 4096 {
        return Err("MODEL_LOADER_TOKEN must contain 32..4096 non-whitespace characters");
    }
    Ok(token.to_owned())
}

pub fn effective_names(
    configured: &[String],
    inventory: &[Container],
) -> Result<Vec<String>, &'static str> {
    if configured.len() > INVENTORY_CAP || inventory.len() > INVENTORY_CAP {
        return Err("container inventory exceeded limit");
    }
    let mut names = Vec::new();
    names
        .try_reserve(configured.len().saturating_add(inventory.len()))
        .map_err(|_| "allocation refused")?;
    let mut seen = std::collections::HashSet::new();
    seen.try_reserve(configured.len().saturating_add(inventory.len()))
        .map_err(|_| "allocation refused")?;
    for name in configured.iter().map(String::as_str).chain(
        inventory
            .iter()
            .filter(|c| {
                c.name != "model-loader" && c.first_image_tag.to_lowercase().contains("llama.cpp")
            })
            .map(|c| c.name.as_str()),
    ) {
        if name.len() > 256 {
            return Err("container name exceeded limit");
        }
        if !name.is_empty() && seen.insert(name) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Rust regex has linear matching. Python's lookaround exclusions are checked on captures.
pub struct Redactor {
    patterns: Vec<(Regex, &'static str)>,
    keys: Regex,
}
// Python re uses str.isalnum plus underscore for word characters; Rust regex
// additionally includes marks and connector punctuation. Python whitespace also
// includes the four information separators. Nested classes preserve negation.
fn python_classes(pattern: &str) -> String {
    pattern
        .replace(r"\w", r"[\p{L}\p{N}_]")
        .replace(r"\s", r"[\s\x1c-\x1f]")
}
impl Redactor {
    pub fn new() -> Result<Self, regex::Error> {
        let mut patterns = Vec::new();
        for (p, replacement) in [
            (
                r#"(?i)(author[iİı]zat[iİı]on\s*[:=]\s*)(?:bearer|bas[iİı]c|token)?\s*[^\s,;\"']+"#,
                "${1}[redacted]",
            ),
            (
                r"(?i)(bearer\s+)[A-Za-zİıſK0-9._~+/=-]{8,}",
                "${1}[redacted]",
            ),
            (
                r"(?i)([a-zİıſK][a-z0-9+.-]*://)[^\s/:@]+:[^\s/@]+@",
                "${1}[redacted]@",
            ),
            (r"hf_[A-Za-z0-9]{20,}", "[redacted]"),
            (r"sk-[A-Za-z0-9_-]{16,}", "[redacted]"),
            (r"gh[pousr]_[A-Za-z0-9]{20,}", "[redacted]"),
            (r"AKIA[0-9A-Z]{16}", "[redacted]"),
            (
                r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
                "[redacted]",
            ),
        ] {
            patterns.push((Regex::new(&python_classes(p))?, replacement));
        }
        Ok(Self {
            patterns,
            keys: Regex::new(&python_classes(
                r#"(?i)(([\w.-]*(?:ap[iİı][_-]?key|ap[iİı]key|token|secret|password|passwd|pwd|credent[iİı]al|cook[iİı]e|sess[iİı]on)[\w.-]*)[\"']?\s*[:=]\s*[\"']?)([^\s,;&\"']+)"#,
            ))?,
        })
    }
    pub fn redact(&self, line: &str) -> Result<String, &'static str> {
        if line.len() > LOG_CAP {
            return Err("log response exceeded limit");
        }
        let mut text = line.to_owned();
        for (index, (pattern, replacement)) in self.patterns.iter().enumerate() {
            if index == 2 {
                text = self
                    .keys
                    .replace_all(&text, |caps: &regex::Captures<'_>| {
                        let key = caps
                            .get(2)
                            .map(|m| m.as_str())
                            .unwrap_or_default()
                            .to_lowercase();
                        let value = caps.get(3).map(|m| m.as_str()).unwrap_or_default();
                        // Broader redaction is intentional: numeric-looking credentials and token
                        // suffixes are also scrubbed. Preserve established numeric counter fields.
                        let counter = (key.starts_with("n_")
                            && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'))
                            || key == "tokens"
                            || key == "token_count"
                            || (key == "session"
                                && value.chars().all(|c| c.is_ascii_digit() || c == '.'));
                        if counter {
                            caps.get(0)
                                .map(|m| m.as_str())
                                .unwrap_or_default()
                                .to_owned()
                        } else {
                            format!(
                                "{}[redacted]",
                                caps.get(1).map(|m| m.as_str()).unwrap_or_default()
                            )
                        }
                    })
                    .into_owned();
            }
            text = pattern.replace_all(&text, *replacement).into_owned();
            if text.len() > LOG_CAP * 2 {
                return Err("redacted logs exceeded limit");
            }
        }
        Ok(text)
    }
    pub fn filter(&self, raw: &[u8], q: &str, level: &str) -> Result<Vec<String>, &'static str> {
        if raw.len() > LOG_CAP || q.len() > 4096 || level.len() > 16 {
            return Err("log input exceeded limit");
        }
        let decoded = String::from_utf8_lossy(raw);
        // Scrub across line separators before splitting: a separator between a key
        // and its value must not turn the value into an unlabelled secret line.
        let decoded = self.redact(&decoded)?;
        // Python's contextual Unicode lower differs from Rust. Non-ASCII search is refused
        // rather than potentially letting a secret-dependent filter differ from the oracle.
        if !q.is_ascii() {
            return Err("non-ASCII log filters are not supported");
        }
        let needle = q.to_ascii_lowercase();
        let mut out = Vec::new();
        out.try_reserve(2000).map_err(|_| "allocation refused")?;
        let mut bytes = 0usize;
        for line in python_lines(&decoded) {
            let line = self.redact(line)?;
            let low = line.to_lowercase();
            if !needle.is_empty() && !low.contains(&needle) {
                continue;
            }
            if level == "error" && !["err", "fatal"].iter().any(|s| low.contains(s)) {
                continue;
            }
            if level == "warn"
                && !["err", "warn", "wrn", "fatal"]
                    .iter()
                    .any(|s| low.contains(s))
            {
                continue;
            }
            bytes = bytes
                .checked_add(line.len())
                .ok_or("log output exceeded limit")?;
            if bytes > LOG_CAP * 2 || out.len() >= 4000 {
                return Err("log output exceeded limit");
            }
            out.try_reserve(1).map_err(|_| "allocation refused")?;
            out.push(line);
        }
        Ok(out)
    }
}

fn python_lines(text: &str) -> impl Iterator<Item = &str> {
    let mut pieces = text.split_inclusive([
        '\n', '\r', '\u{000b}', '\u{000c}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}',
        '\u{2028}', '\u{2029}',
    ]);
    let mut previous_cr = false;
    std::iter::from_fn(move || loop {
        let piece = pieces.next()?;
        if previous_cr && piece == "\n" {
            previous_cr = false;
            continue;
        }
        previous_cr = piece.ends_with('\r');
        let last = piece.chars().next_back()?;
        return Some(
            if matches!(
                last,
                '\n' | '\r'
                    | '\u{000b}'
                    | '\u{000c}'
                    | '\u{001c}'
                    | '\u{001d}'
                    | '\u{001e}'
                    | '\u{0085}'
                    | '\u{2028}'
                    | '\u{2029}'
            ) {
                piece
                    .get(..piece.len().saturating_sub(last.len_utf8()))
                    .unwrap_or_default()
            } else {
                piece
            },
        );
    })
}

pub fn router(state: Arc<StateData>) -> Router {
    Router::new().fallback(handler).with_state(state)
}

/// Lib tests of work that outlives its request (noevia#1997, #1998, #2006).
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::mpsc;

    /// Four request permits, no Docker, the legacy child at `legacy_base`, and nothing native
    /// until a test sets it.
    pub(crate) fn test_state(legacy_base: &str) -> StateData {
        StateData {
            #[cfg(feature = "native-diagnose")]
            diagnosis: None,
            #[cfg(feature = "native-prompts")]
            prompts: None,
            #[cfg(feature = "native-backend-control")]
            backends: None,
            #[cfg(feature = "native-check-updates")]
            check_updates: None,
            #[cfg(feature = "native-downloads")]
            downloads: None,
            #[cfg(feature = "native-model-delete")]
            model_delete: None,
            #[cfg(feature = "native-draft-heads")]
            draft_heads: None,
            #[cfg(feature = "native-backends")]
            backend_stats: None,
            #[cfg(feature = "native-benchmark-runner")]
            bench_runner: None,
            #[cfg(feature = "native-benchmark-overview")]
            benchmark_overview: None,
            #[cfg(feature = "native-settings-write")]
            settings_write: None,
            #[cfg(feature = "native-settings")]
            settings: None,
            #[cfg(feature = "native-model-updates")]
            model_updates: None,
            #[cfg(feature = "native-model-detail")]
            model_detail: None,
            #[cfg(feature = "native-bench-runs")]
            bench_runs: None,
            #[cfg(feature = "native-section-writes")]
            section_writes: None,
            #[cfg(feature = "native-badges")]
            badges: None,
            #[cfg(feature = "native-download-targets")]
            download_targets: None,
            sections: None,
            embedding: None,
            host: None,
            token: String::new(),
            configured: Vec::new(),
            docker: Arc::new(docker::UnixDocker {
                socket: "/nonexistent/docker.sock".into(),
            }),
            redactor: Redactor::new().unwrap(),
            permits: Semaphore::new(4),
            legacy: legacy_client().unwrap(),
            legacy_base: legacy_base.to_owned(),
            idle_unload: None,
        }
    }

    /// A hook for a blocking task: the first call reports that it entered and blocks until the
    /// gate is opened (at most 30 s); later calls pass.
    pub(crate) struct Gate {
        pub(crate) hook: Arc<dyn Fn() + Send + Sync>,
        entered: mpsc::Receiver<()>,
        open: mpsc::Sender<()>,
    }

    impl Gate {
        pub(crate) fn new() -> Self {
            let (entered_tx, entered) = mpsc::channel::<()>();
            let (open, opened) = mpsc::channel::<()>();
            let first = std::sync::Mutex::new(Some((entered_tx, opened)));
            Self {
                hook: Arc::new(move || {
                    let Some((entered, opened)) = first.lock().unwrap().take() else {
                        return;
                    };
                    entered.send(()).unwrap();
                    opened.recv_timeout(Duration::from_secs(30)).unwrap();
                }),
                entered,
                open,
            }
        }

        /// Waits until the hook has been entered.
        pub(crate) async fn entered(self) -> mpsc::Sender<()> {
            let Self { entered, open, .. } = self;
            tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(30)))
                .await
                .unwrap()
                .unwrap();
            open
        }
    }

    /// Starts `request` on the router in its own task, and drops it (the client went away) once
    /// `gate`'s hook has been entered. The sender opens the gate.
    #[cfg(feature = "native-check-updates")]
    pub(crate) async fn dropped_while_blocked(
        state: &Arc<StateData>,
        request: axum::http::Request<Body>,
        gate: Gate,
    ) -> mpsc::Sender<()> {
        use tower::ServiceExt;
        let task = tokio::spawn(router(state.clone()).oneshot(request));
        drop_once_entered(task, gate).await
    }

    /// noevia#2006: starts `request` on the router in its own task, with a gate that every
    /// blocking task it starts through `docker::spawn_blocking_admitted` enters first, and drops
    /// it (the client went away) once the first has entered. The request is gone, its blocking
    /// work is not: checks that the request's permit is still taken. The sender opens the gate.
    pub(crate) async fn dropped_in_blocking_work(
        state: &Arc<StateData>,
        request: axum::http::Request<Body>,
    ) -> mpsc::Sender<()> {
        use tower::ServiceExt;
        let gate = Gate::new();
        let task = tokio::spawn(
            docker::BLOCKING_HOOK.scope(gate.hook.clone(), router(state.clone()).oneshot(request)),
        );
        let open = drop_once_entered(task, gate).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state.permits.available_permits(),
            3,
            "permit returned mid-work"
        );
        open
    }

    async fn drop_once_entered<T: std::fmt::Debug>(
        task: tokio::task::JoinHandle<T>,
        gate: Gate,
    ) -> mpsc::Sender<()> {
        let open = gate.entered().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        open
    }

    /// Waits (at most 30 s) until every request permit is back.
    pub(crate) async fn all_permits_back(state: &StateData) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while state.permits.available_permits() < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}

/// Only the exact GET path is native; PUT (the writer), other methods and slash variants keep
/// their existing forward/405/404 handling.
#[cfg(feature = "native-settings")]
fn native_settings_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.settings.is_some() && *method == Method::GET && path == "/api/v1/settings"
}

/// Only the exact GET path is native; other methods and slash variants keep their existing
/// forward/405/404 handling.
#[cfg(feature = "native-model-updates")]
fn native_model_updates_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.model_updates.is_some() && *method == Method::GET && path == "/api/v1/models/updates"
}

/// Only the exact GET path is native; other methods and slash variants keep their existing
/// forward/405/404 handling.
#[cfg(feature = "native-model-detail")]
fn native_model_detail_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.model_detail.is_some() && *method == Method::GET && path == "/api/v1/models/detail"
}

/// Only the exact GET path is native; `POST /api/v1/downloads` (which validates its `target`
/// against the same list) and other methods keep their existing forward/405/404 handling.
#[cfg(feature = "native-download-targets")]
fn native_download_targets_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.download_targets.is_some() && *method == Method::GET && path == "/api/v1/download-targets"
}

/// Only the exact path with PUT or DELETE is native; GET and other methods keep the existing 405
/// and the slash variant its forward.
#[cfg(feature = "native-badges")]
fn native_badges_route(state: &StateData, method: &Method, path: &str) -> bool {
    state.badges.is_some()
        && (*method == Method::PUT || *method == Method::DELETE)
        && path == "/api/v1/badges"
}

/// Only GET of a canonical id (`-?[0-9]+` inside i64) is native; other spellings, methods and
/// the slash variant keep their existing forward/405/404 handling.
#[cfg(feature = "native-bench-runs")]
fn native_bench_run_id(state: &StateData, method: &Method, path: &str) -> Option<i64> {
    if state.bench_runs.is_none() || *method != Method::GET {
        return None;
    }
    path.strip_prefix("/api/v1/benchmark/runs/")
        .and_then(bench_runs::canonical_id)
}

#[cfg(feature = "native-section-writes")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionWrite {
    Save(String),
    Delete(String),
    Rename(String),
    SafeDefaults(String),
    ModelsIni,
}

/// The native models.ini writers: exact paths and methods only; a name segment is
/// percent-decoded and must not contain `/` (FastAPI would not match it either).
#[cfg(feature = "native-section-writes")]
pub fn section_write_route(method: &Method, path: &str) -> Option<SectionWrite> {
    if path == "/api/v1/models-ini" {
        return (*method == Method::PUT).then_some(SectionWrite::ModelsIni);
    }
    let rest = path.strip_prefix("/api/v1/sections/")?;
    let mut parts = rest.split('/');
    let (seg, tail, extra) = (parts.next()?, parts.next(), parts.next());
    if seg.is_empty() || extra.is_some() {
        return None;
    }
    // uvicorn's `unquote`: invalid escapes stay literal, invalid UTF-8 becomes U+FFFD.
    let name = lossy_unquote(seg);
    if name.contains('/') {
        return None;
    }
    match (method, tail) {
        (&Method::PUT, None) => Some(SectionWrite::Save(name)),
        (&Method::DELETE, None) => Some(SectionWrite::Delete(name)),
        (&Method::POST, Some("rename")) => Some(SectionWrite::Rename(name)),
        (&Method::POST, Some("safe-defaults")) => Some(SectionWrite::SafeDefaults(name)),
        _ => None,
    }
}

#[cfg(any(feature = "native-section-writes", feature = "native-draft-heads"))]
fn lossy_unquote(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        let hex = |b: Option<&u8>| b.and_then(|b| char::from(*b).to_digit(16));
        if byte == b'%' {
            if let (Some(a), Some(b)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
                out.push(u8::try_from(a * 16 + b).unwrap_or(0));
                i += 3;
                continue;
            }
        }
        out.push(byte);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// noevia#1318: `GET /api/v1/sections/{name}/draft-heads`, the name segment percent-decoded as
/// uvicorn does; a name that decodes to a slash is not this route (FastAPI would not match it).
#[cfg(feature = "native-draft-heads")]
pub fn draft_heads_route(method: &Method, path: &str) -> Option<String> {
    if *method != Method::GET {
        return None;
    }
    let seg = path
        .strip_prefix("/api/v1/sections/")?
        .strip_suffix("/draft-heads")?;
    if seg.is_empty() || seg.contains('/') {
        return None;
    }
    let name = lossy_unquote(seg);
    (!name.contains('/')).then_some(name)
}

/// FastAPI's answer to an unhandled exception, byte for byte.
fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "Internal Server Error",
    )
        .into_response()
}

fn detail(status: StatusCode, text: &'static str) -> Response {
    (status, Json(json!({"detail":text}))).into_response()
}

async fn handler(State(state): State<Arc<StateData>>, req: Request) -> Response {
    #[cfg(feature = "native-diagnose")]
    let diagnosis_deadline = std::time::Instant::now() + TIMEOUT;
    let path = req.uri().path();
    if path != "/api/v1/health" && !state.token.is_empty() {
        let supplied = req
            .headers()
            .get("x-model-loader-token")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        if !server_auth::js::ct_eq(supplied, &state.token) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            )
                .into_response();
        }
    }
    if req.uri().to_string().len() > 8192 {
        return detail(StatusCode::URI_TOO_LONG, "request URI exceeded limit");
    }
    #[cfg(feature = "native-diagnose")]
    if let Some(diagnosis) = &state.diagnosis {
        let route = diagnose_docker::classify(path);
        if !matches!(route, diagnose_docker::Route::NotOwned) {
            return native_diagnosis(
                state.clone(),
                diagnosis.clone(),
                req,
                route,
                diagnosis_deadline,
            )
            .await;
        }
    }
    if path == "/api/v1/health" {
        return dispatch(state, req).await;
    }
    if req.method() == Method::GET
        && state.sections.is_some()
        && (path == "/api/v1/sections"
            || path
                .strip_prefix("/api/v1/sections/")
                .is_some_and(|n| !n.is_empty() && !n.contains('/')))
    {
        return dispatch(state, req).await;
    }
    let Some(admission) = Admission::try_new(&state) else {
        return detail(StatusCode::SERVICE_UNAVAILABLE, "service is busy");
    };
    let native = path
        .strip_prefix("/api/v1/backends/")
        .and_then(|p| p.strip_suffix("/logs"))
        .is_some();
    #[cfg(feature = "native-prompts")]
    let native = native
        || (state.prompts.is_some()
            && !matches!(prompts::classify(path), prompts::Route::NotOwned));
    #[cfg(feature = "native-settings")]
    let native = native || native_settings_route(&state, req.method(), path);
    #[cfg(feature = "native-model-updates")]
    let native = native || native_model_updates_route(&state, req.method(), path);
    #[cfg(feature = "native-download-targets")]
    let native = native || native_download_targets_route(&state, req.method(), path);
    #[cfg(feature = "native-badges")]
    let native = native || native_badges_route(&state, req.method(), path);
    // A download start waits on the child's plan (Hub lookups): a forwarded route's budget.
    #[cfg(feature = "native-downloads")]
    let native = native
        || (state.downloads.is_some()
            && matches!(
                downloads_route(req.method(), path),
                Some(DownloadRoute::List | DownloadRoute::Cancel(_) | DownloadRoute::Clear)
            ));
    // A benchmark run also waits on the child for its live values: a forwarded route's budget,
    // like model detail (no change to `native` here).
    // The detail route waits on the Python child for its live fields: a forwarded route's budget.
    #[cfg(feature = "native-model-detail")]
    let native = native && !native_model_detail_route(&state, req.method(), path);
    let budget = if native || (path == "/api/v1/host" && state.host.is_some()) {
        TIMEOUT
    } else if path == "/api/v1/embedding-service" && state.embedding.is_some() {
        // The Docker action is bounded by embedding::ACT_TIMEOUT and runs in its own task.
        embedding::ACT_TIMEOUT + embedding::LOCK_WAIT + Duration::from_secs(5)
    } else {
        LEGACY_TIMEOUT
    };
    serve_admitted(state, admission, req, budget).await
}

/// The deadline of an admitted request's route. Work the request starts in its own admitted task
/// (a delete forward) is bounded by it as well, so the timeout answer still goes out by the
/// route's budget and what that work holds is released then (noevia#1997, #1998).
#[derive(Clone, Copy)]
struct RouteDeadline(tokio::time::Instant);

/// The deadline `req` must be answered by: its route's, or a forwarded route's budget from now.
fn route_deadline(req: &Request) -> tokio::time::Instant {
    req.extensions().get::<RouteDeadline>().map_or_else(
        || tokio::time::Instant::now() + LEGACY_TIMEOUT,
        |deadline| deadline.0,
    )
}

fn timed_out() -> Response {
    detail(
        StatusCode::GATEWAY_TIMEOUT,
        "model manager request timed out",
    )
}

/// Dispatches an admitted request within `budget`.
async fn serve_admitted(
    state: Arc<StateData>,
    admission: Admission,
    mut req: Request,
    budget: Duration,
) -> Response {
    let deadline = tokio::time::Instant::now() + budget;
    req.extensions_mut().insert(RouteDeadline(deadline));
    // The dispatch future is dropped inside the scope (also on a timeout), so a Docker driver it
    // leaves behind is registered with this admission and joined before the permit returns.
    // (tokio drops a scoped future inside its scope, also when the request itself is dropped.)
    // Work it starts with `docker::spawn_admitted` or `spawn_blocking_admitted` is registered
    // here when spawned and is never aborted: the permit returns once that work has ended
    // (noevia#1998). A delete forward is bounded by the same deadline.
    let result = docker::ORPHANS
        .scope(
            admission.orphans.clone(),
            tokio::time::timeout_at(deadline, dispatch(state.clone(), req)),
        )
        .await;
    admission.release().await;
    result.unwrap_or_else(|_| timed_out())
}

/// One of the request permits (noevia#1385). The permit is taken out of the semaphore with
/// `forget` and given back only after every Docker driver the request left behind has been
/// joined: inline in [`Admission::release`], or, when the request future is dropped (the client
/// went away), by a task that owns those joins and the permit until they are observed.
struct Admission {
    state: Arc<StateData>,
    orphans: docker::Orphans,
    held: bool,
}

impl Admission {
    fn try_new(state: &Arc<StateData>) -> Option<Self> {
        state.permits.try_acquire().ok()?.forget();
        Some(Admission {
            state: state.clone(),
            orphans: Default::default(),
            held: true,
        })
    }

    async fn release(mut self) {
        docker::observe(&self.orphans).await;
        self.held = false;
        self.state.permits.add_permits(1);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        let (state, orphans) = (self.state.clone(), self.orphans.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    docker::observe(&orphans).await;
                    state.permits.add_permits(1);
                });
            }
            // No runtime left (shutdown): no task can still be running a driver.
            Err(_) => state.permits.add_permits(1),
        }
    }
}

#[cfg(feature = "native-diagnose")]
async fn native_diagnosis(
    state: Arc<StateData>,
    diagnosis: Arc<diagnose_docker::Diagnosis>,
    req: Request,
    route: diagnose_docker::Route,
    deadline: std::time::Instant,
) -> Response {
    fn head(mut response: Response, method: &Method) -> Response {
        if *method == Method::HEAD {
            *response.body_mut() = Body::empty();
        }
        response
    }
    match route {
        diagnose_docker::Route::NotFound | diagnose_docker::Route::NotOwned => {
            head(detail(StatusCode::NOT_FOUND, "Not Found"), req.method())
        }
        diagnose_docker::Route::Redirect(canonical) => {
            let host = req
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("localhost");
            let mut url = match reqwest::Url::parse(&format!("http://{host}")) {
                Ok(url) => url,
                Err(_) => return detail(StatusCode::BAD_REQUEST, "invalid request path"),
            };
            url.set_path(&canonical);
            url.set_query(req.uri().query());
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(axum::http::header::LOCATION, url.as_str())],
                Body::empty(),
            )
                .into_response()
        }
        diagnose_docker::Route::Item(name) => {
            if req.method() != Method::GET {
                let mut response = detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
                response.headers_mut().insert(
                    axum::http::header::ALLOW,
                    axum::http::HeaderValue::from_static("GET"),
                );
                return head(response, req.method());
            }
            if diagnose_docker::identifier(&name).is_err() {
                return detail(StatusCode::BAD_REQUEST, "invalid backend name");
            }
            match diagnosis.request(state, name, deadline).await {
                diagnose_docker::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                diagnose_docker::Outcome::Unknown => {
                    detail(StatusCode::NOT_FOUND, "unknown backend")
                }
                diagnose_docker::Outcome::Busy => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, "service is busy")
                }
                diagnose_docker::Outcome::Refused => detail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Native backend diagnosis is unavailable",
                ),
            }
        }
    }
}

/// noevia#1937: the child's `/api/v1/internal/*` endpoints are for this parent only (it calls them
/// itself through `child_runtime` and `set_restart_error`, never through `dispatch`). True for the
/// raw path and for the percent-decoded, dot-segment-resolved path, so `%2F`, `%2e%2e` and doubly
/// encoded spellings cannot reach them either.
fn is_internal_path(path: &str) -> bool {
    const PREFIX: &str = "/api/v1/internal/";
    if path.starts_with(PREFIX) {
        return true;
    }
    let mut current = path.to_owned();
    // Decode to a fixed point (bounded), so `%252F` is seen as `/` as well.
    for _ in 0..4 {
        let decoded = percent_decode_lossy(&current);
        if decoded == current {
            break;
        }
        current = decoded;
    }
    let normalized = current.replace('\\', "/");
    let mut segments: Vec<&str> = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut leading = segments.iter();
    matches!(
        (leading.next(), leading.next(), leading.next()),
        (Some(a), Some(v), Some(i))
            if a.eq_ignore_ascii_case("api")
                && v.eq_ignore_ascii_case("v1")
                && i.eq_ignore_ascii_case("internal")
    )
}

fn percent_decode_lossy(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let mut ahead = bytes.clone();
            let pair = ahead
                .next()
                .and_then(|hi| (hi as char).to_digit(16))
                .zip(ahead.next().and_then(|lo| (lo as char).to_digit(16)));
            if let Some((hi, lo)) = pair {
                out.push(u8::try_from(hi * 16 + lo).unwrap_or(b'%'));
                bytes.next();
                bytes.next();
                continue;
            }
        }
        out.push(byte);
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn dispatch(state: Arc<StateData>, req: Request) -> Response {
    if is_internal_path(req.uri().path()) {
        return detail(StatusCode::NOT_FOUND, "Not Found");
    }
    let path = req.uri().path();
    #[cfg(feature = "native-prompts")]
    if let Some(prompts) = &state.prompts {
        let route = prompts::classify(path);
        match &route {
            prompts::Route::Invalid => {
                return detail(StatusCode::BAD_REQUEST, "invalid request path")
            }
            prompts::Route::Redirect(canonical) => {
                let host = req
                    .headers()
                    .get("host")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("localhost");
                let query = req
                    .uri()
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                return (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(
                        axum::http::header::LOCATION,
                        format!("http://{host}{canonical}{query}"),
                    )],
                )
                    .into_response();
            }
            _ => {}
        }
        if matches!(route, prompts::Route::Collection | prompts::Route::Item(_)) {
            let item = match &route {
                prompts::Route::Item(id) => Some(id.as_str()),
                _ => None,
            };
            let operation = match (req.method(), item) {
                (&Method::GET, None) => prompts::Operation::List,
                (&Method::POST, None) => {
                    let json_content = match req.headers().get(axum::http::header::CONTENT_TYPE) {
                        Some(value) => match value.to_str() {
                            Ok(value) if value.len() <= 8192 => {
                                prompts::json_content_type(Some(value))
                            }
                            _ => return prompt_error(prompts::Error::Body),
                        },
                        None => true,
                    };
                    let bytes = match to_bytes(req.into_body(), REQUEST_CAP).await {
                        Ok(b) => b,
                        Err(_) => {
                            return detail(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                "request body exceeded limit",
                            )
                        }
                    };
                    if !json_content && !bytes.is_empty() {
                        return match prompts::non_json_validation(&bytes) {
                            Ok(error) => {
                                (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response()
                            }
                            Err(e) => prompt_error(e),
                        };
                    }
                    let decoded = match prompts::decode_body(&bytes) {
                        Ok(text) => text,
                        Err(e) => return prompt_error(e),
                    };
                    if let Some(error) =
                        prompts::validation_with_empty(decoded.as_bytes(), bytes.is_empty())
                    {
                        return (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response();
                    }
                    let (name, body) = match prompts::parse_fields_json(&decoded) {
                        Ok(v) => v,
                        Err(e) => return prompt_error(e),
                    };
                    prompts::Operation::Add(name, body)
                }
                (&Method::DELETE, Some(id)) => {
                    let id=match prompts::parse_id(id){Ok(id)=>id,Err(_)=>return (StatusCode::UNPROCESSABLE_ENTITY,Json(validation::parsing("int_parsing",&[validation::key("path"),validation::key("pid")],"Input should be a valid integer, unable to parse string as an integer",id))).into_response()};
                    prompts::Operation::Delete(id)
                }
                _ => return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed"),
            };
            return match prompts.response(operation).await {
                Ok(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                Err(e) => prompt_error(e),
            };
        }
        if crate::decode_segment(path)
            .ok()
            .is_some_and(|path| path.starts_with("/api/v1/prompts/"))
        {
            return detail(StatusCode::NOT_FOUND, "Not Found");
        }
    }
    if req.method() == Method::GET {
        if let Some(sections) = &state.sections {
            if let Some(trimmed) = path.strip_suffix('/') {
                if trimmed == "/api/v1/sections"
                    || trimmed
                        .strip_prefix("/api/v1/sections/")
                        .is_some_and(|n| !n.is_empty() && !n.contains('/'))
                {
                    let host = req
                        .headers()
                        .get("host")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("localhost");
                    let query = req
                        .uri()
                        .query()
                        .map(|q| format!("?{q}"))
                        .unwrap_or_default();
                    return (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(
                            axum::http::header::LOCATION,
                            format!("http://{host}{trimmed}{query}"),
                        )],
                    )
                        .into_response();
                }
            }
            if path == "/api/v1/sections"
                || path
                    .strip_prefix("/api/v1/sections/")
                    .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            {
                return native_sections(sections, path, req.uri().query().unwrap_or_default())
                    .await;
            }
        }
    }
    #[cfg(feature = "native-settings")]
    if native_settings_route(&state, req.method(), path) {
        if let Some(settings) = &state.settings {
            return match settings.response().await {
                settings::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                // Byte-identical to FastAPI's unhandled-exception answer.
                settings::Outcome::Failed => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; charset=utf-8",
                    )],
                    "Internal Server Error",
                )
                    .into_response(),
            };
        }
    }
    #[cfg(feature = "native-model-updates")]
    if native_model_updates_route(&state, req.method(), path) {
        if let Some(updates) = &state.model_updates {
            return match updates.response().await {
                model_updates::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                model_updates::Outcome::Failed => internal_error(),
                model_updates::Outcome::Refused => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, model_updates::REFUSAL)
                }
            };
        }
    }
    #[cfg(feature = "native-downloads")]
    if let Some(table) = state.downloads.clone() {
        if let Some(route) = downloads_route(req.method(), path) {
            return native_downloads(&state, &table, route, req).await;
        }
    }
    #[cfg(feature = "native-benchmark-runner")]
    if let Some(runner) = state.bench_runner.clone() {
        if let Some(route) = bench_runner_route(req.method(), path) {
            return native_bench_runner(&runner, route, req).await;
        }
    }
    #[cfg(feature = "native-draft-heads")]
    if let Some(heads) = state.draft_heads.clone() {
        if let Some(name) = draft_heads_route(req.method(), path) {
            return match heads.get(&name).await {
                draft_heads::Outcome::Json(status, bytes) => (
                    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                draft_heads::Outcome::Failed => internal_error(),
                draft_heads::Outcome::Refused => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, draft_heads::REFUSAL)
                }
            };
        }
    }
    #[cfg(feature = "native-backends")]
    if *req.method() == Method::GET && path == "/api/v1/backends" {
        if let Some(stats) = state.backend_stats.clone() {
            return match stats.get().await {
                backend_stats::Outcome::Json(body) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
                    .into_response(),
                backend_stats::Outcome::Failed => internal_error(),
                backend_stats::Outcome::Refused => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, backend_stats::REFUSAL)
                }
            };
        }
    }
    #[cfg(feature = "native-check-updates")]
    if *req.method() == Method::POST && path == "/api/v1/models/check-updates" {
        if let Some(check) = state.check_updates.clone() {
            return match check.post().await {
                check_updates::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                check_updates::Outcome::Failed => internal_error(),
                check_updates::Outcome::Refused => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, model_updates::REFUSAL)
                }
            };
        }
    }
    #[cfg(feature = "native-benchmark-overview")]
    if *req.method() == Method::GET && path == "/api/v1/benchmark" {
        if let Some(overview) = state.benchmark_overview.clone() {
            return native_benchmark_overview(&state, &overview).await;
        }
    }
    #[cfg(feature = "native-backend-control")]
    if let Some(control) = state.backends.clone() {
        if let Some((name, action)) = backend_control_route(req.method(), path) {
            return native_backend_control(state.clone(), control, name, action, req).await;
        }
    }
    #[cfg(feature = "native-settings-write")]
    if *req.method() == Method::PUT && path == "/api/v1/settings" {
        if let Some(writer) = state.settings_write.clone() {
            return native_settings_write(&state, &writer, req).await;
        }
    }
    #[cfg(feature = "native-section-writes")]
    if let Some(writes) = state.section_writes.clone() {
        if let Some(route) = section_write_route(req.method(), path) {
            return native_section_write(&state, writes, route, req).await;
        }
    }
    #[cfg(feature = "native-bench-runs")]
    if let Some(run_id) = native_bench_run_id(&state, req.method(), path) {
        if let Some(runs) = state.bench_runs.clone() {
            return native_bench_run(&state, &runs, run_id).await;
        }
    }
    #[cfg(feature = "native-badges")]
    if native_badges_route(&state, req.method(), path) {
        if let Some(badges) = state.badges.clone() {
            return native_badges(&badges, req).await;
        }
    }
    #[cfg(feature = "native-download-targets")]
    if native_download_targets_route(&state, req.method(), path) {
        if let Some(targets) = &state.download_targets {
            return match targets.response().await {
                download_targets::Outcome::Json(bytes) => (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    bytes,
                )
                    .into_response(),
                download_targets::Outcome::Failed => internal_error(),
            };
        }
    }
    #[cfg(feature = "native-model-detail")]
    if native_model_detail_route(&state, req.method(), path) {
        if let Some(detail) = state.model_detail.clone() {
            return native_model_detail(&state, &detail, req.uri().query().unwrap_or_default())
                .await;
        }
    }
    if path == "/api/v1/health" {
        return if req.method() == Method::GET {
            Json(json!({"ok":true})).into_response()
        } else {
            detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        };
    }
    if path == "/api/v1/host" {
        if let Some(host) = &state.host {
            return if req.method() == Method::GET {
                Json(host.lock().await.response()).into_response()
            } else {
                detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
            };
        }
    }
    if path == IDLE_UNLOAD_ROUTE {
        if let Some(reconciler) = state.idle_unload.clone() {
            return native_idle_unload(&reconciler, req).await;
        }
    }
    if let Some(service) = state.embedding.clone() {
        if path == "/api/v1/embedding-service" {
            return native_embedding(&service, req).await;
        }
        if path == "/api/v1/models/delete" && req.method() == Method::POST {
            return guarded_delete(state, service, req).await;
        }
    }
    #[cfg(feature = "native-model-delete")]
    if path == "/api/v1/models/delete" && req.method() == Method::POST {
        if let Some(delete) = state.model_delete.clone() {
            return native_model_delete(&delete, req, None).await;
        }
    }
    #[cfg(feature = "native-downloads")]
    if path == "/api/v1/models/delete" && req.method() == Method::POST {
        if let Some(table) = python_delete_table(&state) {
            return forward_python_delete(state, table, req, None).await;
        }
    }
    let name = path
        .strip_prefix("/api/v1/backends/")
        .and_then(|p| p.strip_suffix("/logs"))
        .filter(|p| !p.is_empty() && !p.contains('/'));
    if let Some(name) = name {
        if req.method() != Method::GET {
            return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        }
        return native_logs(&state, name, req.uri().query().unwrap_or_default()).await;
    }
    forward(state, req).await
}

/// noevia#1318: the file part natively, then the live fields from the child.
#[cfg(feature = "native-model-detail")]
async fn native_model_detail(
    state: &StateData,
    detail: &model_detail::ModelDetail,
    query: &str,
) -> Response {
    let outcome = match detail.locate(query).await {
        model_detail::Step::Done(outcome) => outcome,
        model_detail::Step::Located(located) => {
            match model_detail_runtime(state, &model_detail::runtime_query(&located)).await {
                Ok(body) => model_detail::splice(&located, &body),
                Err(outcome) => outcome,
            }
        }
    };
    match outcome {
        model_detail::Outcome::Json(status, bytes) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        model_detail::Outcome::Failed => internal_error(),
        model_detail::Outcome::Refused => {
            detail_response(StatusCode::SERVICE_UNAVAILABLE, model_detail::REFUSAL)
        }
        model_detail::Outcome::Unavailable => {
            detail_response(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable")
        }
    }
}

#[cfg(feature = "native-model-detail")]
fn detail_response(status: StatusCode, text: &'static str) -> Response {
    detail(status, text)
}

/// The child's `{"loaded": api._loaded_map(), "fit": services.vram_fit_chips(bytes)}`. Its own
/// unhandled error is Python's 500; anything else it cannot answer is the forwarded 502.
#[cfg(feature = "native-model-detail")]
async fn model_detail_runtime(
    state: &StateData,
    query: &str,
) -> Result<Vec<u8>, model_detail::Outcome> {
    let mut request = state.legacy.get(format!(
        "{}{}?{query}",
        state.legacy_base,
        model_detail::RUNTIME_PATH
    ));
    if !state.token.is_empty() {
        request = request.header("x-model-loader-token", &state.token);
    }
    let response = request
        .send()
        .await
        .map_err(|_| model_detail::Outcome::Unavailable)?;
    let status = response.status();
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| model_detail::Outcome::Unavailable)?;
        if bytes.len().saturating_add(chunk.len()) > model_detail::RUNTIME_CAP {
            return Err(model_detail::Outcome::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    match status {
        StatusCode::OK => Ok(bytes),
        StatusCode::INTERNAL_SERVER_ERROR => Err(model_detail::Outcome::Failed),
        _ => Err(model_detail::Outcome::Unavailable),
    }
}

/// noevia#1318: the store part natively, then the live chart values from the child.
#[cfg(feature = "native-bench-runs")]
async fn native_bench_run(
    state: &StateData,
    runs: &bench_runs::BenchRuns,
    run_id: i64,
) -> Response {
    let outcome = match runs.locate(run_id).await {
        bench_runs::Step::Done(outcome) => outcome,
        bench_runs::Step::Located(located) => {
            let query = bench_runs::runtime_query(&located);
            match child_runtime(
                state,
                bench_runs::RUNTIME_PATH,
                &query,
                bench_runs::RUNTIME_CAP,
            )
            .await
            {
                Ok(body) => bench_runs::splice(&located, &body),
                Err(true) => bench_runs::Outcome::Failed,
                Err(false) => bench_runs::Outcome::Unavailable,
            }
        }
    };
    match outcome {
        bench_runs::Outcome::Json(status, bytes) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        bench_runs::Outcome::Failed => internal_error(),
        bench_runs::Outcome::Unavailable => {
            detail(StatusCode::SERVICE_UNAVAILABLE, bench_runs::REFUSAL)
        }
    }
}

/// Set once the child has answered its health check: its startup (which may migrate models.ini
/// cache-ram values) is over, so this side is the only models.ini writer from then on.
#[cfg(feature = "native-section-writes")]
static CHILD_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "native-section-writes")]
async fn child_started(state: &StateData) -> bool {
    use std::sync::atomic::Ordering;
    if CHILD_STARTED.load(Ordering::Acquire) {
        return true;
    }
    let ok = child_runtime(state, "/api/v1/health", "", 4096)
        .await
        .is_ok();
    if ok {
        CHILD_STARTED.store(true, Ordering::Release);
    }
    ok
}

/// noevia#1318: the models.ini writers. Body and name checks run first (FastAPI's 422s, then
/// the handlers' 400s), then everything else under the one write lock; the store work runs on a
/// blocking thread that owns the lock guard, so a dropped request still finishes before the next.
#[cfg(feature = "native-section-writes")]
async fn native_section_write(
    state: &StateData,
    writes: Arc<section_writes::SectionWrites>,
    route: SectionWrite,
    req: Request,
) -> Response {
    use section_writes::Outcome;
    let unavailable = || {
        detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "Native models.ini writes are unavailable",
        )
    };
    let query = req.uri().query().unwrap_or_default().to_owned();
    // Python reads the body only where the handler has one.
    let body = match route {
        SectionWrite::ModelsIni => {
            // A body this large can only carry a text over the editor limit (or be malformed,
            // which Python would answer with a 422 first; see the docs).
            let over: fn() -> Response = || {
                detail(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "models.ini exceeds the editor limit",
                )
            };
            match fastapi_dict_body_capped(
                req,
                8 * REQUEST_CAP,
                Some((section_writes::MODELS_INI_MAX_BYTES, over)),
            )
            .await
            {
                Ok(body) => Some(body),
                Err(response) => return response,
            }
        }
        SectionWrite::Save(_) | SectionWrite::Rename(_) => match fastapi_dict_body(req).await {
            Ok(body) => Some(body),
            Err(response) => return response,
        },
        _ => None,
    };
    let early = match (&route, &body) {
        (SectionWrite::Save(name), Some(body)) => {
            section_writes::section_body(name, body).map(|b| Some((Some(b), None, None)))
        }
        (SectionWrite::ModelsIni, Some(body)) => {
            section_writes::models_ini_body(body).map(|t| Some((None, Some(t), None)))
        }
        (SectionWrite::Rename(_), Some(body)) => {
            section_writes::rename_body(body).map(|n| Some((None, None, Some(n))))
        }
        (SectionWrite::SafeDefaults(name), _) => {
            section_writes::safe_defaults_name(name).map(|()| None)
        }
        _ => Ok(None),
    };
    let early = match early {
        Ok(e) => e,
        Err(outcome) => return section_outcome(outcome),
    };
    if !child_started(state).await {
        return unavailable();
    }
    let guard = writes.lock().await;
    let plan = if let SectionWrite::SafeDefaults(name) = &route {
        let query: String = form_urlencoded::Serializer::new(String::new())
            .append_pair("name", name)
            .finish();
        match child_runtime(state, section_writes::PLAN_PATH, &query, 1024 * 1024).await {
            Ok(bytes) => match section_writes::parse_plan(&bytes) {
                Some(plan) => Some(plan),
                None => return unavailable(),
            },
            Err(true) => return internal_error(),
            Err(false) => return unavailable(),
        }
    } else {
        None
    };
    let job = {
        let writes = writes.clone();
        let route = route.clone();
        move || -> Outcome {
            let _guard = guard;
            let (section, ini, new) = early.unwrap_or((None, None, None));
            match route {
                SectionWrite::Save(name) => match (body.as_ref(), section) {
                    (Some(body), Some(parsed)) => writes.save_section(&name, body, &parsed),
                    _ => Outcome::Failed,
                },
                SectionWrite::ModelsIni => match (body.as_ref(), ini) {
                    (Some(body), Some((text, backup))) => {
                        writes.replace_models_ini(&text, body.get("baseRevision"), backup)
                    }
                    _ => Outcome::Failed,
                },
                SectionWrite::Rename(old) => match (body.as_ref(), new) {
                    (Some(body), Some(new)) => writes.rename(&old, &new, body.get("baseRevision")),
                    _ => Outcome::Failed,
                },
                SectionWrite::Delete(name) => {
                    let base = form_urlencoded::parse(query.as_bytes())
                        .filter(|(k, _)| k == "baseRevision")
                        .last()
                        .map(|(_, v)| v.into_owned())
                        .unwrap_or_default();
                    writes.delete(&name, &base)
                }
                SectionWrite::SafeDefaults(name) => match plan {
                    Some(plan) => writes.safe_defaults(&name, plan),
                    None => Outcome::Failed,
                },
            }
        }
    };
    // noevia#2006: admitted: a dropped request keeps its permit until the write returns.
    let outcome = docker::spawn_blocking_admitted(job)
        .await
        .unwrap_or(section_writes::Outcome::Failed);
    // #1747: a saved preset may have lost the chat preset's idle key; the answer reports the
    // revision of the file as it is after that is put back, as for the forwarded writers.
    let outcome = match (outcome, state.idle_unload.as_ref()) {
        (Outcome::Json(200, bytes), Some(reconciler)) => {
            Outcome::Json(200, after_native_preset_write(reconciler, bytes).await)
        }
        (other, _) => other,
    };
    section_outcome(outcome)
}

/// Replace the `"revision"` value of a native writer's answer after the idle-unload reconcile,
/// keeping every other byte (its key order included).
#[cfg(feature = "native-section-writes")]
async fn after_native_preset_write(
    reconciler: &idle_unload::Reconciler,
    bytes: Vec<u8>,
) -> Vec<u8> {
    let Ok(Some(revision)) = reconciler.reconcile().await else {
        return bytes;
    };
    let Ok(text) = String::from_utf8(bytes.clone()) else {
        return bytes;
    };
    let Some(start) = text.find("\"revision\":\"") else {
        return bytes;
    };
    let from = start + "\"revision\":\"".len();
    let Some(len) = text[from..].find('"') else {
        return bytes;
    };
    format!("{}{revision}{}", &text[..from], &text[from + len..]).into_bytes()
}

#[cfg(feature = "native-section-writes")]
fn section_outcome(outcome: section_writes::Outcome) -> Response {
    match outcome {
        section_writes::Outcome::Json(status, bytes) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        section_writes::Outcome::Failed => internal_error(),
    }
}

/// `POST /api/v1/backends/{name}/restart|test` with a plain name (no escapes); anything else keeps
/// its existing handling (forwarded).
#[cfg(feature = "native-backend-control")]
pub fn backend_control_route(method: &Method, path: &str) -> Option<(String, &'static str)> {
    if *method != Method::POST {
        return None;
    }
    let rest = path.strip_prefix("/api/v1/backends/")?;
    let (name, action) = rest.split_once('/')?;
    let action = match action {
        "restart" => "restart",
        "test" => "test",
        _ => return None,
    };
    (!name.is_empty() && name.len() <= 256 && name.is_ascii() && !name.contains(['%', '/']))
        .then(|| (name.to_owned(), action))
}

/// `int(body.get("maxTokens") or 256)` clamped to 1..=4096; `None` where Python's int() raises
/// (noevia#1928: a 500 there, a 400 here).
#[cfg(feature = "native-backend-control")]
pub fn max_tokens(value: Option<&prompts::JsonValue>) -> Option<i64> {
    use prompts::JsonValue as J;
    let n: i64 = match value {
        None => 256,
        Some(v) if !v.truthy() => 256,
        Some(J::Bool(_)) => 1,
        Some(J::Int(n)) => {
            let text = n.to_string();
            text.parse().unwrap_or(if text.starts_with('-') {
                i64::MIN
            } else {
                i64::MAX
            })
        }
        Some(J::Float(f)) if f.is_finite() => {
            let t = f.trunc();
            #[allow(clippy::cast_possible_truncation)]
            if t >= 9.2e18 {
                i64::MAX
            } else if t <= -9.2e18 {
                i64::MIN
            } else {
                t as i64
            }
        }
        Some(J::Str(s)) => {
            let t = model_autoconfig::pyval::py_strip(s);
            let (neg, d) = match t.as_bytes().first() {
                Some(b'-') => (true, &t[1..]),
                Some(b'+') => (false, &t[1..]),
                _ => (false, t),
            };
            if d.is_empty()
                || d.starts_with('_')
                || d.ends_with('_')
                || d.contains("__")
                || !d.bytes().all(|b| b.is_ascii_digit() || b == b'_')
            {
                return None;
            }
            let digits: String = d.chars().filter(char::is_ascii_digit).collect();
            let v: i64 = digits.parse().unwrap_or(i64::MAX);
            if neg {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    Some(n.clamp(1, 4096))
}

/// noevia#1318: the child keeps `last_restart_error`; set (`Some`) or clear (`None`) it.
#[cfg(feature = "native-backend-control")]
async fn set_restart_error(
    legacy: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    error: Option<&str>,
) {
    let body = serde_json::json!({"name": name, "error": error}).to_string();
    let mut request = legacy
        .put(format!("{base}{}", backends::RESTART_ERROR_PATH))
        .header("content-type", "application/json")
        .timeout(Duration::from_secs(10))
        .body(body);
    if !token.is_empty() {
        request = request.header("x-model-loader-token", token);
    }
    if request
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
    {
        return;
    }
    eprintln!(
        "model-manager backend restart: the child did not record the restart state for a backend"
    );
}

#[cfg(feature = "native-backend-control")]
async fn native_backend_control(
    state: Arc<StateData>,
    control: Arc<backends::Backends>,
    name: String,
    action: &'static str,
    req: Request,
) -> Response {
    let json = |status: StatusCode, body: String| {
        (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    };
    let body = if action == "test" {
        match fastapi_dict_body(req).await {
            Ok(b) => Some(b),
            Err(response) => return response,
        }
    } else {
        None
    };
    let (inventory, reachable) = match state.docker.containers().await {
        Ok(i) => (i, true),
        Err("docker unreachable") => (Vec::new(), false),
        Err(_) => (Vec::new(), true),
    };
    let names = match effective_names(&state.configured, &inventory) {
        Ok(n) => n,
        Err(e) => return detail(StatusCode::BAD_GATEWAY, e),
    };
    if !names.contains(&name) {
        return detail(StatusCode::NOT_FOUND, "unknown backend");
    }
    let answer = |ok: bool, message: &str| {
        json(
            StatusCode::OK,
            format!(
                "{{\"ok\":{ok},\"message\":{}}}",
                serde_json::to_string(message).unwrap_or_default()
            ),
        )
    };
    if action == "restart" {
        if !reachable {
            return answer(false, "docker unreachable");
        }
        match state.docker.backend_spec(&name).await {
            Ok(Some(_)) => {}
            Ok(None) => return answer(false, "container not found"),
            Err(e) => return answer(false, &format!("docker error: {e}")),
        }
        #[cfg(feature = "native-backends")]
        if let Some(stats) = &state.backend_stats {
            stats.set_restart_error(&name, None);
        }
        set_restart_error(&state.legacy, &state.legacy_base, &state.token, &name, None).await;
        let task_state = state.clone();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                Duration::from_secs(60),
                task_state.docker.restart_backend(&name),
            )
            .await;
            let error = match outcome {
                Ok(Ok(())) => return,
                Ok(Err(e)) => format!("APIError: {e}"),
                Err(_) => "APIError: the restart did not finish in time".to_owned(),
            };
            #[cfg(feature = "native-backends")]
            if let Some(stats) = &task_state.backend_stats {
                stats.set_restart_error(&name, Some(&error));
            }
            set_restart_error(
                &task_state.legacy,
                &task_state.legacy_base,
                &task_state.token,
                &name,
                Some(&error),
            )
            .await;
        });
        return answer(true, "restart queued");
    }
    let Some(body) = body else {
        return internal_error();
    };
    let prompt = match prompts::py_str(body.get("prompt")) {
        Ok(p) => model_autoconfig::pyval::py_strip(&p).to_owned(),
        Err(_) => return internal_error(),
    };
    if prompt.is_empty() {
        return detail(StatusCode::BAD_REQUEST, "prompt is required");
    }
    let Some(max) = max_tokens(body.get("maxTokens")) else {
        return detail(StatusCode::BAD_REQUEST, "maxTokens must be a number");
    };
    match control
        .test(state.docker.as_ref(), &name, &prompt, max)
        .await
    {
        Ok(body) => json(StatusCode::OK, body),
        Err(backends::Raised) => internal_error(),
    }
}

/// noevia#1318: native `PUT /api/v1/settings`.
#[cfg(feature = "native-settings-write")]
async fn native_settings_write(
    state: &StateData,
    writer: &settings_write::SettingsWrite,
    req: Request,
) -> Response {
    let body = match fastapi_dict_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let json = |status: StatusCode, bytes: Vec<u8>| {
        (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response()
    };
    match writer.put(&body).await {
        settings_write::Outcome::Saved(bytes, false) => json(StatusCode::OK, bytes),
        settings_write::Outcome::Saved(bytes, true) => {
            let test = match child_runtime(state, settings_write::TEST_PATH, "", 64 * 1024).await {
                Ok(t) if settings_write::valid_test(&t) => t,
                _ => settings_write::TEST_UNAVAILABLE.as_bytes().to_vec(),
            };
            json(StatusCode::OK, settings_write::with_test(&bytes, &test))
        }
        settings_write::Outcome::Json(status, bytes) => json(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            bytes,
        ),
        settings_write::Outcome::Failed => internal_error(),
    }
}

/// A private GET to the Python child, bounded by `cap`. `Err(true)` is the child's 500 (Python
/// raised), `Err(false)` anything else that leaves no usable answer.
#[cfg(any(
    feature = "native-bench-runs",
    feature = "native-section-writes",
    feature = "native-settings-write"
))]
async fn child_runtime(
    state: &StateData,
    path: &str,
    query: &str,
    cap: usize,
) -> Result<Vec<u8>, bool> {
    if query.len() > 256 * 1024 {
        return Err(true);
    }
    let url = if query.is_empty() {
        format!("{}{path}", state.legacy_base)
    } else {
        format!("{}{path}?{query}", state.legacy_base)
    };
    let mut request = state.legacy.get(url).timeout(Duration::from_secs(60));
    if !state.token.is_empty() {
        request = request.header("x-model-loader-token", &state.token);
    }
    let response = request.send().await.map_err(|_| false)?;
    let status = response.status();
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| false)?;
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(false);
        }
        bytes.extend_from_slice(&chunk);
    }
    match status {
        StatusCode::OK => Ok(bytes),
        StatusCode::INTERNAL_SERVER_ERROR => Err(true),
        _ => Err(false),
    }
}

async fn forward(state: Arc<StateData>, req: Request) -> Response {
    let path = req.uri().path();
    if !python_owned(req.method().as_str(), path) {
        if ["GET", "POST", "PUT", "DELETE"]
            .iter()
            .any(|method| python_owned(method, path))
        {
            return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
        }
        return detail(StatusCode::NOT_FOUND, "Not Found");
    }
    let (parts, body) = req.into_parts();
    let preset_write = idle_unload::writes_models_ini(parts.method.as_str(), parts.uri.path());
    #[cfg(feature = "native-downloads")]
    let overview = parts.method == Method::GET && parts.uri.path() == "/api/v1/overview";
    let bytes = match to_bytes(body, REQUEST_CAP).await {
        Ok(b) => b,
        Err(_) => return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit"),
    };
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut legacy = state
        .legacy
        .request(parts.method, format!("{}{target}", state.legacy_base))
        .body(bytes);
    if let Some(content_type) = parts.headers.get("content-type") {
        legacy = legacy.header("content-type", content_type);
    }
    if !state.token.is_empty() {
        legacy = legacy.header("x-model-loader-token", &state.token);
    }
    let response = match legacy.send().await {
        Ok(r) => r,
        Err(_) => return detail(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable"),
    };
    let status = response.status();
    let mut chunks = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => return detail(StatusCode::BAD_GATEWAY, "Python-owned response unavailable"),
        };
        if bytes.len().saturating_add(chunk.len()) > 16 * LOG_CAP {
            return detail(
                StatusCode::BAD_GATEWAY,
                "Python-owned response exceeded limit",
            );
        }
        if bytes.try_reserve(chunk.len()).is_err() {
            return detail(StatusCode::SERVICE_UNAVAILABLE, "allocation refused");
        }
        bytes.extend_from_slice(&chunk);
    }
    if preset_write && status.is_success() {
        if let Some(reconciler) = &state.idle_unload {
            bytes = after_preset_write(reconciler, bytes).await;
        }
    }
    #[cfg(feature = "native-downloads")]
    if overview && status == StatusCode::OK {
        if let Some(table) = &state.downloads {
            if let Some(patched) = downloads::patch_overview(&bytes, table.active()) {
                bytes = patched;
            }
        }
    }
    (
        status,
        [("content-type", "application/json")],
        Body::from(bytes),
    )
        .into_response()
}

/// #1747: a models.ini save may have dropped the chat preset's global idle key (the raw editor
/// saves whatever text it was given). Put it back, and report the revision of the file as it now
/// is, so the editor's next save is not refused as a conflict with our own rewrite. Only the
/// models.ini save routes get here, and their `revision` is always the models.ini revision.
async fn after_preset_write(reconciler: &idle_unload::Reconciler, bytes: Vec<u8>) -> Vec<u8> {
    let Ok(Some(revision)) = reconciler.reconcile().await else {
        return bytes;
    };
    let Ok(serde_json::Value::Object(mut body)) = serde_json::from_slice(&bytes) else {
        return bytes;
    };
    if !matches!(body.get("revision"), Some(serde_json::Value::String(_))) {
        return bytes;
    }
    body.insert("revision".into(), serde_json::Value::String(revision));
    serde_json::to_vec(&body).unwrap_or(bytes)
}

/// noevia#1863: the chat-model idle unload setting. Native only: the Python child never owned it.
pub const IDLE_UNLOAD_ROUTE: &str = "/api/v1/idle-unload";

/// `GET` reports the effective timeout; `PUT {"seconds": n}` saves an admin choice (0 is
/// "Never", otherwise 300..=86400) and `PUT {"seconds": null}` clears it, back to the env seed.
/// The chat preset's `[*]` key is rewritten before the answer; the caller reloads the router.
async fn native_idle_unload(reconciler: &idle_unload::Reconciler, req: Request) -> Response {
    match *req.method() {
        Method::GET => Json(reconciler.view().json()).into_response(),
        Method::PUT => {
            if !reconciler.writable() {
                return detail(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "the idle unload setting cannot be saved here",
                );
            }
            let Ok(raw) = to_bytes(req.into_body(), 4096).await else {
                return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
            };
            let Ok(serde_json::Value::Object(body)) = serde_json::from_slice(&raw) else {
                return detail(StatusCode::BAD_REQUEST, "invalid idle unload request");
            };
            let value = match body.get("seconds") {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::Number(n)) => {
                    match n.as_u64().and_then(idle_unload::admin_value) {
                        Some(v) => Some(v),
                        None => {
                            return detail(
                                StatusCode::BAD_REQUEST,
                                "seconds must be 0 (never) or 300 to 86400",
                            )
                        }
                    }
                }
                _ => {
                    return detail(
                        StatusCode::BAD_REQUEST,
                        "seconds must be 0 (never), 300 to 86400, or null",
                    )
                }
            };
            match reconciler.update(value).await {
                Ok((view, preset)) => {
                    let mut out = view.json();
                    let (state, error) = match preset {
                        idle_unload::Preset::Written => ("written", None),
                        idle_unload::Preset::Unchanged => ("unchanged", None),
                        idle_unload::Preset::Failed(e) => ("failed", Some(e)),
                    };
                    if let serde_json::Value::Object(map) = &mut out {
                        map.insert("preset".into(), json!(state));
                        if let Some(e) = error {
                            map.insert("presetError".into(), json!(e));
                        }
                    }
                    Json(out).into_response()
                }
                Err(_) => detail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the idle unload setting could not be saved",
                ),
            }
        }
        _ => detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed"),
    }
}

async fn native_embedding(service: &Arc<embedding::EmbeddingService>, req: Request) -> Response {
    match *req.method() {
        Method::GET => Json(service.status().await).into_response(),
        Method::PUT => {
            let Ok(raw) = to_bytes(req.into_body(), 16 * 1024).await else {
                return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
            };
            let Ok(desired) = serde_json::from_slice::<embedding::Desired>(&raw) else {
                return detail(StatusCode::BAD_REQUEST, "invalid embedding service request");
            };
            match service.apply(desired).await {
                Ok(_) => (StatusCode::ACCEPTED, Json(service.status().await)).into_response(),
                Err(embedding::Refusal::Invalid(text)) => detail(StatusCode::BAD_REQUEST, text),
                Err(embedding::Refusal::Unavailable(text)) => {
                    detail(StatusCode::SERVICE_UNAVAILABLE, text)
                }
            }
        }
        _ => detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed"),
    }
}

/// #336/#1668: the embedding router holds its model through the preset, not its command line,
/// so the legacy holder scan cannot see it. Refuse here, before anything is forwarded.
async fn guarded_delete(
    state: Arc<StateData>,
    service: Arc<embedding::EmbeddingService>,
    req: Request,
) -> Response {
    let deadline = route_deadline(&req);
    let (parts, body) = req.into_parts();
    let Ok(raw) = to_bytes(body, REQUEST_CAP).await else {
        return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
    };
    let keys: Vec<String> = serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("models").and_then(|m| m.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .map(|k| match k {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    // Held across the check and the delete, so a concurrent switch cannot choose a model whose
    // file this request is deleting. noevia#1997: owned, it moves into the task doing the delete
    // and is released when that work ends, not when this request future is dropped. Waiting for
    // it, and a forward under it, end at the route's deadline.
    let Ok(held) = tokio::time::timeout_at(deadline, service.hold_owned()).await else {
        return timed_out();
    };
    for key in &keys {
        if service.protects(key).await {
            return detail(
                StatusCode::CONFLICT,
                "The embedding service is set to run this model or still has it loaded. Switch embedding to another model or turn embedding off first.",
            );
        }
    }
    let req = Request::from_parts(parts, Body::from(raw));
    // noevia#1318: under the hold, so the embedding service cannot switch to a model mid-delete.
    #[cfg(feature = "native-model-delete")]
    if let Some(delete) = state.model_delete.clone() {
        return native_model_delete(&delete, req, Some(held)).await;
    }
    #[cfg(feature = "native-downloads")]
    if let Some(table) = python_delete_table(&state) {
        return forward_python_delete(state, table, req, Some(held)).await;
    }
    // noevia#1997/#1998: the forward runs in its own admitted task that owns the hold, until
    // the route's deadline at the latest.
    docker::spawn_admitted(async move {
        let response = tokio::time::timeout_at(deadline, forward(state, req))
            .await
            .unwrap_or_else(|_| timed_out());
        drop(held);
        response
    })
    .await
    .unwrap_or_else(|_| internal_error())
}

/// The 409 for a Python-served delete while the native table has a queued or running download.
#[cfg(feature = "native-downloads")]
pub const PYTHON_DELETE_WHILE_DOWNLOADING: &str = "A download is queued or running; wait for it to finish or cancel it before deleting a model. Nothing was deleted.";

/// The native download table when a delete is served by Python (native downloads, Python
/// delete: the mixed setup).
#[cfg(feature = "native-downloads")]
fn python_delete_table(state: &StateData) -> Option<Arc<downloads::Downloads>> {
    #[cfg(feature = "native-model-delete")]
    if state.model_delete.is_some() {
        return None;
    }
    state.downloads.clone()
}

/// noevia#1966: with native downloads but a Python delete, Python cannot see the native jobs, so
/// any active download refuses the delete before it reaches Python (coarser than the native
/// per-folder check, never a deletion under a running transfer).
///
/// noevia#1984: nor can this table see Python's delete reservations, and a URL or draft-head
/// start may be waiting on the child's plan while the delete runs. So the whole table is
/// reserved (taken before the active check, as the native delete does) and every start answers
/// `Deleting` until the forwarded request has ended. The forward runs in its own task that owns
/// the reservation: a request dropped mid-forward (client gone, route timeout) does not release
/// it while Python may still be deleting, only once the child has answered or the forward has
/// failed or reached the route's deadline ([`LEGACY_TIMEOUT`], 600 s after the request
/// arrived). At that deadline it is released even if Python is still deleting: this side cannot
/// see past it.
///
/// noevia#1994: the request body is read before the reservation is taken (as `guarded_delete`
/// does), so a client whose body stalls holds nothing; only the forward itself runs under it.
///
/// noevia#1997: the embedding service's hold (`guarded_delete`), when given, moves into the same
/// task. noevia#1998: that task is admitted ([`docker::spawn_admitted`]), so a dropped request
/// also keeps its admission permit until the forward has ended. The forward ends at the route's
/// deadline at the latest, so the timeout answer goes out by the route's budget.
#[cfg(feature = "native-downloads")]
async fn forward_python_delete(
    state: Arc<StateData>,
    table: Arc<downloads::Downloads>,
    req: Request,
    embedding: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> Response {
    let deadline = route_deadline(&req);
    let (parts, body) = req.into_parts();
    let Ok(raw) = to_bytes(body, REQUEST_CAP).await else {
        return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
    };
    let req = Request::from_parts(parts, Body::from(raw));
    let held = table.reserve_all();
    if table.active() > 0 {
        return detail(StatusCode::CONFLICT, PYTHON_DELETE_WHILE_DOWNLOADING);
    }
    docker::spawn_admitted(async move {
        let response = tokio::time::timeout_at(deadline, forward(state, req))
            .await
            .unwrap_or_else(|_| timed_out());
        drop(held);
        drop(embedding);
        response
    })
    .await
    .unwrap_or_else(|_| internal_error())
}

/// noevia#1318: `POST /api/v1/models/delete` with FastAPI's `dict` body rules, answered natively.
/// noevia#1997: `embedding` (the embedding service's hold) is kept until the last removal ends.
#[cfg(feature = "native-model-delete")]
async fn native_model_delete(
    delete: &model_delete::ModelDelete,
    req: Request,
    embedding: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> Response {
    let body = match fastapi_dict_body_capped(req, REQUEST_CAP, None).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match delete.post_holding(&body, embedding).await {
        model_delete::Outcome::Json(bytes) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        model_delete::Outcome::Conflict(text) => {
            (StatusCode::CONFLICT, Json(json!({ "detail": text }))).into_response()
        }
        model_delete::Outcome::Failed => internal_error(),
        model_delete::Outcome::Refused => {
            detail(StatusCode::SERVICE_UNAVAILABLE, model_delete::REFUSAL)
        }
    }
}

/// noevia#1318: the routes the native benchmark runner answers.
#[cfg(feature = "native-benchmark-runner")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchRoute {
    Start,
    Sweep,
    Progress,
    Cancel,
}

#[cfg(feature = "native-benchmark-runner")]
pub fn bench_runner_route(method: &Method, path: &str) -> Option<BenchRoute> {
    match (method.as_str(), path) {
        ("POST", "/api/v1/benchmark/start") => Some(BenchRoute::Start),
        ("POST", "/api/v1/benchmark/sweep") => Some(BenchRoute::Sweep),
        ("GET", "/api/v1/benchmark/progress") => Some(BenchRoute::Progress),
        ("POST", "/api/v1/benchmark/cancel") => Some(BenchRoute::Cancel),
        _ => None,
    }
}

/// `{"job": _bench_job()}`.
#[cfg(feature = "native-benchmark-runner")]
fn job_response(runner: &bench_runner::Runner) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("{{\"job\":{}}}", runner.job()),
    )
        .into_response()
}

#[cfg(feature = "native-benchmark-runner")]
async fn native_bench_runner(
    runner: &Arc<bench_runner::Runner>,
    route: BenchRoute,
    req: Request,
) -> Response {
    match route {
        BenchRoute::Progress => job_response(runner),
        BenchRoute::Cancel => {
            runner.cancel();
            job_response(runner)
        }
        BenchRoute::Start => {
            let body = match fastapi_dict_body(req).await {
                Ok(b) => b,
                Err(response) => return response,
            };
            let Ok(request) = bench_runner::start_request(&body) else {
                return internal_error();
            };
            match runner.start(request).await {
                Ok(Ok(())) => job_response(runner),
                Ok(Err(message)) => {
                    (StatusCode::CONFLICT, Json(json!({"detail": message}))).into_response()
                }
                Err(_) => internal_error(),
            }
        }
        BenchRoute::Sweep => {
            let body = match fastapi_dict_body(req).await {
                Ok(b) => b,
                Err(response) => return response,
            };
            let Ok(request) = bench_runner::sweep_request(&body) else {
                return internal_error();
            };
            match runner.start_sweep(request).await {
                Ok(Ok(())) => job_response(runner),
                Ok(Err(message)) => {
                    (StatusCode::CONFLICT, Json(json!({"detail": message}))).into_response()
                }
                Err(_) => internal_error(),
            }
        }
    }
}

/// noevia#1318: `GET /api/v1/benchmark`: the store, models.ini and Docker reads natively, then
/// the runner's live `job` from the child (which owns the runner thread).
#[cfg(feature = "native-benchmark-overview")]
async fn native_benchmark_overview(
    state: &StateData,
    overview: &benchmark_overview::BenchmarkOverview,
) -> Response {
    let Ok(local) = overview.local().await else {
        return internal_error();
    };
    // `discover_llama_containers` answers [] for any Docker failure.
    let inventory = state.docker.containers().await.unwrap_or_default();
    let Ok(backends) = effective_names(&state.configured, &inventory) else {
        return internal_error();
    };
    #[cfg(feature = "native-benchmark-runner")]
    if let Some(runner) = &state.bench_runner {
        let job = runner.job().into_bytes();
        return match benchmark_overview::render(&local, &backends, &job) {
            Some(bytes) => (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                bytes,
            )
                .into_response(),
            None => internal_error(),
        };
    }
    let job = match child_runtime(
        state,
        benchmark_overview::JOB_PATH,
        "",
        benchmark_overview::JOB_CAP,
    )
    .await
    {
        Ok(job) if benchmark_overview::job_object(&job) => job,
        Ok(_) | Err(true) => return internal_error(),
        Err(false) => return detail(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable"),
    };
    match benchmark_overview::render(&local, &backends, &job) {
        Some(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        None => internal_error(),
    }
}

/// A FastAPI `dict` body read exactly as the prompt writer reads it (content type, CPython JSON
/// encodings, the 1 MiB cap and the same 422 bodies).
#[cfg(any(
    feature = "native-badges",
    feature = "native-section-writes",
    feature = "native-settings-write",
    feature = "native-backend-control",
    feature = "native-benchmark-runner"
))]
#[allow(clippy::result_large_err)]
async fn fastapi_dict_body(req: Request) -> Result<prompts::JsonValue, Response> {
    fastapi_dict_body_capped(req, REQUEST_CAP, None).await
}

/// [`fastapi_dict_body`] with a larger read cap; a body over `over.0` bytes answers `over.1`
/// (the route's own size refusal) before it is parsed.
#[cfg(any(
    feature = "native-badges",
    feature = "native-section-writes",
    feature = "native-settings-write",
    feature = "native-backend-control",
    feature = "native-model-delete"
))]
#[allow(clippy::result_large_err)]
async fn fastapi_dict_body_capped(
    req: Request,
    cap: usize,
    over: Option<(usize, fn() -> Response)>,
) -> Result<prompts::JsonValue, Response> {
    // A missing content type is JSON, as for the prompt writer.
    let header = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .map(|value| value.to_str().ok().filter(|v| v.len() <= 8192));
    let json_content = if let Some(value) = header {
        let Some(value) = value else {
            return Err(prompt_error(prompts::Error::Body));
        };
        prompts::json_content_type(Some(value))
    } else {
        true
    };
    let Ok(bytes) = to_bytes(req.into_body(), cap).await else {
        return Err(detail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeded limit",
        ));
    };
    if let Some((limit, answer)) = over {
        if bytes.len() > limit {
            return Err(answer());
        }
    }
    if !json_content && !bytes.is_empty() {
        return Err(match prompts::non_json_validation(&bytes) {
            Ok(error) => (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response(),
            Err(e) => prompt_error(e),
        });
    }
    let decoded = prompts::decode_body(&bytes).map_err(prompt_error)?;
    if let Some(error) = prompts::validation_with_empty(decoded.as_bytes(), bytes.is_empty()) {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response());
    }
    match prompts::parse_json(&decoded) {
        Ok(value @ prompts::JsonValue::Obj(_)) => Ok(value),
        Ok(_) => Err(prompt_error(prompts::Error::Object)),
        Err(e) => Err(prompt_error(e)),
    }
}

/// noevia#1318: `PUT` reads a FastAPI `dict` body exactly as the prompt writer does (same
/// content-type, encoding, 422 and size rules); `DELETE` reads its two required query values.
#[cfg(feature = "native-badges")]
async fn native_badges(badges: &badges::Badges, req: Request) -> Response {
    let outcome = if req.method() == Method::DELETE {
        badges.delete(req.uri().query().unwrap_or_default()).await
    } else {
        let body = match fastapi_dict_body(req).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        badges.put(&body).await
    };
    match outcome {
        badges::Outcome::Json(status, bytes) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        badges::Outcome::Failed => internal_error(),
    }
}

#[cfg(feature = "native-prompts")]
fn prompt_error(error: prompts::Error) -> Response {
    let status = match error {
        prompts::Error::Input
        | prompts::Error::Body
        | prompts::Error::Required
        | prompts::Error::Text => StatusCode::BAD_REQUEST,
        prompts::Error::Object => StatusCode::UNPROCESSABLE_ENTITY,
        prompts::Error::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"detail":error.to_string()}))).into_response()
}

async fn native_sections(sections: &sections::Sections, path: &str, query: &str) -> Response {
    let name = path.strip_prefix("/api/v1/sections/");
    let decoded = name.map(decode_segment);
    // FastAPI matches the decoded route before validating its query parameters.
    if decoded
        .as_ref()
        .is_some_and(|result| matches!(result, Ok(name) if name.contains('/')))
    {
        return detail(StatusCode::NOT_FOUND, "Not Found");
    }
    let parsed = reqwest::Url::parse(&format!("http://localhost/?{query}"));
    let mut defaults = false;
    if let Ok(url) = parsed {
        if let Some((_, value)) = url
            .query_pairs()
            .filter(|(key, _)| key == "defaults")
            .last()
        {
            let low = value.to_lowercase();
            defaults = match low.as_str() {
                "1" | "true" | "on" | "yes" | "y" | "t" => true,
                "0" | "false" | "off" | "no" | "n" | "f" => false,
                _ if name.is_some() => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(validation::parsing(
                            "bool_parsing",
                            &[validation::key("query"), validation::key("defaults")],
                            "Input should be a valid boolean, unable to interpret input",
                            &value,
                        )),
                    )
                        .into_response()
                }
                _ => false,
            };
        }
    }
    let name = match decoded {
        Some(result) => match result {
            Ok(name) => Some(name),
            Err(_) => return detail(StatusCode::BAD_REQUEST, "invalid section name"),
        },
        None => None,
    };
    match sections.response(name, defaults).await {
        Ok(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(sections::Error::InvalidName) => {
            detail(StatusCode::BAD_REQUEST, "invalid section name")
        }
        Err(_) => detail(StatusCode::SERVICE_UNAVAILABLE, sections::REFUSAL),
    }
}
fn decode_segment(raw: &str) -> Result<String, ()> {
    let mut bytes = Vec::new();
    bytes.try_reserve(raw.len()).map_err(|_| ())?;
    let mut chars = raw.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let a = char::from(chars.next().ok_or(())?).to_digit(16).ok_or(())?;
            let b = char::from(chars.next().ok_or(())?).to_digit(16).ok_or(())?;
            bytes.push((a * 16 + b) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| ())
}

async fn native_logs(state: &StateData, name: &str, query: &str) -> Response {
    if name.len() > 256 || !name.is_ascii() || name.contains('%') {
        return detail(StatusCode::BAD_REQUEST, "invalid backend name");
    }
    let params: std::collections::HashMap<_, _> =
        reqwest::Url::parse(&format!("http://localhost/?{query}"))
            .map(|u| {
                u.query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            })
            .unwrap_or_default();
    let q = params.get("q").map(String::as_str).unwrap_or_default();
    let level = params.get("level").map(String::as_str).unwrap_or_default();
    let tail = match parse_tail(params.get("tail").map(String::as_str)) {
        Ok(t) => t,
        Err(_) => return detail(StatusCode::UNPROCESSABLE_ENTITY, "invalid tail"),
    };
    let inventory = match state.docker.containers().await {
        Ok(i) => i,
        Err("docker unreachable") => Vec::new(),
        Err(e) => return detail(StatusCode::BAD_GATEWAY, e),
    };
    let names = match effective_names(&state.configured, &inventory) {
        Ok(n) => n,
        Err(e) => return detail(StatusCode::BAD_GATEWAY, e),
    };
    if !names.iter().any(|n| n == name) {
        return detail(StatusCode::NOT_FOUND, "unknown backend");
    }
    match state
        .docker
        .logs(name, tail)
        .await
        .and_then(|raw| state.redactor.filter(&raw, q, level))
    {
        Ok(lines) => Json(json!({"ok":true,"lines":lines})).into_response(),
        Err(e) => Json(json!({"ok":false,"error":e,"lines":[]})).into_response(),
    }
}

pub fn parse_tail(value: Option<&str>) -> Result<usize, &'static str> {
    let Some(value) = value else {
        return Ok(400);
    };
    let value = value.trim();
    let (negative, digits) = if let Some(d) = value.strip_prefix('-') {
        (true, d)
    } else {
        (false, value.strip_prefix('+').unwrap_or(value))
    };
    if digits.is_empty() || digits.len() > 4096 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid tail");
    }
    let number = digits.bytes().fold(0usize, |n, b| {
        n.saturating_mul(10)
            .saturating_add(usize::from(b - b'0'))
            .min(2001)
    });
    Ok(if number == 0 {
        400
    } else if negative {
        20
    } else {
        number.clamp(20, 2000)
    })
}

/// noevia#1318: the routes the native download job table answers.
#[cfg(feature = "native-downloads")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadRoute {
    List,
    Start,
    Cancel(String),
    Clear,
}

/// `urllib.parse.unquote` as uvicorn applies it to the path: valid `%XX` escapes decoded, the
/// bytes read as UTF-8 with replacement, invalid escapes kept.
#[cfg(feature = "native-downloads")]
fn py_unquote(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = |j: usize| bytes.get(j).and_then(|c| char::from(*c).to_digit(16));
        match (b, hex(i + 1), hex(i + 2)) {
            (b'%', Some(h), Some(l)) => {
                out.push(u8::try_from(h * 16 + l).unwrap_or(0));
                i += 3;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Which download route a request is, on the path as the child would decode it. A segment
/// that decodes to contain `/` is not a route there (404), so it is left to the child.
#[cfg(feature = "native-downloads")]
pub fn downloads_route(method: &Method, path: &str) -> Option<DownloadRoute> {
    let parts: Vec<&str> = path.split('/').collect();
    let segment = |raw: &str| {
        let decoded = py_unquote(raw);
        (!decoded.is_empty() && !decoded.contains('/')).then_some(decoded)
    };
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["", "api", "v1", "downloads"]) => Some(DownloadRoute::List),
        ("POST", ["", "api", "v1", "downloads"]) => Some(DownloadRoute::Start),
        ("POST", ["", "api", "v1", "downloads", "clear"]) => Some(DownloadRoute::Clear),
        ("POST", ["", "api", "v1", "downloads", id, "cancel"]) => {
            segment(id).map(DownloadRoute::Cancel)
        }
        ("POST", ["", "api", "v1", "sections", name, "draft-heads", "download"]) => {
            segment(name).map(|_| DownloadRoute::Start)
        }
        _ => None,
    }
}

#[cfg(feature = "native-downloads")]
fn json_bytes(status: StatusCode, body: impl Into<Body>) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.into(),
    )
        .into_response()
}

/// noevia#1318: the download routes. A start is planned by the child (the same route, run
/// without queueing) and its files are queued here; everything else is this side's table.
#[cfg(feature = "native-downloads")]
async fn native_downloads(
    state: &StateData,
    table: &Arc<downloads::Downloads>,
    route: DownloadRoute,
    req: Request,
) -> Response {
    match route {
        DownloadRoute::List => json_bytes(StatusCode::OK, table.list()),
        DownloadRoute::Cancel(id) => {
            json_bytes(StatusCode::OK, format!("{{\"ok\":{}}}", table.cancel(&id)))
        }
        DownloadRoute::Clear => {
            json_bytes(StatusCode::OK, format!("{{\"cleared\":{}}}", table.clear()))
        }
        DownloadRoute::Start => {
            let (parts, body) = req.into_parts();
            let Ok(bytes) = to_bytes(body, REQUEST_CAP).await else {
                return detail(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeded limit");
            };
            let target = parts.uri.path_and_query().map_or("/", |p| p.as_str());
            let mut request = state
                .legacy
                .post(format!("{}{target}", state.legacy_base))
                .header(downloads::PLAN_HEADER, "1")
                .body(bytes);
            if let Some(content_type) = parts.headers.get("content-type") {
                request = request.header("content-type", content_type);
            }
            if !state.token.is_empty() {
                request = request.header("x-model-loader-token", &state.token);
            }
            let Ok(response) = request.send().await else {
                return detail(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable");
            };
            let status = response.status();
            let mut chunks = response.bytes_stream();
            let mut answer = Vec::new();
            while let Some(chunk) = chunks.next().await {
                let Ok(chunk) = chunk else {
                    return detail(StatusCode::BAD_GATEWAY, "Python-owned response unavailable");
                };
                if answer.len().saturating_add(chunk.len()) > downloads::PLAN_CAP {
                    return detail(
                        StatusCode::BAD_GATEWAY,
                        "Python-owned response exceeded limit",
                    );
                }
                answer.extend_from_slice(&chunk);
            }
            if status == StatusCode::INTERNAL_SERVER_ERROR {
                return internal_error();
            }
            let plan = match (status, downloads::parse_plan(&answer)) {
                (StatusCode::OK, Some(plan)) => plan,
                _ => return detail(StatusCode::BAD_GATEWAY, "Python-owned endpoint unavailable"),
            };
            let Ok(code) = StatusCode::from_u16(plan.status) else {
                return internal_error();
            };
            if code == StatusCode::OK {
                match table.queue(plan.jobs) {
                    downloads::Queued::Done => {}
                    downloads::Queued::Raised => return internal_error(),
                    downloads::Queued::Deleting => {
                        return detail(StatusCode::CONFLICT, downloads::DELETE_IN_PROGRESS)
                    }
                    downloads::Queued::Full => {
                        return detail(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "too many downloads are held; clear the finished ones",
                        )
                    }
                }
            }
            json_bytes(code, plan.body)
        }
    }
}

/// Explicit Python-owned endpoint/method table. Native-owned paths never enter this table.
pub fn python_owned(method: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/") else {
        return false;
    };
    let segments: Vec<_> = rest.split('/').collect();
    matches!(
        (method, segments.as_slice()),
        (
            "GET",
            ["overview"
                | "models"
                | "sections"
                | "backends"
                | "host"
                | "settings"
                | "search"
                | "downloads"
                | "download-targets"
                | "prompts"
                | "benchmark"]
        ) | ("GET", ["models", "detail" | "updates"])
            | ("GET", ["search", "repo"])
            | ("GET", ["benchmark", "progress"])
            | ("GET", ["sections", _, "draft-heads" | "autoconfig"])
            | ("GET", ["sections", _])
            | ("GET", ["backends", _, "diagnose"])
            | ("GET", ["benchmark", "runs", _])
            | ("POST", ["models", "delete" | "check-updates"])
            | ("POST", ["sections", _, "safe-defaults" | "rename"])
            | ("POST", ["sections", _, "draft-heads", "download"])
            | ("POST", ["backends", _, "restart" | "test"])
            | ("POST", ["downloads" | "prompts"])
            | ("POST", ["downloads", _, "cancel"])
            | ("POST", ["downloads", "clear"])
            | ("POST", ["benchmark", "start" | "sweep" | "cancel"])
            | ("PUT", ["sections", _])
            | ("PUT", ["models-ini" | "settings" | "badges"])
            | ("DELETE", ["sections" | "prompts", _])
            | ("DELETE", ["badges"])
    )
}
