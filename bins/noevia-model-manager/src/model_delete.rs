//! noevia#1318: native `POST /api/v1/models/delete`.
//!
//! Python (`model-manager/app/api.py` and `services.py`):
//!
//! ```python
//! for item in body.get("models") or []:
//!     key = str(item)
//!     g = _find_entry(key)                       # 400 "bad model key" / 404 "model not found"
//!     containers = services._running_containers()            # once; fails closed with a 409
//!     holders = services.model_holders(g, containers)        # any holder: 409, nothing deleted
//!     todo.append((key, g))
//! for key, g in todo:
//!     ok, msg, freed = services.delete_gguf(g.display_name, g.subdir)
//! return {"results": results}
//! ```
//!
//! Everything is native: the key check, the models-folder snapshot with its ini aliases (shared
//! with the native model detail read), the container check (`docker inspect` of every running
//! container: command-line arguments, mounts and the embedding/reranker env names, with the
//! #1668 embedding-service exception), and `delete_gguf` with its three shapes (a model's own
//! folder removed whole, per-file deletion in a shared or protected folder, and the Hugging Face
//! cache layout with its blobs), the protected-folder rules (models root, download locations,
//! symlinked folders, mount points from `/proc/self/mountinfo`) and the tidy-up.
//!
//! Every file operation goes through directory handles opened from the models folder without
//! following links, so a folder swapped for a link after the snapshot is never descended.
//!
//! #1966: a queued or running native download into the model's folder refuses the delete (409),
//! as services#81 does against Python's job table.
//!
//! Deliberate differences (see `docs/native-model-delete.md`): a Docker socket that exists but cannot be read refuses with the 409 (Python treats a socket it
//! cannot connect to as "no Docker"); conditions the bounded reader cannot follow exactly answer
//! the native 503 before anything is deleted.
mod fs;

use crate::model_detail::{snapshot, Entry, Snapshot};
use crate::model_updates::{exists, json_string, Stop};
use crate::prompts::JsonValue;
use crate::sections::{access::Root, Error as WalkError};
use crate::DockerRead;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Body of the native refusal (503) when the bounded reader cannot answer exactly.
pub const REFUSAL: &str = "Native model delete is unavailable";
/// `_MODEL_ENV_KEYS`, in Python's order.
const MODEL_ENV_KEYS: [&str; 6] = [
    "EMBEDDING_MODEL",
    "EMBED_MODEL",
    "EMBEDDING_NEXT_MODEL",
    "LLM_EMBED_MODEL",
    "LLM_EMBED_NEXT_MODEL",
    "RERANK_MODEL",
];
/// Most keys one request may name (each is a snapshot lookup and possibly a deletion).
pub const KEY_CAP: usize = 4096;

/// What `docker inspect` says about one running container, as `model_holders` reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Running {
    pub name: String,
    pub id: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub args: Vec<String>,
    pub env: Vec<String>,
    /// `(Destination, Source)` of every mount.
    pub mounts: Vec<(String, String)>,
}

/// The settings `delete_gguf` and `model_holders` read.
#[derive(Debug, Clone)]
pub struct Config {
    /// `settings.models_dir` as configured (absolute and normalised).
    pub models: String,
    /// `settings.models_ini_path`.
    pub ini: PathBuf,
    /// `settings.models_host_path`.
    pub host_path: String,
    /// `settings.model_download_targets`.
    pub targets: String,
    /// `services.MOUNTINFO_PATH`.
    pub mountinfo: PathBuf,
    /// `HOSTNAME`: this container, skipped.
    pub hostname: String,
    /// #1668: the guarded embedding service's container and address (`None`: not guarded).
    pub embed: Option<(String, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(Vec<u8>),
    /// HTTPException(409, detail).
    Conflict(String),
    /// An unhandled error: FastAPI's plain 500.
    Failed,
    /// The native 503: nothing was deleted.
    Refused,
}

pub struct ModelDelete {
    config: Arc<Config>,
    docker: Arc<dyn DockerRead>,
    /// noevia#1966: the native download table (this process serves downloads when it deletes).
    downloads: Option<Arc<crate::downloads::Downloads>>,
    open: fn(&Path) -> Result<Root, WalkError>,
    lock: tokio::sync::Mutex<()>,
}

/// One phase-two answer: `(ok, message, freed)`.
type Deleted = (bool, String, u128);

impl ModelDelete {
    pub fn new(config: Config, docker: Arc<dyn DockerRead>) -> Self {
        Self::with_root(config, docker, Root::new)
    }

