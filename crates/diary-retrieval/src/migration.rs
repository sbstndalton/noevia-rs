//! Re-embedding the Diary index for a new embedding model, without downtime (port of
//! `diary/agent/embedding_migration.py`, #1636 / #1638 / #1768).
//!
//! `set_next` builds a target's vectors beside the active ones while search keeps using the
//! active model; `switch` makes next active once it covers every index file (or forced, or, for a
//! move of the same model to another endpoint, once that endpoint gives the same vectors);
//! `rollback`, `finish` and `clear_next` as in the Python. A pass works file by file over the
//! service index and every tenant's `index.db` / `managed-index.db`, re-embedding the chunk text
//! already stored in each file and writing the vectors back into that same file, paced to at most
//! `duty` of the wall time.
//!
//! Threads and timers stay with the caller: [`Migration::run_once`] is one `_run` round and
//! returns the timers the Python would start ([`Timers`]). The writer lock and index-publication
//! guard of a tenant's file are the caller's [`FileGuard`].

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::embedding::config::{ChangeError, EmbeddingConfig};
use crate::embedding::EmbeddingConfigError;
use crate::slots;
use crate::{Error, Target};

/// Fixed synthetic strings for the same-model endpoint check: never diary content.
pub const PARITY_SAMPLE: [&str; 4] = [
    "The quick brown fox jumps over the lazy dog.",
    "Quarterly budget review moved to Tuesday afternoon.",
    "function add(a, b) { return a + b; }",
    "Heute regnet es, morgen soll die Sonne scheinen.",
];
pub const PARITY_MIN_COSINE: f64 = 0.995;

/// Embeds stored chunk text as documents for a target (the caller frames, fits and sends).
pub trait Embed {
    fn embed(&self, target: &Target, texts: &[String]) -> Result<Vec<Vec<f64>>, String>;
}

/// Python `tenant_writer.hold(tenant_of(path))` + `index_publication(path)` around one write.
///
/// The Python connection also carries `tenant_writer`'s SQLite authorizer (no `ATTACH`, no write
/// while a backup effect is pending, no write to a writer tenant's index without its lock). Here
/// that is the managed implementation's job: it must refuse (`Error::Busy`) in those states.
pub trait FileGuard {
    fn write(&self, path: &Path, write: &mut dyn FnMut() -> crate::Result<()>)
        -> crate::Result<()>;
}

/// No guard (an index no other process writes).
pub struct Unguarded;

impl FileGuard for Unguarded {
    fn write(
        &self,
        _path: &Path,
        write: &mut dyn FnMut() -> crate::Result<()>,
    ) -> crate::Result<()> {
        write()
    }
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return f64::NAN;
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb = b.iter().map(|y| y * y).sum::<f64>().sqrt();
    if na != 0.0 && nb != 0.0 {
        dot / (na * nb)
    } else {
        f64::NAN
    }
}

/// Python `_clamp(value, default, lo, hi)`.
pub fn clamp(value: Option<f64>, default: f64, lo: f64, hi: f64) -> f64 {
    match value {
        Some(v) if !v.is_nan() => v.max(lo).min(hi),
        _ => default,
    }
}

