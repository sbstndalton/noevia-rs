//! D5: the write side of an app-managed Diary, byte-compatible with managed_storage.py and
//! journal.py. Nothing here creates a database: every connection opens an existing file
//! read-write (Python's `mode=rw`), so a tenant removed meanwhile is never revived.
//!
//! SINGLE WRITER. A tenant's writes are serialized by `Writers::lock` for the whole operation
//! (journal enqueue, guarded corpus write, mark applied), the counterpart of Python's
//! `store._write_lock` + `managed.migration_lock()`. Python forwards a tenant's writes here only
//! when that tenant's writer is this process (rust_forward.py, `DIARY_RUST_WRITES`); it then
//! performs none itself. `fence` lets Python's tenant deletion wait for an in-flight write and
//! refuse later ones.

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// ManagedCorpusBackend(debounce=3.0): `due = now + debounce` after every save.
const DEBOUNCE_S: f64 = 3.0;
/// Python's tenant tombstone lifetime (app.py `_TENANT_DELETED_TTL_S`).
const FENCE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
pub struct Error(pub String);

impl<E: std::fmt::Display> From<E> for Error {
    fn from(e: E) -> Self {
        Error(e.to_string())
    }
}

// ------------------------------------------------------------------ per-tenant writer

/// One lock per tenant, plus the tenants Python is deleting.
#[derive(Default)]
pub struct Writers {
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    fenced: Mutex<HashMap<String, Instant>>,
}

pub struct WriteGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl Writers {
    fn slot(&self, uid: &str) -> Arc<Mutex<()>> {
        let mut map = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(uid.to_string()).or_default().clone()
    }

    /// Hold the tenant's writer for one operation. None: the tenant is fenced (being deleted).
    pub fn with_lock<T>(&self, uid: &str, f: impl FnOnce() -> T) -> Option<T> {
        let slot = self.slot(uid);
        let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        // Checked under the tenant lock: a fence taken before this point is always seen.
        if self.is_fenced(uid) {
            return None;
        }
        let _held = WriteGuard { _guard: guard };
        Some(f())
    }

    pub fn is_fenced(&self, uid: &str) -> bool {
        let mut map = self.fenced.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, at| at.elapsed() < FENCE_TTL);
        map.contains_key(uid)
    }

    /// Python's DELETE /api/internal/tenant: refuse every later write for `uid` and wait for the
    /// one in flight (if any) to finish before Python removes the tenant root.
    pub fn fence(&self, uid: &str) {
        self.fenced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uid.to_string(), Instant::now());
        let slot = self.slot(uid);
        drop(slot.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

// ------------------------------------------------------------------ Python value formatting

/// Python `str(float)` (repr) for a finite value in decimal range (time.time() values).
pub fn py_float(v: f64) -> Option<String> {
    if !crate::workspace::float_ok(v) {
        return None;
    }
    let s = format!("{v}");
    Some(if s.contains('.') { s } else { format!("{s}.0") })
}

pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `datetime.now().isoformat(timespec="seconds")`: naive local time.
pub fn local_iso_seconds() -> String {
    chrono::Local::now()
        .naive_local()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

/// `str(uuid.uuid4())`.
pub fn uuid4() -> Result<String, Error> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    if let Some(x) = b.get_mut(6) {
        *x = (*x & 0x0f) | 0x40;
    }
    if let Some(x) = b.get_mut(8) {
        *x = (*x & 0x3f) | 0x80;
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    let part = |a: usize, z: usize| hex.get(a..z).unwrap_or("").to_string();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        part(0, 8),
        part(8, 12),
        part(12, 16),
        part(16, 20),
        part(20, 32)
    ))
}

/// A JSON value for `json.dumps(payload, ensure_ascii=False)` with the default separators.
pub enum PyVal<'a> {
    Str(&'a str),
    Bool(bool),
}