    /// Tests open the models root with the synthetic (non-Linux) reader.
    pub fn with_root(
        config: Config,
        docker: Arc<dyn DockerRead>,
        open: fn(&Path) -> Result<Root, WalkError>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            docker,
            downloads: None,
            open,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// noevia#1966: refuse while a queued or running download writes into a model's folder.
    #[must_use]
    pub fn with_downloads(mut self, downloads: Arc<crate::downloads::Downloads>) -> Self {
        self.downloads = Some(downloads);
        self
    }

    /// Destinations of active downloads that land on one of `entry`'s files or inside the folder
    /// a delete may remove (its subdir, or the Hugging Face repository folder).
    fn download_blockers(&self, entry: &Entry) -> Vec<String> {
        let Some(table) = &self.downloads else {
            return Vec::new();
        };
        blockers(entry, &table.active_destinations())
    }

    /// `api.delete_models(body)` for a body FastAPI accepted as a `dict`.
    pub async fn post(&self, body: &JsonValue) -> Outcome {
        // One delete at a time: Python's handlers may interleave, but a second request must
        // never act on a snapshot the first is changing.
        let _held = self.lock.lock().await;
        let keys = match keys(body) {
            Ok(keys) => keys,
            Err(outcome) => return outcome,
        };
        let snap = match self.snapshot().await {
            Ok(snap) => snap,
            Err(Stop::Failed) => return Outcome::Failed,
            Err(Stop::Refused) => return Outcome::Refused,
        };
        let mut results = Vec::new();
        let mut todo: Vec<(String, Arc<Entry>)> = Vec::new();
        let mut containers: Option<Vec<Running>> = None;
        for key in keys {
            if key.contains("..") || key.starts_with('/') || key.matches('/').count() > 4 {
                results.push(failure(&key, "bad model key"));
                continue;
            }
            let found = snap.as_ref().and_then(|(_, entries)| {
                entries
                    .iter()
                    .find(|g| {
                        g.route_key() == key || (g.model.subdir.is_empty() && g.model.base == key)
                    })
                    .cloned()
            });
            let Some(entry) = found else {
                results.push(failure(&key, "model not found"));
                continue;
            };
            if entry.unknowable {
                return Outcome::Refused;
            }
            if containers.is_none() {
                match self.running().await {
                    Ok(list) => containers = Some(list),
                    Err(message) => return Outcome::Conflict(message),
                }
            }
            let holders = holders(&self.config, &entry, containers.as_deref().unwrap_or(&[]));
            if !holders.is_empty() {
                return Outcome::Conflict(in_use_message(&entry, &holders));
            }
            let downloads = self.download_blockers(&entry);
            if !downloads.is_empty() {
                return Outcome::Conflict(downloading_message(&entry, &downloads));
            }
            todo.push((key, entry));
        }
        drop(snap);
        for (key, entry) in todo {
            let (ok, message, freed) = self
                .delete_gguf(&entry.model.base, &entry.model.subdir)
                .await;
            let freed_h = if freed == 0 {
                String::new()
            } else {
                crate::model_detail::human_bytes(freed)
            };
            results.push(format!(
                "{{\"key\":{},\"ok\":{ok},\"message\":{},\"freed\":{freed},\"freedH\":{}}}",
                json_string(&key),
                json_string(&message),
                json_string(&freed_h)
            ));
        }
        Outcome::Json(format!("{{\"results\":[{}]}}", results.join(",")).into_bytes())
    }

    /// The snapshot with each row shared; `None` for a missing models folder.
    async fn snapshot(&self) -> Result<Option<(Snapshot, Vec<Arc<Entry>>)>, Stop> {
        let (models, ini, open) = (
            PathBuf::from(&self.config.models),
            self.config.ini.clone(),
            self.open,
        );
        tokio::task::spawn_blocking(move || {
            let Some(mut snap) = snapshot(&models, &ini, open)? else {
                return Ok(None);
            };
            let entries = std::mem::take(&mut snap.entries)
                .into_iter()
                .map(Arc::new)
                .collect();
            Ok(Some((snap, entries)))
        })
        .await
        .map_err(|_| Stop::Failed)?
    }

    /// `services._running_containers()`, or the `ModelInUse` text when Docker cannot be read.
    async fn running(&self) -> Result<Vec<Running>, String> {
        match self.docker.running_containers().await {
            Ok(None) => Ok(Vec::new()),
            Ok(Some(list)) => Ok(list
                .into_iter()
                .filter(|c| {
                    c.name != "model-loader"
                        && (self.config.hostname.is_empty()
                            || !c.id.starts_with(&self.config.hostname))
                })
                .collect()),
            Err(kind) => Err(format!(
                "Could not check which containers use this model ({kind}); nothing was deleted."
            )),
        }
    }

    /// `services.delete_gguf(display_name, subdir)`.
    async fn delete_gguf(&self, display_name: &str, subdir: &str) -> Deleted {
        let refused = || {
            (
                false,
                "the native model manager could not read the models folder; nothing was deleted"
                    .to_owned(),
                0,
            )
        };
        let snap = match self.snapshot().await {
            Ok(snap) => snap,
            Err(_) => return refused(),
        };
        let Some((snap, entries)) = snap else {
            // A file in place of the folder lists nothing; a missing folder is the snapshot's
            // own error.
            return match exists(Path::new(&self.config.models)) {
                Ok(Some(_)) => (false, format!("not found: {display_name}"), 0),
                _ => (
                    false,
                    "directory not found (is the volume mounted?)".into(),
                    0,
                ),
            };
        };
        // noevia#1318: no fallback to a same-named model in another folder. The JSON API always
        // names the folder, so the fallback could only ever delete a model nobody asked for.
        let Some(found) = entries
            .iter()
            .find(|g| g.model.base == display_name && g.model.subdir == subdir)
            .cloned()
        else {
            return (false, format!("not found: {display_name}"), 0);
        };
        if found.unknowable {
            return refused();
        }
        let containers = match self.running().await {
            Ok(list) => list,
            Err(message) => return (false, message, 0),
        };
        let holders = holders(&self.config, &found, &containers);
        if !holders.is_empty() {
            return (false, in_use_message(&found, &holders), 0);
        }
        let downloads = self.download_blockers(&found);
        if !downloads.is_empty() {
            return (false, downloading_message(&found, &downloads), 0);
        }
        let config = self.config.clone();
        let base = snap.base.clone();
        drop(snap);
        tokio::task::spawn_blocking(move || fs::delete(&config, &base, &found))
            .await
            .unwrap_or_else(|_| refused())
    }
}

/// `{"key": key, "ok": False, "message": e.detail}`.
fn failure(key: &str, message: &str) -> String {
    format!(
        "{{\"key\":{},\"ok\":false,\"message\":{}}}",
        json_string(key),
        json_string(message)
    )
}

/// `[str(item) for item in body.get("models") or []]`; a value Python cannot iterate is its 500.
fn keys(body: &JsonValue) -> Result<Vec<String>, Outcome> {
    let Some(models) = body.get("models").filter(|v| v.truthy()) else {
        return Ok(Vec::new());
    };
    let keys: Vec<String> = match models {
        JsonValue::Arr(items) => {
            if items.len() > KEY_CAP {
                return Err(Outcome::Refused);
            }
            items.iter().map(py_str).collect::<Result<_, _>>()?
        }
        // A str iterates its characters, a dict its keys (first-seen order).
        JsonValue::Str(text) => text.chars().map(String::from).collect(),
        JsonValue::Obj(pairs) => {
            let mut out: Vec<String> = Vec::new();
            for (k, _) in pairs {
                if !out.contains(k) {
                    out.push(k.clone());
                }
            }
            out
        }
        _ => return Err(Outcome::Failed),
    };
    if keys.len() > KEY_CAP {
        return Err(Outcome::Refused);
    }
    Ok(keys)
}

/// `str(item)` for one JSON value.
fn py_str(value: &JsonValue) -> Result<String, Outcome> {
    match value {
        JsonValue::Null => Ok("None".into()),
        JsonValue::Bool(true) => Ok("True".into()),
        JsonValue::Bool(false) => Ok("False".into()),
        JsonValue::Int(n) if !value.truthy() => Ok(n.to_string()),
        JsonValue::Float(f) if f.is_nan() => Ok("nan".into()),
        JsonValue::Float(f) if f.is_infinite() => Ok(if *f > 0.0 { "inf" } else { "-inf" }.into()),
        JsonValue::Float(f) => Ok(gguf::py_float_repr(*f)),
        JsonValue::Str(s) => Ok(s.clone()),
        JsonValue::Arr(items) if items.is_empty() => Ok("[]".into()),
        JsonValue::Obj(pairs) if pairs.is_empty() => Ok("{}".into()),
        other => crate::prompts::py_str(Some(other)).map_err(|_| Outcome::Refused),
    }
}

/// `services.in_use_message(entry, holders)`.
fn in_use_message(entry: &Entry, holders: &[String]) -> String {
    format!(
        "{} is in use by a running container: {}. Stop or reconfigure it first; nothing was deleted.",
        entry.model.base,
        holders.join("; ")
    )
}

/// `services.downloading_message(entry, rels)`.
fn downloading_message(entry: &Entry, rels: &[String]) -> String {
    format!(
        "{} has a download queued or running in its folder: {}. Wait for it to finish or cancel it first; nothing was deleted.",
        entry.model.base,
        rels.join("; ")
    )
}

/// `services.download_blockers(entry, jobs)` over models-relative destinations.
pub(crate) fn blockers(entry: &Entry, destinations: &[String]) -> Vec<String> {
    let own = own_rels(entry);
    let folder = fs::hf_repo(&entry.model.subdir).unwrap_or_else(|| entry.model.subdir.clone());
    let mut found: Vec<String> = Vec::new();
    for dest in destinations {
        let rel = relnorm(dest);
        let inside = !folder.is_empty()
            && rel
                .strip_prefix(folder.as_str())
                .is_some_and(|rest| rest.starts_with('/'));
        if (own.contains(&rel) || inside) && !found.contains(&rel) {
            found.push(rel);
        }
    }
    found
}

/// `posixpath.normpath` of a relative path.
fn relnorm(rel: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// The model's files relative to the models folder: parts, then companions.
pub(crate) fn own_rels(entry: &Entry) -> Vec<String> {
    let rel = |name: &str| {
        if entry.model.subdir.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", entry.model.subdir)
        }
    };
    entry
        .model
        .parts
        .iter()
        .chain(&entry.model.companion_parts)
        .map(|p| rel(&p.name))
        .collect()
}

/// `services.model_holders(entry, containers)`.
fn holders(config: &Config, entry: &Entry, containers: &[Running]) -> Vec<String> {
    let mut rels: Vec<String> = Vec::new();
    for rel in own_rels(entry) {
        if !rels.contains(&rel) {
            rels.push(rel);
        }
    }
    let mut names: Vec<&str> = vec![entry.model_id(), entry.stem(), &entry.model.base];
    names.extend(entry.aliases.iter().map(String::as_str));
    names.retain(|n| !n.is_empty());
    let host_root = config.host_path.trim_end_matches('/');
    let mut found = Vec::new();
    for c in containers {
        let mut why = String::new();
        let mounts: Vec<(&str, &str)> = c
            .mounts
            .iter()
            .map(|(d, s)| (d.trim_end_matches('/'), s.trim_end_matches('/')))
            .collect();
        'args: for raw in c.entrypoint.iter().chain(&c.cmd).chain(&c.args) {
            let arg = match raw.split_once('=') {
                Some((_, value)) if raw.starts_with("--") => value,
                _ => raw.as_str(),
            };
            let norm = if arg.starts_with('/') {
                normpath(arg)
            } else {
                arg.to_owned()
            };
            for rel in &rels {
                if norm != *rel && !norm.ends_with(&format!("/{rel}")) {
                    continue;
                }
                let mount = mounts
                    .iter()
                    .find(|(d, _)| !d.is_empty() && norm.starts_with(&format!("{d}/")));
                if let Some((d, s)) = mount {
                    if !host_root.is_empty() {
                        let rest = norm.get(d.len()..).unwrap_or_default();
                        if normpath(&format!("{s}{rest}")) != format!("{host_root}/{rel}") {
                            continue;
                        }
                    }
                }
                why = format!("its command runs {rel}");
                break 'args;
            }
        }
        if why.is_empty() {
            let env = env_map(&c.env);
            let get = |key: &str| {
                env.iter()
                    .rev()
                    .find(|(k, _)| k == key)
                    .map_or("", |(_, v)| v.as_str())
            };
            let rerank_on = matches!(
                get("NOEVIA_FEATURE_RAG_RERANK").to_lowercase().as_str(),
                "1" | "true" | "on"
            );
            for key in MODEL_ENV_KEYS {
                if seed_only(config, c, get, key) {
                    continue;
                }
                let v = py_strip(get(key));
                if v.is_empty()
                    || v.to_lowercase() == "default"
                    || (key == "RERANK_MODEL" && !rerank_on)
                {
                    continue;
                }
                if names.contains(&v) {
                    why = format!("{key} is set to {v}");
                    break;
                }
            }
        }
        if !why.is_empty() {
            found.push(format!("{} ({why})", c.name));
        }
    }
    found
}

/// `_env_map(attrs)`: `KEY=value` items, the last one of a key wins.
fn env_map(items: &[String]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|item| match item.split_once('=') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => (item.clone(), String::new()),
        })
        .collect()
}

