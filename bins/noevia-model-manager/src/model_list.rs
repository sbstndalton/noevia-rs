//! noevia#1318: native, read-only `GET /api/v1/models` and `GET /api/v1/overview`.
//!
//! Python (`model-manager/app/api.py`):
//!
//! ```python
//! @router.get("/overview")
//! async def overview() -> dict:
//!     snap = services.snapshot_models_dir()
//!     backends = await services.snapshot_backends()  # the services backend snapshot
//!     return {"modelsDir": {"path": str(snap.path), "hostPath": settings.models_host_path or None,
//!                           "exists": snap.exists, "error": snap.error, "disk": {...} if snap.disk else None},
//!             "models": sum(1 for g in snap.ggufs if not g.is_companion),
//!             "sections": len(ini.section_names()), "backends": [_plain(b) for b in backends],
//!             "activeDownloads": sum(1 for j in manager.snapshot() if j.status in ("queued", "downloading")),
//!             "revision": revision()}
//!
//! @router.get("/models")
//! async def models() -> dict:
//!     snap = services.snapshot_models_dir()
//!     loaded = await _loaded_map()
//!     badges = _badges_for_files(snap)
//!     return {"models": [_entry(g, loaded, badges) for g in snap.ggufs if not g.is_companion],
//!             "unregistered": ini.unregistered_gguf_stems(), "revision": revision()}
//! ```
//!
//! Native here, reusing what earlier slices ported: the models-folder snapshot, its ini aliases
//! and `sections_by_file` (model detail's), `_entry`'s file fields and `services.model_shape`,
//! the section names, `unregistered_gguf_stems`, the badge join of `_badges_for_files` (read from
//! `DATA_DIR/model_loader.db`, read-only, never created), the disk usage and the revision.
//!
//! Delegated, never re-implemented: the live state that only the Python child computes. For
//! `/models`, `loadedOn` (`api._loaded_map()`: Docker discovery plus inference-server probes) and `fit`
//! (`services.vram_fit_chips`: the GPU sampler and host RAM), asked of the child in one private
//! `POST /api/v1/internal/models-runtime` call. For `/overview`, `backends`
//! (the services backend snapshot) and `activeDownloads` (the child's download table)
//! through `GET /api/v1/internal/overview-runtime`; while this process owns the download table its
//! own count replaces the child's, which is then empty. The child renders those values with the
//! routes' own serialisation (pydantic's JSON mode), and they are placed verbatim.
//!
//! noevia#1318 step 1b: while this process samples the backends, those live fields are produced
//! natively instead (`backend_stats::live`, byte-identical to the child's answers) and the child's
//! private runtime call is not made.
//!
//! Deliberate differences (see `docs/native-model-list.md`): what the bounded reader cannot follow
//! exactly answers the native 503 [`REFUSAL`] instead of a guess.
use crate::bench_runs::{Row, Val};
use crate::model_detail::{self, Entry, Watched};
use crate::model_updates::{exists, json_string, py_float_repr, read_ini, Stop};
use crate::sections::{access::Root, Error as WalkError};
use futures_util::StreamExt;
use rusqlite::{types::ValueRef, Connection};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// The child's private calls for the live fields.
pub const MODELS_RUNTIME_PATH: &str = "/api/v1/internal/models-runtime";
pub const OVERVIEW_RUNTIME_PATH: &str = "/api/v1/internal/overview-runtime";
/// Body of the native refusal (503) when the bounded reader cannot answer exactly.
pub const REFUSAL: &str = "Native model list is unavailable";
/// Largest runtime answer read from the child (chips for every model of a large folder).
pub const RUNTIME_CAP: usize = 16 * 1024 * 1024;
/// Most models listed; a larger folder is refused rather than sent to the child in one call.
pub const MAX_MODELS: usize = 4096;
/// Most cached model shapes (Python's `gguf_meta` cache, keyed the same way, is unbounded).
pub const SHAPE_CACHE: usize = 4096;
/// The file part of one answer (the walk and, for the list, every model header not cached yet).
const DEADLINE: Duration = Duration::from_secs(60);
/// The child's live fields probe every backend and ask Docker for every model's chips.
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
/// Most badge rows read.
const ROW_CAP: usize = 100_000;
const VALUE_CAP: i32 = 16 * 1024 * 1024;
/// The badge rows' text and blob bytes together (the bench run reader's response bound).
const STORE_BYTES: usize = 64 * 1024 * 1024;
const BUSY_WAIT: Duration = Duration::from_millis(3000);
const STORE_DEADLINE: Duration = Duration::from_millis(3500);
/// `services.snapshot_models_dir()`'s error for a missing folder.
const MISSING: &str = "directory not found (is the volume mounted?)";
/// `db.BADGE_CATEGORIES`, in order.
const CATEGORIES: [&str; 5] = ["coding", "writing", "reasoning", "tools", "vision"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Models,
    Overview,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A finished 200 answer.
    Json(Vec<u8>),
    /// Python's handler raised: FastAPI's plain 500.
    Failed,
    /// The bounded native reader could not answer exactly: the native 503 refusal.
    Refused,
    /// The Python child did not answer the live fields: 502, as a forwarded route.
    Unavailable,
}