fn json_str(s: &str) -> String {
    // serde_json's string escaping is Python's for ensure_ascii=False: `"`, `\`, \b \f \n \r \t,
    // other controls as \u00xx (lowercase), everything else literal.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// `json.dumps(dict, ensure_ascii=False)`: `{"k": v, "k2": v2}` in insertion order.
pub fn py_json(pairs: &[(&str, PyVal<'_>)]) -> String {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| {
            let value = match v {
                PyVal::Str(s) => json_str(s),
                PyVal::Bool(true) => "true".into(),
                PyVal::Bool(false) => "false".into(),
            };
            format!("{}: {value}", json_str(k))
        })
        .collect();
    format!("{{{}}}", items.join(", "))
}

// ------------------------------------------------------------------ connections

pub(crate) fn open_rw(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(30))?;
    Ok(conn)
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `int(meta('generation', '0')) + 1`, only for the canonical decimal text Python writes.
fn next_generation(text: Option<String>) -> Result<String, Error> {
    let text = text.unwrap_or_else(|| "0".into());
    let digits = text.strip_prefix('-').unwrap_or(&text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error("non-canonical generation".into()));
    }
    let n: i64 = text.parse()?;
    Ok(n.checked_add(1)
        .ok_or_else(|| Error("generation overflow".into()))?
        .to_string())
}

/// The bookkeeping every managed save commits with its data (managed_storage.py put /
/// create_directory): generation + 1, `due = now + 3 s`, `snapshot_time = now`.
pub(crate) fn bump(tx: &Connection, now: f64) -> Result<(), Error> {
    let generation: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let next = next_generation(generation)?;
    let due = py_float(now + DEBOUNCE_S).ok_or_else(|| Error("clock".into()))?;
    let snap = py_float(now).ok_or_else(|| Error("clock".into()))?;
    tx.execute(
        "INSERT OR REPLACE INTO meta VALUES (?,?)",
        ["generation", next.as_str()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO meta VALUES (?,?)",
        ["due", due.as_str()],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO meta VALUES (?,?)",
        ["snapshot_time", snap.as_str()],
    )?;
    Ok(())
}

// ------------------------------------------------------------------ managed corpus writes

pub struct Store {
    db_path: PathBuf,
}

/// `ManagedCorpusBackend.put` outcomes.
#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    /// (ETag, 201 created | 204 replaced)
    Ok(String, u16),
    /// 412: the If-Match/If-None-Match condition failed.
    Precondition,
    /// IsADirectoryError(path) / NotADirectoryError(ancestor): `str(exc)` is the path given.
    PathConflict(String),
}

/// `ManagedCorpusBackend.create_directory` outcomes.
#[derive(Debug, PartialEq, Eq)]
pub enum Mkdir {
    Ok,
    Exists,
    NoParent,
}

impl Store {
    pub fn new(db_path: &Path) -> Self {
        Store {
            db_path: db_path.to_path_buf(),
        }
    }