/// The service index and every tenant's index files, existing ones only (never created here).
pub fn index_files(service_db: &Path, skip_user: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if service_db.is_file() {
        out.push(service_db.to_path_buf());
    }
    let users = service_db.parent().map(|p| p.join("users"));
    if let Some(users) = users.filter(|u| u.is_dir()) {
        let mut tenants: Vec<PathBuf> = std::fs::read_dir(&users)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        tenants.sort();
        for tenant in tenants {
            let name = tenant
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if skip_user(&name) {
                continue;
            }
            for file in ["index.db", "managed-index.db"] {
                let f = tenant.join(file);
                if f.is_file() {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// How far a target's vectors cover the index files (Python `_coverage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    pub files: usize,
    pub chunks: i64,
    pub vectors: i64,
    pub unavailable: usize,
    pub complete: bool,
    pub ready: bool,
}

impl Coverage {
    pub fn to_json(&self) -> Value {
        json!({"files": self.files, "chunks": self.chunks, "vectors": self.vectors, "unavailable": self.unavailable,
               "complete": self.complete, "ready": self.ready})
    }
}

/// One file's fill (Python `_fill`'s dict).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fill {
    pub embedded: i64,
    pub refused: usize,
    pub more: bool,
    pub aborted: bool,
    pub unavailable: bool,
    pub stuck: bool,
}

/// One pass (Python `_pass`'s dict).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pass {
    pub embedded: i64,
    pub failed: i64,
    pub refused: i64,
    /// "<class>: <message>" of the last file that failed, as `type(exc).__name__`.
    pub error: Option<String>,
    pub more: bool,
    pub aborted: bool,
}

impl Pass {
    pub fn to_json(&self) -> Value {
        let mut out = json!({"embedded": self.embedded, "failed": self.failed, "refused": self.refused, "error": self.error});
        if let Some(map) = out.as_object_mut() {
            if self.aborted {
                map.insert("aborted".into(), json!(true));
            } else {
                map.insert("more".into(), json!(self.more));
            }
        }
        out
    }
}

/// The timers `_run` starts after a pass (seconds from now).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Timers {
    /// `_retry_timer`: a failed pass while a re-index is under way.
    pub retry: Option<f64>,
    /// `_recheck_timer`: the budget left over or the soonest cooldown end.
    pub recheck: Option<f64>,
}

type RefusalKey = (String, i64, String);

pub struct Options {
    pub duty: Option<f64>,
    pub batch: Option<f64>,
    pub retry_s: f64,
    pub cooldown_s: f64,
    pub max_cooldown_s: f64,
    pub active_budget: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            duty: None,
            batch: Some(4.0),
            retry_s: 60.0,
            cooldown_s: 600.0,
            max_cooldown_s: 86400.0,
            active_budget: 64,
        }
    }
}

pub struct Migration<'a> {
    pub config: &'a Mutex<EmbeddingConfig>,
    embed: &'a (dyn Embed + Sync),
    guard: &'a (dyn FileGuard + Sync),
    service_db: PathBuf,
    pub duty: f64,
    pub batch: i64,
    skip_user: Box<dyn Fn(&str) -> bool + Send + Sync + 'a>,
    stop: AtomicBool,
    now: Box<dyn Fn() -> f64 + Send + Sync + 'a>,
    sleep: Box<dyn Fn(f64) + Send + Sync + 'a>,
    retry_s: f64,
    cooldown_s: f64,
    max_cooldown_s: f64,
    active_budget: i64,
    refusals: Mutex<HashMap<RefusalKey, (u32, f64)>>,
    retry_pending: Mutex<bool>,
    recheck_pending: Mutex<bool>,
    pub last_pass: Mutex<Option<Value>>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The pass's `error` text: the Python exception class a Rust error stands for, then the SQLite
/// error code (known difference: Python's text carries sqlite3's own message there).
fn error_text(e: &Error) -> String {
    let class = match e {
        Error::Sqlite(code) => match code.as_str() {
            "ConstraintViolation" | "TypeMismatch" => "IntegrityError",
            "NotADatabase" | "DatabaseCorrupt" | "AuthorizationForStatementDenied" => {
                "DatabaseError"
            }
            "TooBig" => "DataError",
            "ParameterOutOfRange" | "ApiMisuse" => "ProgrammingError",
            _ => "OperationalError",
        },
        Error::BadWidth(_) => "ValueError",
        Error::Busy(_) => "WriterBusy",
        Error::Value(_) => "OverflowError",
    };
    format!("{class}: {e}")
}