/// `shutil.disk_usage(path)` as `(total, free)` bytes.
pub type DiskUsage = Arc<dyn Fn(&Path) -> Result<(u128, u128), ()> + Send + Sync>;

/// `shutil.disk_usage`: `f_blocks * f_frsize` and `f_bavail * f_frsize` of `statvfs(path)`.
pub fn statvfs_usage() -> DiskUsage {
    Arc::new(|path: &Path| {
        let stats = rustix::fs::statvfs(path).map_err(|_| ())?;
        let fragment = u128::from(stats.f_frsize);
        Ok((
            u128::from(stats.f_blocks) * fragment,
            u128::from(stats.f_bavail) * fragment,
        ))
    })
}

/// What the routes read, named as Python's settings name them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `MODELS_DIR`, absolute and in pathlib's normal form (it is answered as configured).
    pub models: PathBuf,
    /// `MODELS_INI_PATH`.
    pub ini: PathBuf,
    /// `DATA_DIR` when `GET /api/v1/models` is native (the badge store is in it).
    pub data: Option<PathBuf>,
    /// `MODELS_HOST_PATH` when `GET /api/v1/overview` is native.
    pub host_path: Option<String>,
}

/// How the parent reaches its child, and the native live fields when this process samples the
/// backends.
pub struct Child<'a> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub token: &'a str,
    #[cfg(feature = "native-backends")]
    pub live: Option<&'a crate::backend_stats::BackendStats>,
}

type ShapeKey = (PathBuf, i128, u64);

