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
//!   source: they are read once at startup from a JSON file (`SEARCH_TRUSTED_PATH`, default
//!   [`TRUSTED_PATH`]), `{"quantisers": [...], "labs": [...]}`. The entries are kept exactly as
//!   written: `discover.judge` lower-cases only the owner and tests exact membership, so an entry
//!   with an upper-case letter never matches, here as in Python. A missing file is an error like a
//!   malformed one (the search runtime then stays with the child); empty sets apply only when the
//!   file itself says so.
//!
//! TODO(noevia#1318, child retirement): both fallbacks to the child must then become a startup
//! refusal or the native 503, never silently empty sets (the search defaults to trusted-only).
//!
//! The repository read's chips and context estimates (`autoconfig.analyze`) are native too while
//! the section autoconfig is (`estimates`); otherwise they come from the child's
//! `POST /api/v1/internal/search-repo-runtime`.
//!
//! The badge store's schema is created by the child's startup, so the first native answer waits
//! once for the child's exact private startup-ready advertisement (502 until ready, as when the runtime call
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

/// Reads the trusted sets. A missing file, anything but a regular file (a FIFO would block
/// startup), or anything but `{"quantisers": [text], "labs": [text]}` (both keys, nothing else) is
/// an error, and the search runtime then stays with the child.
pub fn load_trusted(path: &Path) -> Result<Trusted, &'static str> {
    use std::io::Read;
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("the trusted publisher file is not a regular file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("the trusted publisher file is missing (seed it with the deploy tool)")
        }
        Err(_) => return Err("the trusted publisher file cannot be read"),
    }
    let file =
        std::fs::File::open(path).map_err(|_| "the trusted publisher file cannot be read")?;
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
    /// noevia#1318 step 3a: `settings.cache_ram_limits[0]` while the section autoconfig is native;
    /// the repository read's chips and estimates are then computed here (`None`: the child's).
    pub estimates_cache_cap_mib: Option<i64>,
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
    let mut by_repo: Vec<(String, Vec<String>, HashSet<String>)> = Vec::new();
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
        match by_repo.iter_mut().find(|(r, _, _)| *r == repo) {
            Some((_, files, seen)) => {
                if seen.insert(file.clone()) {
                    files.push(file);
                }
            }
            None => {
                let seen = HashSet::from([file.clone()]);
                by_repo.push((repo, vec![file], seen));
            }
        }
    }
    let mut out = String::from("{");
    let mut first = true;
    for (repo, files, _) in &by_repo {
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

    /// Wait for the exact private advertisement after the child's schema startup.
    async fn child_started(&self) -> bool {
        crate::startup::ready(&self.legacy, &self.legacy_base, &self.token, &self.started).await
    }

    #[cfg(test)]
    pub(crate) async fn startup_fixture_runtime(&self, source: &Source) -> Option<Vec<u8>> {
        self.native_runtime(source).await.ok()
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