impl<'a> Migration<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &'a Mutex<EmbeddingConfig>,
        embed: &'a (dyn Embed + Sync),
        guard: &'a (dyn FileGuard + Sync),
        service_db: &Path,
        options: Options,
        skip_user: Box<dyn Fn(&str) -> bool + Send + Sync + 'a>,
        now: Box<dyn Fn() -> f64 + Send + Sync + 'a>,
        sleep: Box<dyn Fn(f64) + Send + Sync + 'a>,
    ) -> Self {
        let cooldown = options.cooldown_s.max(1.0);
        Self {
            config,
            embed,
            guard,
            service_db: service_db.to_path_buf(),
            duty: clamp(options.duty, 0.5, 0.05, 1.0),
            #[allow(clippy::cast_possible_truncation)]
            batch: clamp(options.batch, 4.0, 1.0, 16.0) as i64,
            skip_user,
            stop: AtomicBool::new(false),
            now,
            sleep,
            retry_s: options.retry_s,
            cooldown_s: cooldown,
            max_cooldown_s: cooldown.max(options.max_cooldown_s),
            active_budget: options.active_budget.max(1),
            refusals: Mutex::new(HashMap::new()),
            retry_pending: Mutex::new(false),
            recheck_pending: Mutex::new(false),
            last_pass: Mutex::new(None),
        }
    }

    fn files(&self) -> Vec<PathBuf> {
        index_files(&self.service_db, &*self.skip_user)
    }

    fn active(&self) -> Target {
        locked(self.config).active()
    }

    fn next(&self) -> Option<Target> {
        locked(self.config).next()
    }

    fn previous(&self) -> Option<Target> {
        locked(self.config).previous()
    }

    fn version(&self) -> u64 {
        locked(self.config).get().version
    }

    /// Opens an existing index file read-write (never creates one) with the vector extension;
    /// None when it is gone, has no index schema, or vectors are unavailable.
    fn open(&self, path: &Path) -> Option<Connection> {
        if sqlite_vec_loader::register().is_err() {
            return None;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        )
        .ok()?;
        conn.busy_timeout(Duration::from_secs(30)).ok()?;
        let has = |name: &str| {
            conn.query_row("SELECT 1 FROM sqlite_master WHERE name = ?", [name], |_| {
                Ok(())
            })
            .optional()
            .ok()
            .flatten()
            .is_some()
        };
        let vec_ok = conn
            .query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
            .is_ok();
        if !vec_ok || !has("chunks") || !has("vec_meta") {
            return None;
        }
        Some(conn)
    }

    /// Python `_coverage`: a file that cannot be opened counts as unavailable; a read error in an
    /// open file is an error, as the Python raises it.
    pub fn coverage(&self, target: Option<&Target>) -> crate::Result<Option<Coverage>> {
        let Some(target) = target else {
            return Ok(None);
        };
        let files = self.files();
        let active = self.active();
        let (mut chunks, mut vectors, mut unavailable, mut behind) = (0i64, 0i64, 0usize, 0usize);
        for f in &files {
            let Some(conn) = self.open(f) else {
                unavailable += 1;
                continue;
            };
            let read = || -> crate::Result<(slots::Coverage, Option<slots::Coverage>)> {
                let c = slots::coverage(&conn, slots::find_slot(&conn, Some(target))?.as_ref())?;
                let a = if *target != active {
                    Some(slots::coverage(
                        &conn,
                        slots::find_slot(&conn, Some(&active))?.as_ref(),
                    )?)
                } else {
                    None
                };
                Ok((c, a))
            };
            let (c, a) = read()?;
            chunks += c.chunks;
            vectors += c.vectors;
            if a.is_some_and(|a| c.vectors < a.vectors) {
                behind += 1;
            }
        }
        Ok(Some(Coverage {
            files: files.len(),
            chunks,
            vectors,
            unavailable,
            complete: unavailable == 0 && vectors >= chunks,
            ready: unavailable == 0 && behind == 0,
        }))
    }

    /// (id, hash, vector) for the rows the server embeds; a refused chunk is skipped this pass.
    fn embed_rows(
        &self,
        target: &Target,
        rows: &[slots::Missing],
        refused: &mut BTreeSet<i64>,
    ) -> Vec<(i64, String, Vec<f64>)> {
        let texts: Vec<String> = rows.iter().map(|r| r.body.clone()).collect();
        match self.embed.embed(target, &texts) {
            Ok(vectors) => rows
                .iter()
                .zip(vectors)
                .map(|(r, v)| (r.id, r.content_hash.clone(), v))
                .collect(),
            Err(_) => {
                if let [row] = rows {
                    refused.insert(row.id);
                    return Vec::new();
                }
                // Try one by one, so one bad chunk does not block the file.
                rows.iter()
                    .flat_map(|r| self.embed_rows(target, std::slice::from_ref(r), refused))
                    .collect()
            }
        }
    }

    /// Ids of this file's chunks still inside their refusal cooldown.
    fn cooling(&self, path: &Path) -> BTreeSet<i64> {
        let now = (self.now)();
        let key = path.to_string_lossy().into_owned();
        locked(&self.refusals)
            .iter()
            .filter(|((f, _, _), (_, until))| *f == key && *until > now)
            .map(|((_, id, _), _)| *id)
            .collect()
    }

    fn note_refused(&self, path: &Path, rows: &[slots::Missing], refused: &BTreeSet<i64>) {
        let now = (self.now)();
        let key = path.to_string_lossy().into_owned();
        let mut map = locked(&self.refusals);
        for row in rows.iter().filter(|r| refused.contains(&r.id)) {
            let k = (key.clone(), row.id, row.content_hash.clone());
            let n = map.get(&k).map_or(0, |(n, _)| *n) + 1;
            let wait = (self.cooldown_s * 2f64.powi(i32::try_from(n - 1).unwrap_or(i32::MAX)))
                .min(self.max_cooldown_s);
            map.insert(k, (n, now + wait));
        }
    }

    fn halted(&self, version: u64) -> bool {
        self.stop.load(Ordering::SeqCst) || self.version() != version
    }

    /// Python `_fill`.
    pub fn fill(
        &self,
        path: &Path,
        target: &Target,
        version: u64,
        mut budget: Option<&mut i64>,
    ) -> crate::Result<Fill> {
        let mut out = Fill::default();
        let mut refused: BTreeSet<i64> = BTreeSet::new();
        let active = *target == self.active();
        let cooling = if active {
            self.cooling(path)
        } else {
            BTreeSet::new()
        };
        loop {
            if self.halted(version) {
                return Ok(Fill {
                    aborted: true,
                    ..out
                });
            }
            let Some(conn) = self.open(path) else {
                return Ok(Fill {
                    unavailable: true,
                    ..out
                });
            };
            let limit = match budget.as_deref() {
                None => self.batch,
                Some(b) => self.batch.min(*b),
            };
            if limit <= 0 {
                return Ok(Fill {
                    more: true,
                    refused: refused.len(),
                    ..out
                });
            }
            let exclude: Vec<i64> = refused.union(&cooling).copied().collect();
            let rows = slots::missing_chunks(
                &conn,
                slots::find_slot(&conn, Some(target))?.as_ref(),
                limit,
                &exclude,
            )?;
            if rows.is_empty() {
                return Ok(Fill {
                    refused: refused.len(),
                    ..out
                });
            }
            if let Some(b) = budget.as_deref_mut() {
                *b -= i64::try_from(rows.len()).unwrap_or(i64::MAX);
            }
            let started = (self.now)();
            let refused_before = refused.len();
            let done = self.embed_rows(target, &rows, &mut refused);
            if active {
                self.note_refused(path, &rows, &refused);
                let key = path.to_string_lossy().into_owned();
                let mut map = locked(&self.refusals);
                for (id, h, _) in &done {
                    map.remove(&(key.clone(), *id, h.clone()));
                }
            }
            if self.halted(version) {
                return Ok(Fill {
                    aborted: true,
                    ..out
                });
            }
            let mut n = 0usize;
            let mut aborted = false;
            if let Some((_, _, first)) = done.first() {
                let width = first.len();
                self.guard.write(path, &mut || {
                    conn.execute_batch("BEGIN IMMEDIATE")?;
                    let body = || -> crate::Result<Option<usize>> {
                        // The embedding took a while: check again, now that nothing else can write.
                        if self.halted(version) {
                            return Ok(None);
                        }
                        slots::adopt_legacy(&conn, Some(&self.active()))?;
                        let slot = slots::ensure_slot(&conn, target, width)?;
                        let mut current: Vec<(i64, &str, &[f64])> = Vec::new();
                        for (id, h, v) in &done {
                            let still = conn
                                .query_row(
                                    "SELECT 1 FROM chunks WHERE id = ? AND content_hash = ?",
                                    rusqlite::params![id, h],
                                    |_| Ok(()),
                                )
                                .optional()?
                                .is_some();
                            if still {
                                current.push((*id, h.as_str(), v.as_slice()));
                            }
                        }
                        Ok(Some(slots::write_vectors(&conn, &slot, &current)?))
                    };
                    match body() {
                        Ok(Some(written)) => match conn.execute_batch("COMMIT") {
                            Ok(()) => {
                                n = written;
                                Ok(())
                            }
                            Err(e) => {
                                let _ = conn.execute_batch("ROLLBACK");
                                Err(e.into())
                            }
                        },
                        Ok(None) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            aborted = true;
                            Ok(())
                        }
                        Err(e) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            Err(e)
                        }
                    }
                })?;
            }
            drop(conn);
            if aborted {
                return Ok(Fill {
                    aborted: true,
                    ..out
                });
            }
            out.embedded += i64::try_from(n).unwrap_or(i64::MAX);
            // Known difference (a Python defect): when nothing was written and no new chunk was
            // refused in this round, the next round would read the same rows again, for ever.
            // Python only stops when nothing was ever refused; here the fill reports stuck.
            if n == 0 && (refused.is_empty() || refused.len() == refused_before) {
                return Ok(Fill { stuck: true, ..out });
            }
            let busy = (self.now)() - started;
            (self.sleep)((busy * (1.0 - self.duty) / self.duty).max(0.05));
        }
    }

    /// Python `_pass`.
    pub fn pass(&self) -> Pass {
        let version = self.version();
        let mut out = Pass::default();
        let targets: Vec<Target> = [Some(self.active()), self.next()]
            .into_iter()
            .flatten()
            .collect();
        let mut budget = self.active_budget;
        for f in self.files() {
            for target in &targets {
                let is_active = *target == self.active();
                let r = self.fill(
                    &f,
                    target,
                    version,
                    if is_active { Some(&mut budget) } else { None },
                );
                let r = match r {
                    Ok(r) => r,
                    Err(e) => {
                        out.failed += 1;
                        out.error = Some(error_text(&e));
                        continue;
                    }
                };
                out.embedded += r.embedded;
                if r.stuck || r.unavailable {
                    out.failed += 1;
                }
                out.refused += i64::try_from(r.refused).unwrap_or(i64::MAX);
                out.more = out.more || r.more;
                if r.aborted {
                    return Pass {
                        aborted: true,
                        more: false,
                        ..out
                    };
                }
            }
        }
        out
    }

    /// One `_run` round: a pass, its record (stamped with `wall()` read after the pass, Python's
    /// `time.time()`), and the timers to start.
    pub fn run_once(&self, wall: &dyn Fn() -> f64) -> Timers {
        let r = self.pass();
        let mut record = Map::new();
        record.insert("at".into(), json!(wall()));
        if let Some(obj) = r.to_json().as_object() {
            record.extend(obj.clone());
        }
        *locked(&self.last_pass) = Some(Value::Object(record));
        let mut timers = Timers::default();
        let stopped = self.stop.load(Ordering::SeqCst);
        if r.failed > 0 && self.next().is_some() && !stopped {
            let mut pending = locked(&self.retry_pending);
            if !*pending {
                *pending = true;
                timers.retry = Some(self.retry_s);
            }
        }
        if !stopped {
            timers.recheck = self.schedule_recheck(r.more);
        }
        timers
    }

    fn schedule_recheck(&self, more: bool) -> Option<f64> {
        let mut pending = locked(&self.recheck_pending);
        if *pending {
            return None;
        }
        let now = (self.now)();
        let mut waits: Vec<f64> = if more { vec![self.retry_s] } else { Vec::new() };
        waits.extend(
            locked(&self.refusals)
                .values()
                .filter(|(_, until)| *until > now)
                .map(|(_, until)| (until - now).max(0.0)),
        );
        let wait = waits.into_iter().reduce(f64::min)?;
        *pending = true;
        Some(wait.max(1.0))
    }

    /// The caller's timer fired (Python `_retry` / `_recheck`): it may be started again.
    pub fn retry_fired(&self) {
        *locked(&self.retry_pending) = false;
    }

    pub fn recheck_fired(&self) {
        *locked(&self.recheck_pending) = false;
    }

    /// Admin: retry every chunk without a vector now, cooldowns cleared (the caller kicks a pass).
    pub fn clear_refusals(&self) {
        locked(&self.refusals).clear();
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        *locked(&self.retry_pending) = false;
        *locked(&self.recheck_pending) = false;
    }

    /// Python `status()`; `running` is the caller's (whether a pass is under way).
    pub fn status(&self, running: bool) -> Result<Value, MigrationError> {
        let snap = locked(self.config).get();
        let mut out = snap.to_json();
        if let Some(map) = out.as_object_mut() {
            let active = self
                .coverage(Some(&snap.active))
                .map_err(MigrationError::Index)?
                .map(|c| c.to_json());
            let next = self
                .coverage(snap.next.as_ref())
                .map_err(MigrationError::Index)?
                .map(|c| c.to_json());
            map.insert("coverage".into(), json!({"active": active, "next": next}));
            map.insert("running".into(), json!(running));
            map.insert(
                "last_pass".into(),
                locked(&self.last_pass).clone().unwrap_or(Value::Null),
            );
            map.insert("duty".into(), json!(self.duty));
            map.insert("batch".into(), json!(self.batch));
        }
        Ok(out)
    }

    fn drop_everywhere(&self, target: &Target) {
        for f in self.files() {
            let Some(conn) = self.open(&f) else {
                continue;
            };
            // Failures are logged by the Python (sqlite3.Error, WriterBusy) and skipped.
            let _ = self.guard.write(&f, &mut || {
                conn.execute_batch("BEGIN IMMEDIATE")?;
                match slots::drop_slot(&conn, target) {
                    Ok(_) => match conn.execute_batch("COMMIT") {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            Err(e.into())
                        }
                    },
                    Err(e) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        Err(e)
                    }
                }
            });
        }
    }

    fn drop_if_orphaned(&self, target: Option<&Target>) {
        let Some(target) = target else {
            return;
        };
        let held: Vec<String> = [Some(self.active()), self.next(), self.previous()]
            .into_iter()
            .flatten()
            .map(|t| t.identity())
            .collect();
        if !held.contains(&target.identity()) {
            self.drop_everywhere(target);
        }
    }

    fn change<T>(r: Result<T, ChangeError>) -> Result<T, MigrationError> {
        r.map_err(|e| match e {
            // A refusal is the configuration's error, as Python raises it.
            ChangeError::Config(c) => MigrationError::Config(c, None),
            other => MigrationError::Change(other),
        })
    }

    /// Python `set_next` (the caller kicks a pass afterwards).
    pub fn set_next(&self, raw: &Value) -> Result<Value, MigrationError> {
        let before = self.next();
        Self::change(locked(self.config).set_next(raw))?;
        let after = self.next();
        if let (Some(before), Some(after)) = (&before, &after) {
            let prev = self
                .previous()
                .map_or_else(|| Target::new(None, "", "").identity(), |p| p.identity());
            if before.identity() != after.identity()
                && before.identity() != self.active().identity()
                && before.identity() != prev
            {
                self.drop_everywhere(before);
            }
        }
        self.status(false)
    }

    pub fn clear_next(&self) -> Result<Value, MigrationError> {
        let before = self.next();
        Self::change(locked(self.config).clear_next())?;
        if let Some(before) = before {
            let prev = self
                .previous()
                .map_or_else(|| Target::new(None, "", "").identity(), |p| p.identity());
            if before.identity() != self.active().identity() && before.identity() != prev {
                self.drop_everywhere(&before);
            }
        }
        self.status(false)
    }

    /// Python `switch` (the caller kicks a pass afterwards).
    pub fn switch(&self, force: bool) -> Result<Value, MigrationError> {
        let (next, active) = (self.next(), self.active());
        let Some(next) = next else {
            return Err(MigrationError::Config(
                EmbeddingConfigError::new("No re-index is under way.", "noNext", 409),
                None,
            ));
        };
        if next.identity() == active.identity() {
            let sample: Vec<String> = PARITY_SAMPLE.iter().map(|s| (*s).to_owned()).collect();
            // The second sample is embedded only when the first one was (Python's try block).
            let both = self
                .embed
                .embed(&active, &sample)
                .and_then(|a| self.embed.embed(&next, &sample).map(|b| (a, b)));
            let ok = match both {
                Ok((a, b)) => {
                    let sims: Vec<f64> = a.iter().zip(&b).map(|(x, y)| cosine(x, y)).collect();
                    sims.len() == PARITY_SAMPLE.len()
                        && sims.iter().all(|s| !s.is_nan())
                        && sims
                            .iter()
                            .copied()
                            .reduce(f64::min)
                            .is_some_and(|m| m >= PARITY_MIN_COSINE)
                }
                Err(_) => false,
            };
            if !ok && !force {
                return Err(MigrationError::Config(
                    EmbeddingConfigError::new("The new embeddings address does not give the same vectors as the current one.", "parityFailed", 409),
                    None,
                ));
            }
        } else if !force {
            let cov = self.coverage(Some(&next)).map_err(MigrationError::Index)?;
            if !cov.is_some_and(|c| c.ready) {
                return Err(MigrationError::Config(
                    EmbeddingConfigError::new(
                        "The new model has not finished indexing every Diary file yet.",
                        "notReady",
                        409,
                    ),
                    Some(cov.map_or(Value::Null, |c| c.to_json())),
                ));
            }
        }
        let displaced = self.previous();
        Self::change(locked(self.config).switch_to_next(Some(&next)))?;
        // An older rollback target is displaced by the new one: its vectors would never be read.
        self.drop_if_orphaned(displaced.as_ref());
        self.status(false)
    }

    pub fn rollback(&self) -> Result<Value, MigrationError> {
        let abandoned = self.next();
        Self::change(locked(self.config).rollback())?;
        self.drop_if_orphaned(abandoned.as_ref());
        self.status(false)
    }

    pub fn finish(&self) -> Result<Value, MigrationError> {
        let Some(old) = self.previous() else {
            return Err(MigrationError::Config(
                EmbeddingConfigError::new(
                    "There is no previous embedding model to remove.",
                    "noPrevious",
                    409,
                ),
                None,
            ));
        };
        Self::change(locked(self.config).finish())?;
        let next = self.next();
        if old.identity() != self.active().identity()
            && next.is_none_or(|n| old.identity() != n.identity())
        {
            self.drop_everywhere(&old);
        }
        self.status(false)
    }
}

/// A refused action: the configuration's refusal (with the coverage of a notReady switch), or a
/// change the configuration could not save.
#[derive(Debug)]
pub enum MigrationError {
    Config(EmbeddingConfigError, Option<Value>),
    Change(ChangeError),
    /// An index file could not be read (Python raises the sqlite3 error out of `_coverage`).
    Index(Error),
}