pub struct ModelList {
    models: PathBuf,
    ini: PathBuf,
    database: Option<PathBuf>,
    host_path: Option<String>,
    disk: DiskUsage,
    /// One list file part (or badge read) at a time: the list may read many headers. A hung
    /// filesystem read holds its slot and the request's admission permit until it returns.
    reads: Arc<tokio::sync::Semaphore>,
    /// The overview's own slot, so a list stuck on a hung read never holds the overview up.
    counts: Arc<tokio::sync::Semaphore>,
    shapes: Arc<Mutex<HashMap<ShapeKey, String>>>,
    /// Set once the child has answered its health check (its startup ran `db.init()`).
    #[cfg_attr(not(feature = "native-backends"), allow(dead_code))]
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl ModelList {
    pub fn new(settings: Settings, disk: DiskUsage) -> Self {
        Self {
            models: settings.models,
            ini: settings.ini,
            database: settings.data.map(|d| d.join("model_loader.db")),
            host_path: settings.host_path,
            disk,
            reads: Arc::new(tokio::sync::Semaphore::new(1)),
            counts: Arc::new(tokio::sync::Semaphore::new(1)),
            shapes: Arc::new(Mutex::new(HashMap::new())),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// The route this instance answers for `GET path` (exact paths only; other methods and the
    /// slash variants keep their existing handling).
    pub fn route(&self, method: &axum::http::Method, path: &str) -> Option<Route> {
        if *method != axum::http::Method::GET {
            return None;
        }
        match path {
            "/api/v1/models" if self.database.is_some() => Some(Route::Models),
            "/api/v1/overview" if self.host_path.is_some() => Some(Route::Overview),
            _ => None,
        }
    }

    /// Runs `work` on a blocking thread after taking `slot` (waiting counts against the file
    /// part's deadline). The slot and the request's admission live as long as the work.
    async fn blocking<T: Send + 'static>(
        slot: &Arc<tokio::sync::Semaphore>,
        work: impl FnOnce(Instant) -> Result<T, Stop> + Send + 'static,
    ) -> Result<T, Outcome> {
        let deadline = Instant::now() + DEADLINE;
        let Ok(Ok(permit)) = tokio::time::timeout(DEADLINE, slot.clone().acquire_owned()).await
        else {
            return Err(Outcome::Refused);
        };
        crate::docker::spawn_blocking_admitted(move || {
            let _permit = permit;
            work(deadline)
        })
        .await
        .map_err(|_| Outcome::Failed)?
        .map_err(stop)
    }

    /// `GET /api/v1/models`.
    pub async fn models(&self, child: Child<'_>) -> Outcome {
        let Some(database) = self.database.clone() else {
            return Outcome::Failed;
        };
        let (models, ini, shapes) = (self.models.clone(), self.ini.clone(), self.shapes.clone());
        let disk = self.disk.clone();
        let listed = match Self::blocking(&self.reads, move |deadline| {
            list(&models, &ini, &disk, deadline, Root::new, &shapes)
        })
        .await
        {
            Ok(listed) => listed,
            Err(outcome) => return outcome,
        };
        let runtime = match post_runtime(&child, &listed.sizes(), &self.started).await {
            Ok(body) => body,
            Err(outcome) => return outcome,
        };
        // The store is read once the child has answered: its startup (`db.init()`) has run.
        let keys = listed.keys.clone();
        let badges = match Self::blocking(&self.reads, move |_| {
            badges(&database, &keys, Instant::now() + STORE_DEADLINE)
        })
        .await
        {
            Ok(badges) => badges,
            Err(outcome) => return outcome,
        };
        splice_models(&listed, &runtime, &badges)
    }

    /// `GET /api/v1/overview`. `active` is this process's own download count while it owns the
    /// download table.
    pub async fn overview(&self, child: Child<'_>, active: Option<usize>) -> Outcome {
        let Some(host_path) = self.host_path.clone() else {
            return Outcome::Failed;
        };
        let (models, ini, disk) = (self.models.clone(), self.ini.clone(), self.disk.clone());
        let counted = match Self::blocking(&self.counts, move |_| {
            count(&models, &ini, &host_path, &disk, Root::new)
        })
        .await
        {
            Ok(counted) => counted,
            Err(outcome) => return outcome,
        };
        let runtime = match get_runtime(&child, active).await {
            Ok(body) => body,
            Err(outcome) => return outcome,
        };
        splice_overview(&counted, &runtime, active)
    }
}

fn stop(stop: Stop) -> Outcome {
    match stop {
        Stop::Failed => Outcome::Failed,
        Stop::Refused => Outcome::Refused,
    }
}

// --- GET /api/v1/models: the file part ----------------------------------------------------------

/// One listed model, waiting for its live fields and badges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listing {
    /// `_entry` from `{"key":` through `"shape":…,`.
    head: String,
    model_id: String,
    display: String,
    bytes: u128,
}

/// The file part of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    entries: Vec<Listing>,
    /// `(display_name, aliases)` of every snapshot row, companions included, in order: what
    /// `_badges_for_files` joins.
    keys: Vec<(String, Vec<String>)>,
    /// The `unregistered` JSON array.
    unregistered: String,
    revision: String,
}

impl Listed {
    /// The sizes the child sizes its chips for, in list order.
    pub(crate) fn sizes(&self) -> Vec<u128> {
        self.entries.iter().map(|e| e.bytes).collect()
    }
}