    /// (data, ETag) of a file, raw bytes.
    pub fn get(&self, path: &str) -> Result<Option<(Vec<u8>, String)>, Error> {
        let conn = open_rw(&self.db_path)?;
        let row: Option<(Vec<u8>, String)> = conn
            .query_row("SELECT data,version FROM files WHERE path=?", [path], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        Ok(row.map(|(d, v)| (d, format!("\"sha256:{v}\""))))
    }

    /// `put(path, data, if_match)` with Python's `if_none_match='*'` default.
    pub fn put(&self, path: &str, data: &[u8], if_match: Option<&str>) -> Result<Put, Error> {
        if !crate::managed::safe_key(path) {
            return Err(Error("Invalid corpus path".into()));
        }
        let mut conn = open_rw(&self.db_path)?;
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<String> = tx
            .query_row("SELECT version FROM files WHERE path=?", [path], |r| {
                r.get(0)
            })
            .optional()?;
        let current = row.as_ref().map(|v| format!("\"sha256:{v}\""));
        let refused = match if_match {
            Some(tag) => current.as_deref() != Some(tag),
            None => row.is_some(),
        };
        if refused {
            return Ok(Put::Precondition);
        }
        let is_dir = tx
            .query_row("SELECT 1 FROM directories WHERE path=?", [path], |_| Ok(()))
            .optional()?
            .is_some()
            || tx
                .query_row(
                    "SELECT 1 FROM files WHERE substr(path,1,?)=? LIMIT 1",
                    rusqlite::params![path.chars().count() as i64 + 1, format!("{path}/")],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
        if is_dir {
            return Ok(Put::PathConflict(path.to_string()));
        }
        let parts: Vec<&str> = path.split('/').collect();
        for n in (1..parts.len()).rev() {
            let ancestor = parts.get(..n).unwrap_or(&[]).join("/");
            if tx
                .query_row("SELECT 1 FROM files WHERE path=?", [&ancestor], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Ok(Put::PathConflict(ancestor));
            }
        }
        let version = sha256_hex(data);
        let now = now_epoch();
        tx.execute(
            "INSERT OR REPLACE INTO files VALUES (?,?,?,?)",
            rusqlite::params![path, data, version, now],
        )?;
        bump(&tx, now)?;
        tx.commit()?;
        Ok(Put::Ok(
            format!("\"sha256:{version}\""),
            if row.is_some() { 204 } else { 201 },
        ))
    }

    pub fn create_directory(&self, path: &str) -> Result<Mkdir, Error> {
        if !crate::managed::safe_key(path) {
            return Err(Error("Invalid corpus path".into()));
        }
        let mut conn = open_rw(&self.db_path)?;
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let all: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT path FROM files UNION SELECT path FROM directories")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let below = format!("{path}/");
        if all.iter().any(|p| p == path || p.starts_with(&below)) {
            return Ok(Mkdir::Exists);
        }
        let parent_below = format!("{parent}/");
        if !parent.is_empty()
            && !all
                .iter()
                .any(|p| p == parent || p.starts_with(&parent_below))
        {
            return Ok(Mkdir::NoParent);
        }
        if !parent.is_empty()
            && tx
                .query_row("SELECT 1 FROM files WHERE path=?", [parent], |_| Ok(()))
                .optional()?
                .is_some()
        {
            return Ok(Mkdir::NoParent);
        }
        tx.execute("INSERT INTO directories VALUES (?)", [path])?;
        bump(&tx, now_epoch())?;
        tx.commit()?;
        Ok(Mkdir::Ok)
    }

    /// CorpusStore._guarded_write: ETag-guarded read-modify-write, up to five attempts. With
    /// this process as the tenant's only writer the first attempt is the one that lands; the
    /// loop is kept for parity. `mutate` gets the current text (errors="replace") and returns
    /// (new text, changed). Ok(()) when applied or already applied.
    pub fn guarded_write(
        &self,
        path: &str,
        mut mutate: impl FnMut(Option<&str>) -> Result<(String, bool), String>,
    ) -> Result<(), WriteFailure> {
        for _ in 0..5 {
            let current = self.get(path).map_err(WriteFailure::Infra)?;
            let (text, etag) = match &current {
                Some((data, etag)) => (
                    Some(String::from_utf8_lossy(data).into_owned()),
                    Some(etag.as_str()),
                ),
                None => (None, None),
            };
            let (new_text, changed) = mutate(text.as_deref()).map_err(WriteFailure::Permanent)?;
            if !changed {
                return Ok(());
            }
            match self.put(path, new_text.as_bytes(), etag) {
                Ok(Put::Ok(..)) => return Ok(()),
                Ok(Put::Precondition) => continue,
                Ok(Put::PathConflict(at)) => return Err(WriteFailure::Infra(Error(at))),
                Err(e) => return Err(WriteFailure::Infra(e)),
            }
        }
        Err(WriteFailure::Infra(Error(format!(
            "could not write {path} after 5 attempts (persistent conflict)"
        ))))
    }
}

#[derive(Debug)]
pub enum WriteFailure {
    /// PermanentApplyError: the entry can never apply (quarantined).
    Permanent(String),
    /// Anything else: the entry stays pending (attempts + 1).
    Infra(Error),
}

// ------------------------------------------------------------------ journal (index db)

pub struct Journal {
    conn: Connection,
}

pub struct Entry {
    pub id: String,
    pub kind: String,
    pub payload: String,
}

impl Journal {
    /// The tenant's existing index db (Python creates it when it builds the state).
    pub fn open(path: &Path) -> Result<Self, Error> {
        Ok(Journal {
            conn: open_rw(path)?,
        })
    }

    pub fn enqueue(&self, kind: &str, payload: &str) -> Result<String, Error> {
        let id = uuid4()?;
        self.conn.execute(
            "INSERT INTO journal (id, created_at, kind, payload) VALUES (?, ?, ?, ?)",
            rusqlite::params![id, local_iso_seconds(), kind, payload],
        )?;
        Ok(id)
    }

    pub fn mark_applied(&self, id: &str) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE journal SET applied = 1, applied_at = ?, last_error = NULL WHERE id = ?",
            rusqlite::params![local_iso_seconds(), id],
        )?;
        Ok(())
    }

