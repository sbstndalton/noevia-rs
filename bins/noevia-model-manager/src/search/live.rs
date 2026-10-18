//! noevia#1318 step 1c: the search runtime (`main._search_runtime`) natively.
//!
//! `GET /api/v1/search` judged its results against live state the child answered through
//! `GET /api/v1/internal/search-runtime`. While this process samples the backends, the same bytes
//! are produced here and that call is not made:
//!
//! - `budget`: `max(vram_gb of helpers._backend_list())`, 0.0 without one (`fit::budget` over
//!   the native descriptors), as Python's `repr`;
//! - `downloaded`: `helpers._downloaded_and_still_present()`, the `done` rows of
//!   `download_history` (newest first, one entry per file and repository) kept only while the
//!   file (by its stored name, or its last path part) is in the models-folder snapshot;
//! - `quantisers` and `labs`: the trusted publisher sets, sorted. They are deployment data, not
//!   source: they are read at startup from a JSON file (`SEARCH_TRUSTED_PATH`, default
//!   [`TRUSTED_PATH`]), `{"quantisers": [...], "labs": [...]}`, and are empty when it is absent.
//!
//! The repository read's context estimates (`autoconfig.analyze`) still come from the child's
//! `POST /api/v1/internal/search-repo-runtime`.
//!
//! The badge store's schema is created by the child's startup, so the first native answer waits
//! once for the child's `GET /api/v1/health` (502 until it has answered, as when the runtime call
//! came first). A snapshot this side cannot follow exactly (a walk refusal, a models folder on a
//! zero-sized filesystem, an unreadable `models.ini`) answers the native 503 where Python could
//! have answered an empty set or its 500; a stored value Python would trip over answers the 500.
use super::{store, Search, Stop};
use crate::backend_stats::{fit, BackendStats, Fail};
use crate::model_updates::json_string;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Where the trusted publisher sets are read from when SEARCH_TRUSTED_PATH is unset.
pub const TRUSTED_PATH: &str = "/config/search-trusted.json";
/// A trusted-sets file larger than this is not one.
const TRUSTED_CAP: u64 = 1024 * 1024;
/// Most `download_history` rows read.
const ROW_CAP: usize = 200_000;

/// `discover.TRUSTED_QUANTISERS` and `discover.TRUSTED_LABS`, sorted and without repeats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trusted {
    pub quantisers: Vec<String>,
    pub labs: Vec<String>,
}

/// Reads the trusted sets: absent is empty; anything but `{"quantisers": [text], "labs": [text]}`
/// (both keys, nothing else) is an error, and the search then stays with the child.
pub fn load_trusted(path: &Path) -> Result<Trusted, &'static str> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Trusted::default()),
        Err(_) => return Err("the trusted publisher file cannot be read"),
    };
    let mut raw = Vec::new();
    file.take(TRUSTED_CAP + 1)
        .read_to_end(&mut raw)
        .map_err(|_| "the trusted publisher file cannot be read")?;
    if raw.len() as u64 > TRUSTED_CAP {
        return Err("the trusted publisher file is too large");
    }
    let value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|_| "the trusted publisher file is not JSON")?;
    let shape = "the trusted publisher file must be {\"quantisers\": [...], \"labs\": [...]}";
    let object = value.as_object().ok_or(shape)?;
    if object.len() != 2 {
        return Err(shape);
    }
    let list = |key: &str| -> Result<Vec<String>, &'static str> {
        let mut out: Vec<String> = object
            .get(key)
            .and_then(serde_json::Value::as_array)
            .ok_or(shape)?
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or(shape))
            .collect::<Result<_, _>>()?;
        out.sort();
        out.dedup();
        Ok(out)
    };
    Ok(Trusted {
        quantisers: list("quantisers")?,
        labs: list("labs")?,
    })
}

/// What the native runtime reads.
pub struct Source {
    pub stats: Arc<BackendStats>,
    /// `MODELS_DIR` and `MODELS_INI_PATH`.
    pub models: PathBuf,
    pub ini: PathBuf,
    pub trusted: Trusted,
}

/// `_files_present_on_disk()`: every part and folded companion part of the snapshot, by name and,
/// in a subfolder, by `subdir/name`.
fn present(models: &Path, ini: &Path) -> Result<HashSet<String>, Stop> {
    use crate::model_updates::{exists, Stop as Walk};
    let refused = |_: Walk| Stop::Refused;
    if exists(models).map_err(refused)? == Some(true) {
        // `snapshot_models_dir()` divides by the filesystem size: a zero-sized one raises.
        let stats = rustix::fs::statvfs(models).map_err(|_| Stop::Refused)?;
        if stats.f_blocks == 0 || stats.f_frsize == 0 {
            return Err(Stop::Refused);
        }
    }
    let mut names = HashSet::new();
    let Some(snap) = crate::model_detail::snapshot(models, ini, crate::sections::access::Root::new)
        .map_err(refused)?
    else {
        return Ok(names);
    };
    for entry in &snap.entries {
        let m = &entry.model;
        for part in m.parts.iter().chain(&m.companion_parts) {
            names.insert(part.name.clone());
            if !m.subdir.is_empty() {
                names.insert(format!("{}/{}", m.subdir, part.name));
            }
        }
    }
    Ok(names)
}