pub(crate) fn list(
    models: &Path,
    ini: &Path,
    disk: &DiskUsage,
    deadline: Instant,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
    shapes: &Mutex<HashMap<ShapeKey, String>>,
) -> Result<Listed, Stop> {
    // `snapshot_models_dir()` takes the disk usage of an existing folder first: an OSError is only
    // noted in `error` (which the list does not show), but a zero-sized filesystem raises.
    if exists(models)?.is_some() {
        if let Ok((0, _)) = disk(models) {
            return Err(Stop::Failed); // ZeroDivisionError
        }
    }
    let snapshot = model_detail::snapshot(models, ini, open)?;
    let (names, claimed, stems) = match &snapshot {
        Some(snap) => (
            snap.names.clone(),
            snap.claimed.clone(),
            model_detail::stems_present(&snap.found)?,
        ),
        // A missing folder (or a file): nothing is listed, but the section names are still read.
        None => (section_names(ini)?, HashSet::new(), HashMap::new()),
    };
    let mut entries = Vec::new();
    let mut keys = Vec::new();
    if let Some(snap) = &snapshot {
        for entry in &snap.entries {
            keys.push((entry.model.base.clone(), entry.aliases.clone()));
            if entry.model.companion {
                continue;
            }
            if entry.unknowable {
                return Err(Stop::Refused);
            }
            if entries.len() >= MAX_MODELS || Instant::now() >= deadline {
                return Err(Stop::Refused);
            }
            let shape = shape_of(entry, &snap.root, &snap.base, shapes)?;
            entries.push(Listing {
                head: model_detail::entry_head(entry, &shape)?,
                model_id: entry.model_id().to_owned(),
                display: entry.model.base.clone(),
                bytes: entry.model.total,
            });
        }
    }
    Ok(Listed {
        entries,
        keys,
        unregistered: unregistered(&names, &claimed, &stems),
        revision: revision(ini)?,
    })
}

/// `ini.unregistered_gguf_stems()`: stems with no section of their name whose file no section
/// configures, sorted. (A section whose `model =` path the reader cannot check could only claim
/// its own stem's file, whose stem is that section's name: it never changes this list.)
fn unregistered(
    names: &[String],
    claimed: &HashSet<String>,
    stems: &HashMap<String, String>,
) -> String {
    let have: HashSet<&str> = names.iter().map(String::as_str).collect();
    let mut out: Vec<&str> = stems
        .iter()
        .filter(|(stem, rel)| !have.contains(stem.as_str()) && !claimed.contains(*rel))
        .map(|(stem, _)| stem.as_str())
        .collect();
    // Python sorts str by code point, which is UTF-8 byte order.
    out.sort_unstable();
    let items: Vec<String> = out.iter().map(|s| json_string(s)).collect();
    format!("[{}]", items.join(","))
}

/// `ini.section_names()`: `read_ini().sections()` (raises on an unreadable file).
fn section_names(ini: &Path) -> Result<Vec<String>, Stop> {
    Ok(read_ini(ini)?
        .map(|(_, parsed)| parsed.names().map(str::to_owned).collect())
        .unwrap_or_default())
}

/// `api.revision()`: sha256 of the models.ini bytes, of nothing when it does not exist.
pub(crate) fn revision(ini: &Path) -> Result<String, Stop> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    if exists(ini)?.is_some() {
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
                .open(ini)
                .map_err(|_| Stop::Failed)?
        };
        if !file.metadata().map_err(|_| Stop::Failed)?.is_file() {
            return Err(Stop::Failed);
        }
        let mut bytes = Vec::new();
        let cap = crate::sections::INI_CAP as u64;
        file.take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Stop::Failed)?;
        if bytes.len() as u64 > cap {
            return Err(Stop::Failed);
        }
        hash.update(&bytes);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// `services.model_shape(g.parts[0])` as JSON. Python swallows every error reading the header
/// (the shape is then unknown, `null`); a header past this reader's own bound is refused instead.
/// Shapes are cached as `gguf_meta.read_raw` caches headers: by path, `st_mtime_ns` and size.
fn shape_of(
    entry: &Entry,
    root: &Root,
    base: &Path,
    shapes: &Mutex<HashMap<ShapeKey, String>>,
) -> Result<String, Stop> {
    use std::os::unix::fs::MetadataExt;
    const UNKNOWN: &str = "null";
    let rel = entry.first_rel()?;
    let Some(file) = model_detail::open_model_file(root, base, &rel)? else {
        return Ok(UNKNOWN.to_owned());
    };
    let Ok(meta) = file.metadata() else {
        return Ok(UNKNOWN.to_owned());
    };
    let key = (
        base.join(&rel),
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        meta.len(),
    );
    if let Some(hit) = shapes
        .lock()
        .ok()
        .and_then(|cache| cache.get(&key).cloned())
    {
        return Ok(hit);
    }
    let mut reader = Watched {
        inner: file,
        failed: false,
    };
    let parsed = gguf::read_raw_stream(&mut reader);
    if reader.failed {
        return Ok(UNKNOWN.to_owned()); // OSError while reading: swallowed
    }
    let Ok(raw) = parsed else {
        return Ok(UNKNOWN.to_owned()); // GgufMetaError: swallowed, and not cached
    };
    if model_detail::hit_read_bound(&raw) {
        return Err(Stop::Refused);
    }
    let shape = model_detail::shape(&raw)?;
    if let Ok(mut cache) = shapes.lock() {
        if cache.len() >= SHAPE_CACHE {
            cache.clear();
        }
        cache.insert(key, shape.clone());
    }
    Ok(shape)
}