/// `_seed_only(container, env, key)`: an embedding env name that points at the guarded service.
fn seed_only<'a>(
    config: &Config,
    container: &Running,
    get: impl Fn(&str) -> &'a str,
    key: &str,
) -> bool {
    let Some((own, url)) = &config.embed else {
        return false;
    };
    let url_key = match key {
        "EMBEDDING_MODEL" => Some("EMBEDDING_BASE_URL"),
        "EMBEDDING_NEXT_MODEL" => Some("EMBEDDING_NEXT_BASE_URL"),
        "LLM_EMBED_MODEL" => Some("LLM_EMBED_BASE_URL"),
        "LLM_EMBED_NEXT_MODEL" => Some("LLM_EMBED_NEXT_BASE_URL"),
        "EMBED_MODEL" => None,
        _ => return false,
    };
    let own = py_strip(own);
    if !own.is_empty() && container.name.trim_start_matches('/') == own {
        return true;
    }
    let (Some(url_key), Some(service)) = (url_key, host_port(url)) else {
        return false;
    };
    host_port(get(url_key)) == Some(service)
}

/// `_host_port(url)`: urllib's hostname (lower-cased) and port, or the scheme's default. `None`
/// for anything urllib would refuse, and also for spellings this port does not follow exactly
/// (so the env name keeps blocking: never a skip Python would not make).
fn host_port(url: &str) -> Option<(String, u16)> {
    let url = py_strip(url);
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            (scheme.to_ascii_lowercase(), rest)
        }
        _ => return None,
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let (host, port) = if let Some(inner) = hostinfo.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match hostinfo.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (hostinfo, None),
        }
    };
    if host.is_empty() || !host.is_ascii() {
        return None;
    }
    let port = match port {
        None | Some("") => {
            if scheme == "https" {
                443
            } else {
                80
            }
        }
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => {
            let n: u16 = p.parse().ok()?;
            if n == 0 {
                // `u.port or default`: port 0 is falsy.
                if scheme == "https" {
                    443
                } else {
                    80
                }
            } else {
                n
            }
        }
        Some(_) => return None,
    };
    Some((host.to_ascii_lowercase(), port))
}

/// `str.strip()`.
pub(crate) fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// `posixpath.normpath(path)` for an absolute path.
pub(crate) fn normpath(path: &str) -> String {
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if initial.is_empty() {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    let out = format!("{initial}{joined}");
    if out.is_empty() {
        ".".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests;