/// `db.downloaded_files_by_repo()` intersected with `present`, rendered as Python's dict.
fn downloaded(database: &Path, present: &HashSet<String>) -> Result<String, Stop> {
    use rusqlite::types::ValueRef;
    let db = store::open(database).map_err(|_| Stop::Failed)?;
    let mut statement = db
        .prepare(
            "SELECT repo_id, filename FROM download_history WHERE status = 'done' ORDER BY id DESC",
        )
        .map_err(|_| Stop::Failed)?;
    let mut rows = statement.query([]).map_err(|_| Stop::Failed)?;
    let text = |v: ValueRef<'_>| -> Result<Option<String>, Stop> {
        match v {
            ValueRef::Text(t) => std::str::from_utf8(t)
                .map(|s| Some(s.to_owned()))
                .map_err(|_| Stop::Failed),
            ValueRef::Null => Ok(None),
            // A number or blob: Python would key or compare it as such; not followed here.
            _ => Err(Stop::Refused),
        }
    };
    let mut by_repo: Vec<(String, Vec<String>)> = Vec::new();
    let mut count = 0usize;
    while let Some(row) = rows.next().map_err(|_| Stop::Failed)? {
        count += 1;
        if count > ROW_CAP {
            return Err(Stop::Refused);
        }
        let repo = text(row.get_ref(0).map_err(|_| Stop::Failed)?)?;
        let file = text(row.get_ref(1).map_err(|_| Stop::Failed)?)?;
        // `if not rid: continue`.
        let Some(repo) = repo.filter(|r| !r.is_empty()) else {
            continue;
        };
        // A NULL filename later trips `f.rsplit` (AttributeError): the 500.
        let file = file.ok_or(Stop::Failed)?;
        match by_repo.iter_mut().find(|(r, _)| *r == repo) {
            Some((_, files)) => {
                if !files.contains(&file) {
                    files.push(file);
                }
            }
            None => by_repo.push((repo, vec![file])),
        }
    }
    let mut out = String::from("{");
    let mut first = true;
    for (repo, files) in &by_repo {
        let kept: Vec<&String> = files
            .iter()
            .filter(|f| {
                present.contains(f.as_str())
                    || present.contains(f.rsplit('/').next().unwrap_or(f.as_str()))
            })
            .collect();
        if kept.is_empty() {
            continue;
        }
        if !std::mem::take(&mut first) {
            out.push(',');
        }
        out.push_str(&json_string(repo));
        out.push_str(":[");
        let items: Vec<String> = kept.iter().map(|f| json_string(f)).collect();
        out.push_str(&items.join(","));
        out.push(']');
    }
    out.push('}');
    Ok(out)
}

fn list(items: &[String]) -> String {
    let rendered: Vec<String> = items.iter().map(|s| json_string(s)).collect();
    format!("[{}]", rendered.join(","))
}

impl Search {
    /// Makes the search runtime native (once; later calls are ignored).
    pub fn set_live(&self, source: Source) {
        let _ = self.live.set(source);
    }

    /// The badge store exists once the child's startup is over (its health check answered).
    async fn child_started(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.started.load(Ordering::Acquire) {
            return true;
        }
        let ok = self
            .child(
                self.legacy
                    .get(format!("{}/api/v1/health", self.legacy_base)),
            )
            .await
            .is_ok();
        if ok {
            self.started.store(true, Ordering::Release);
        }
        ok
    }

    /// `main._search_runtime`'s body, byte for byte.
    pub(super) async fn native_runtime(&self, source: &Source) -> Result<Vec<u8>, Stop> {
        if !self.child_started().await {
            return Err(Stop::Unavailable);
        }
        let descriptors = source.stats.descriptors().await.map_err(|f| match f {
            Fail::Raised => Stop::Failed,
            Fail::Refused => Stop::Refused,
        })?;
        let budget = fit::budget(&descriptors);
        let (models, ini, database) = (
            source.models.clone(),
            source.ini.clone(),
            self.database.clone(),
        );
        let downloaded = self
            .blocking(move || {
                let present = present(&models, &ini)?;
                downloaded(&database, &present)
            })
            .await??;
        Ok(format!(
            "{{\"budget\":{},\"downloaded\":{downloaded},\"quantisers\":{},\"labs\":{}}}",
            json_string(&gguf::py_float_repr(budget)),
            list(&source.trusted.quantisers),
            list(&source.trusted.labs)
        )
        .into_bytes())
    }
}

#[cfg(test)]
mod tests;