// --- GET /api/v1/models: the badges -------------------------------------------------------------

fn field<'a>(row: &'a Row, name: &str) -> Option<&'a Val> {
    // dict(row)[name]: a repeated column name keeps its last value.
    row.0.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// `a == b` for two stored values used as dict keys (`1 == 1.0`; text and bytes never equal).
fn same_key(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Null, Val::Null) => true,
        (Val::Int(x), Val::Int(y)) => x == y,
        (Val::Real(x), Val::Real(y)) => x == y,
        (Val::Int(i), Val::Real(f)) | (Val::Real(f), Val::Int(i)) => {
            int_float(*i, *f) == Some(std::cmp::Ordering::Equal)
        }
        (Val::Text(x), Val::Text(y)) => x == y,
        (Val::Blob(x), Val::Blob(y)) => x == y,
        _ => false,
    }
}

/// Python's exact comparison of an int with a float (`None` for NaN).
fn int_float(i: i64, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if f.is_nan() {
        return None;
    }
    // 2^63 as a float; every float at or past it is larger than any i64.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if f >= LIMIT {
        return Some(Ordering::Less);
    }
    if f < -LIMIT {
        return Some(Ordering::Greater);
    }
    let whole = f.trunc();
    // |whole| < 2^63 (or exactly -2^63), so the conversion is exact.
    #[allow(clippy::cast_possible_truncation)]
    let w = whole as i64;
    Some(i.cmp(&w).then_with(|| {
        let frac = f - whole;
        if frac > 0.0 {
            Ordering::Less
        } else if frac < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }))
}

/// `value or 0`.
fn or_zero(value: &Val) -> Val {
    match value {
        Val::Null => Val::Int(0),
        Val::Int(0) => Val::Int(0),
        Val::Real(f) if *f == 0.0 => Val::Int(0),
        Val::Text(t) if t.is_empty() => Val::Int(0),
        Val::Blob(b) if b.is_empty() => Val::Int(0),
        other => other.clone(),
    }
}

/// `(b["created_at"] or 0) > (cur["created_at"] or 0)`; comparing different kinds raises.
fn newer(b: &Row, cur: &Row) -> Result<bool, Stop> {
    let x = or_zero(field(b, "created_at").ok_or(Stop::Failed)?);
    let y = or_zero(field(cur, "created_at").ok_or(Stop::Failed)?);
    Ok(match (&x, &y) {
        (Val::Int(a), Val::Int(c)) => a > c,
        (Val::Real(a), Val::Real(c)) => a > c,
        (Val::Int(a), Val::Real(c)) => int_float(*a, *c) == Some(std::cmp::Ordering::Greater),
        (Val::Real(a), Val::Int(c)) => int_float(*c, *a) == Some(std::cmp::Ordering::Less),
        (Val::Text(a), Val::Text(c)) => a > c,
        (Val::Blob(a), Val::Blob(c)) => a > c,
        _ => return Err(Stop::Failed), // TypeError
    })
}