    pub fn mark_failed(&self, id: &str, error: &str) -> Result<(), Error> {
        let short: String = error.chars().take(500).collect();
        self.conn.execute(
            "UPDATE journal SET attempts = attempts + 1, last_error = ? WHERE id = ?",
            rusqlite::params![short, id],
        )?;
        Ok(())
    }

    pub fn quarantine(&self, id: &str, error: &str) -> Result<(), Error> {
        let short: String = error.chars().take(2000).collect();
        self.conn.execute(
            "UPDATE journal SET applied = 1, applied_at = ?, attempts = attempts + 1, last_error = ? WHERE id = ?",
            rusqlite::params![local_iso_seconds(), short, id],
        )?;
        Ok(())
    }

    pub fn is_applied(&self, id: &str) -> Result<bool, Error> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT applied FROM journal WHERE id=?", [id], |r| r.get(0))
            .optional()?;
        Ok(v.is_some_and(|a| a != 0))
    }

    pub fn has_exchange(&self) -> Result<bool, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM journal WHERE kind='exchange' LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Unapplied entries in insertion (rowid) order, as journal.unapplied.
    pub fn unapplied(&self, limit: i64) -> Result<Vec<Entry>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, payload FROM journal WHERE applied = 0 ORDER BY rowid LIMIT ?",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(Entry {
                id: r.get(0)?,
                kind: r.get(1)?,
                payload: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_dirty(&self, document: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT OR IGNORE INTO dirty_documents(document) VALUES (?)",
            [document],
        )?;
        Ok(())
    }

    pub fn is_dirty(&self, document: &str) -> Result<bool, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM dirty_documents WHERE document=?",
                [document],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn clear_dirty(&self, document: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM dirty_documents WHERE document=?", [document])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_float_repr() {
        assert_eq!(py_float(1730000000.0).as_deref(), Some("1730000000.0"));
        assert_eq!(py_float(1730000000.25).as_deref(), Some("1730000000.25"));
        assert_eq!(py_float(f64::NAN), None);
    }

    #[test]
    fn python_json_payload() {
        let s = py_json(&[
            ("scaffold", PyVal::Bool(false)),
            ("xid", PyVal::Str("a\"b\n\u{1}é")),
        ]);
        assert_eq!(s, "{\"scaffold\": false, \"xid\": \"a\\\"b\\n\\u0001é\"}");
    }

    #[test]
    fn generation_is_canonical_only() {
        assert_eq!(next_generation(None).unwrap_or_default(), "1");
        assert_eq!(next_generation(Some("41".into())).unwrap_or_default(), "42");
        assert!(next_generation(Some(" 4".into())).is_err());
        assert!(next_generation(Some("4_0".into())).is_err());
    }

    #[test]
    fn uuid4_shape() {
        let id = uuid4().unwrap_or_default();
        assert_eq!(id.len(), 36);
        assert_eq!(id.get(14..15), Some("4"));
        assert!(matches!(id.get(19..20), Some("8" | "9" | "a" | "b")));
    }
}