/// `helpers._badges_for_files(snap)`: display name -> the badge rows as a JSON array, for every
/// snapshot row with at least one badge (a later row of the same name replaces an earlier one).
pub(crate) fn badges(
    database: &Path,
    keys: &[(String, Vec<String>)],
    deadline: Instant,
) -> Result<HashMap<String, String>, Stop> {
    let db = open_store(database, deadline)?;
    let rows = read_rows(
        &db,
        "SELECT * FROM model_badge ORDER BY alias, category",
        STORE_BYTES,
    )?;
    // db.badges_by_alias(): rows grouped by alias, in query order. Only a text alias can be
    // looked up by a section name.
    let mut by_alias: HashMap<&str, Vec<&Row>> = HashMap::new();
    for row in &rows {
        if let Val::Text(alias) = field(row, "alias").ok_or(Stop::Failed)? {
            by_alias.entry(alias.as_str()).or_default().push(row);
        }
    }
    let mut out = HashMap::new();
    for (display, aliases) in keys {
        let mut best: Vec<(&Val, &Row)> = Vec::new();
        for alias in aliases {
            for &row in by_alias.get(alias.as_str()).into_iter().flatten() {
                let category = field(row, "category").ok_or(Stop::Failed)?;
                match best.iter().position(|(c, _)| same_key(c, category)) {
                    None => best.push((category, row)),
                    Some(at) => {
                        let current = best.get(at).map(|(_, r)| *r).ok_or(Stop::Failed)?;
                        if newer(row, current)? {
                            if let Some(slot) = best.get_mut(at) {
                                slot.1 = row;
                            }
                        }
                    }
                }
            }
        }
        if best.is_empty() {
            continue;
        }
        let mut rendered = Vec::new();
        for name in CATEGORIES {
            if let Some((_, row)) = best
                .iter()
                .find(|(c, _)| matches!(c, Val::Text(t) if t == name))
            {
                rendered.push(row.render().ok_or(Stop::Failed)?);
            }
        }
        out.insert(display.clone(), format!("[{}]", rendered.join(",")));
    }
    Ok(out)
}

/// The store, opened as every native reader opens it (read-write without CREATE, `query_only`).
fn open_store(database: &Path, deadline: Instant) -> Result<Connection, Stop> {
    let logged = |e: rusqlite::Error| {
        crate::sqlite_read::log_once("model_list", &e);
        Stop::Failed
    };
    let db = crate::sqlite_read::open(database, "model_list").map_err(|_| Stop::Failed)?;
    db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, VALUE_CAP)
        .map_err(logged)?;
    db.busy_timeout(BUSY_WAIT).map_err(|_| Stop::Failed)?;
    db.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(|_| Stop::Failed)?;
    Ok(db)
}

/// `dict(r) for r in cursor`; TEXT that is not UTF-8 raises in Python's sqlite3.
fn read_rows(db: &Connection, sql: &str, mut budget: usize) -> Result<Vec<Row>, Stop> {
    let mut statement = db.prepare(sql).map_err(|_| Stop::Failed)?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut cursor = statement.query([]).map_err(|_| Stop::Failed)?;
    let mut out = Vec::new();
    while let Some(row) = cursor.next().map_err(|_| Stop::Failed)? {
        if out.len() >= ROW_CAP {
            return Err(Stop::Failed);
        }
        let mut fields = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            let value = match row.get_ref(i).map_err(|_| Stop::Failed)? {
                ValueRef::Null => Val::Null,
                ValueRef::Integer(n) => Val::Int(n),
                ValueRef::Real(f) => Val::Real(f),
                ValueRef::Text(t) => {
                    budget = budget.checked_sub(t.len()).ok_or(Stop::Failed)?;
                    Val::Text(std::str::from_utf8(t).map_err(|_| Stop::Failed)?.to_owned())
                }
                ValueRef::Blob(b) => {
                    budget = budget.checked_sub(b.len()).ok_or(Stop::Failed)?;
                    Val::Blob(b.to_vec())
                }
            };
            fields.push((name.clone(), value));
        }
        out.push(Row(fields));
    }
    Ok(out)
}

/// The answer: each entry with `loaded[modelId]`, its chips (verbatim) and its badges.
pub(crate) fn splice_models(
    listed: &Listed,
    runtime: &[u8],
    badges: &HashMap<String, String>,
) -> Outcome {
    use serde_json::value::RawValue;
    let Ok(fields) = serde_json::from_slice::<HashMap<String, Box<RawValue>>>(runtime) else {
        return Outcome::Unavailable;
    };
    let (Some(loaded), Some(fit)) = (fields.get("loaded"), fields.get("fit")) else {
        return Outcome::Unavailable;
    };
    let Ok(loaded) = serde_json::from_str::<HashMap<String, Vec<String>>>(loaded.get()) else {
        return Outcome::Unavailable;
    };
    let Ok(fit) = serde_json::from_str::<Vec<Box<RawValue>>>(fit.get()) else {
        return Outcome::Unavailable;
    };
    if fit.len() != listed.entries.len()
        || fit
            .iter()
            .any(|chips| serde_json::from_str::<Vec<Box<RawValue>>>(chips.get()).is_err())
    {
        return Outcome::Unavailable;
    }
    let mut out = String::from("{\"models\":[");
    for (i, (entry, chips)) in listed.entries.iter().zip(&fit).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&entry.head);
        out.push_str("\"loadedOn\":[");
        let names = loaded
            .get(&entry.model_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for (j, name) in names.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&json_string(name));
        }
        out.push_str("],\"fit\":");
        out.push_str(chips.get());
        out.push_str(",\"badges\":");
        out.push_str(badges.get(&entry.display).map_or("[]", String::as_str));
        out.push('}');
    }
    out.push_str("],\"unregistered\":");
    out.push_str(&listed.unregistered);
    out.push_str(",\"revision\":");
    out.push_str(&json_string(&listed.revision));
    out.push('}');
    Outcome::Json(out.into_bytes())
}

// --- GET /api/v1/overview: the file part --------------------------------------------------------

/// The file part of the overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Counted {
    /// The `modelsDir` JSON object.
    models_dir: String,
    models: usize,
    sections: usize,
    revision: String,
}

pub(crate) fn count(
    models: &Path,
    ini: &Path,
    host_path: &str,
    disk: &DiskUsage,
    open: impl FnOnce(&Path) -> Result<Root, WalkError>,
) -> Result<Counted, Stop> {
    let path = models.to_str().ok_or(Stop::Refused)?;
    let host = if host_path.is_empty() {
        "null".to_owned()
    } else {
        json_string(host_path)
    };
    let (found, error, usage, count) = match exists(models)? {
        None => (false, json_string(MISSING), "null".to_owned(), 0),
        Some(_) => {
            // An OSError here is reported in `error` with its errno text, which is not
            // reproduced: refused.
            let (total, free) = disk(models).map_err(|()| Stop::Refused)?;
            let usage = disk_json(total, free)?;
            let count = match model_detail::snapshot(models, ini, open)? {
                Some(snap) => snap.entries.iter().filter(|e| !e.model.companion).count(),
                None => 0,
            };
            (true, "null".to_owned(), usage, count)
        }
    };
    Ok(Counted {
        models_dir: format!(
            "{{\"path\":{},\"hostPath\":{host},\"exists\":{found},\"error\":{error},\"disk\":{usage}}}",
            json_string(path)
        ),
        models: count,
        sections: section_names(ini)?.len(),
        revision: revision(ini)?,
    })
}

/// `DiskInfo` as the overview renders it: `usedPct` is `round(100 * (total - free) / total, 1)`.
fn disk_json(total: u128, free: u128) -> Result<String, Stop> {
    if total == 0 {
        return Err(Stop::Failed); // ZeroDivisionError
    }
    // statvfs never reports more available than total blocks; a negative share is not printed.
    let used = total.checked_sub(free).ok_or(Stop::Refused)?;
    let numerator = used.checked_mul(100).ok_or(Stop::Refused)?;
    let pct = model_autoconfig::pyfloat::round_nd(true_divide(numerator, total), 1);
    Ok(format!(
        "{{\"total\":{total},\"free\":{free},\"usedPct\":{},\"totalH\":{},\"freeH\":{}}}",
        py_float_repr(pct),
        json_string(&model_detail::human_bytes(total)),
        json_string(&model_detail::human_bytes(free))
    ))
}

/// Python's `int / int`: the exact quotient rounded once to the nearest float (ties to even).
pub(crate) fn true_divide(numerator: u128, denominator: u128) -> f64 {
    if denominator == 0 || numerator == 0 {
        return 0.0;
    }
    // Long division to at least 65 significant quotient bits, with what is left folded into the
    // lowest bit (sticky): one `as f64` of that rounds exactly once, as the exact quotient would.
    let mut quotient: u128 = numerator / denominator;
    let mut remainder = numerator % denominator;
    let mut scale: i32 = 0; // the value is (quotient + remainder / denominator) * 2^-scale
    while quotient < (1u128 << 64) {
        quotient <<= 1;
        match remainder.checked_mul(2) {
            Some(doubled) if doubled < denominator => remainder = doubled,
            Some(doubled) => {
                quotient |= 1;
                remainder = doubled - denominator;
            }
            // 2 * remainder passed 2^128, so it is past the denominator too; the difference is
            // below the denominator and fits.
            None => {
                quotient |= 1;
                remainder = remainder.wrapping_mul(2).wrapping_sub(denominator);
            }
        }
        scale += 1;
    }
    if remainder != 0 {
        quotient |= 1;
    }
    #[allow(clippy::cast_precision_loss)]
    let mantissa = quotient as f64;
    mantissa * 2f64.powi(-scale)
}

/// The answer, with the child's `backends` placed verbatim and its download count unless this
/// process owns the download table.
pub(crate) fn splice_overview(counted: &Counted, runtime: &[u8], active: Option<usize>) -> Outcome {
    use serde_json::value::RawValue;
    let Ok(fields) = serde_json::from_slice::<HashMap<String, Box<RawValue>>>(runtime) else {
        return Outcome::Unavailable;
    };
    let (Some(backends), Some(downloads)) = (fields.get("backends"), fields.get("activeDownloads"))
    else {
        return Outcome::Unavailable;
    };
    if serde_json::from_str::<Vec<Box<RawValue>>>(backends.get()).is_err() {
        return Outcome::Unavailable;
    }
    let Ok(child_count) = serde_json::from_str::<u64>(downloads.get()) else {
        return Outcome::Unavailable;
    };
    let active = active.map_or(child_count.to_string(), |n| n.to_string());
    Outcome::Json(
        format!(
            "{{\"modelsDir\":{},\"models\":{},\"sections\":{},\"backends\":{},\"activeDownloads\":{active},\"revision\":{}}}",
            counted.models_dir,
            counted.models,
            counted.sections,
            backends.get(),
            json_string(&counted.revision)
        )
        .into_bytes(),
    )
}

// --- the child's private calls ------------------------------------------------------------------

/// `{"bytes": [...]}` as the child reads it: the sizes in list order.
pub(crate) fn runtime_request(sizes: &[u128]) -> String {
    let items: Vec<String> = sizes.iter().map(u128::to_string).collect();
    format!("{{\"bytes\":[{}]}}", items.join(","))
}

/// A native live field that did not finish: Python's 500, or the native 503.
#[cfg(feature = "native-backends")]
fn live_failed(fail: crate::backend_stats::Fail) -> Outcome {
    match fail {
        crate::backend_stats::Fail::Raised => Outcome::Failed,
        crate::backend_stats::Fail::Refused => Outcome::Refused,
    }
}

async fn post_runtime(
    child: &Child<'_>,
    sizes: &[u128],
    started: &std::sync::atomic::AtomicBool,
) -> Result<Vec<u8>, Outcome> {
    #[cfg(feature = "native-backends")]
    if let Some(live) = child.live {
        let body = live.models_runtime(sizes).await.map_err(live_failed)?;
        if !crate::startup::ready(child.client, child.base, child.token, started).await {
            return Err(Outcome::Unavailable);
        }
        return Ok(body);
    }
    let _ = started;
    let request = child
        .client
        .post(format!("{}{MODELS_RUNTIME_PATH}", child.base))
        .header("content-type", "application/json")
        .body(runtime_request(sizes));
    read_child(child, request).await
}

async fn get_runtime(child: &Child<'_>, active: Option<usize>) -> Result<Vec<u8>, Outcome> {
    // Natively only while this process also owns the download table; else the child's count.
    #[cfg(feature = "native-backends")]
    if let (Some(live), Some(active)) = (child.live, active) {
        return live.overview_runtime(active).await.map_err(live_failed);
    }
    let _ = active;
    let request = child
        .client
        .get(format!("{}{OVERVIEW_RUNTIME_PATH}", child.base));
    read_child(child, request).await
}

/// The child's answer: its own unhandled error is Python's 500; anything else it cannot answer
/// is the forwarded route's 502.
async fn read_child(
    child: &Child<'_>,
    mut request: reqwest::RequestBuilder,
) -> Result<Vec<u8>, Outcome> {
    request = request.timeout(CHILD_TIMEOUT);
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
        200 => Ok(bytes),
        500 => Err(Outcome::Failed),
        _ => Err(Outcome::Unavailable),
    }
}

#[cfg(test)]
mod tests;
