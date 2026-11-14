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
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// ManagedCorpusBackend(debounce=3.0): `due = now + debounce` after every save.
const DEBOUNCE_S: f64 = 3.0;
/// Python's tenant tombstone lifetime (app.py `_TENANT_DELETED_TTL_S`).
const FENCE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A write-side failure: the exact message (journal `last_error`, logs) plus a private kind.
/// The kind is set only where the condition is known, never derived from message text: every
/// conversion from another error (`From<Display>`, SQLite triggers included) is `Kind::Other`.
/// Not `Display`, so `?` can never re-wrap an `Error` through the blanket conversion and lose it.
#[derive(Debug)]
pub struct Error {
    message: String,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Other,
    /// A primary reservation (or the primary's own write transaction) found an unsettled backup
    /// intent, before any effect of that transaction.
    SettlementRefused,
    /// The managed primary at the reserved path is no longer the file this reservation holds.
    PrimaryReplaced,
}

impl Error {
    pub fn other(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            kind: Kind::Other,
        }
    }

    /// Only at the explicit pending checks (Store::put, Store::create_directory and
    /// Journal::primary_reservation). The text is Python's WriterBusy reason.
    fn settlement_refused() -> Self {
        Error {
            message: "backup settlement uncertain".into(),
            kind: Kind::SettlementRefused,
        }
    }

    fn primary_replaced() -> Self {
        Error {
            message: "managed primary replaced".into(),
            kind: Kind::PrimaryReplaced,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn into_message(self) -> String {
        self.message
    }

    /// True only for the settlement refusal (Python's `WriterBusy(uid, 'backup settlement
    /// uncertain')` from tenant_writer.index_publication), never for SQLite, I/O, a trigger with
    /// the same text, a replaced primary or any other failure.
    pub fn is_settlement_refusal(&self) -> bool {
        self.kind == Kind::SettlementRefused
    }

    /// A replaced or removed primary. A generic failure on the wire, never BUSY.
    pub fn is_primary_replaced(&self) -> bool {
        self.kind == Kind::PrimaryReplaced
    }
}

impl<E: std::fmt::Display> From<E> for Error {
    fn from(e: E) -> Self {
        Error::other(e.to_string())
    }
}

// ------------------------------------------------------------------ per-tenant writer

/// One writer slot per tenant, plus the tenants Python is deleting.
///
/// R9 main mode (front.rs): the slot is also what the Python child's tenant writer lock holds, as a
/// LEASE (`lease_acquire` / `lease_release` on the control port), so a tenant has one writer across
/// both processes. A request that names the lease its caller holds (`x-noevia-diary-lease`: the
/// re-index Python asks for, the deletion fence) JOINS it instead of waiting for itself. Without
/// leases (Python as the main process, today) every slot is taken only by `with_lock`, as before.
#[derive(Default)]
pub struct Writers {
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    fenced: Mutex<HashMap<String, Instant>>,
    /// Live leases: id → (tenant, since, the caller's holder nonce).
    leases: Mutex<HashMap<u64, (String, Instant, String)>>,
    next: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    freed: Condvar,
}

#[derive(Default)]
struct SlotState {
    /// The holder: a native operation's or a lease's id (ids are never reused).
    holder: Option<u64>,
    /// Operations running joined to the holding lease.
    joins: usize,
    /// The holding lease was released while joined operations ran: the last one frees the slot.
    released: bool,
}

/// Why a slot was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The tenant is fenced (being deleted): native writes only.
    Fenced,
    /// Another holder kept it past the deadline.
    Busy,
}

/// The held writer slot of one tenant, lent to the operation `with_lock` runs. It is the same
/// slot `fence` waits for, so while any `WriteGuard` (and every `Journal` borrowing it) exists,
/// Python's tenant deletion cannot pass its fence and remove the tenant root. Only `Writers`
/// constructs one. A guard joined to a lease keeps the slot even when the lease is returned
/// meanwhile (a caller that gave up waiting): the slot is free only when the operation ends.
pub struct WriteGuard<'a> {
    uid: String,
    slot: Arc<Slot>,
    id: u64,
    joined: bool,
    _writers: std::marker::PhantomData<&'a Writers>,
}

impl WriteGuard<'_> {
    pub fn uid(&self) -> &str {
        &self.uid
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if self.joined {
            self.slot.unjoin(self.id);
        } else {
            self.slot.free(self.id);
        }
    }
}

impl Slot {
    fn state(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the slot for `id`, waiting until `until` (None: without limit). False: still held.
    fn take(&self, id: u64, until: Option<Instant>) -> bool {
        let mut state = self.state();
        while state.holder.is_some() {
            match until {
                None => state = self.freed.wait(state).unwrap_or_else(|e| e.into_inner()),
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    state = self
                        .freed
                        .wait_timeout(state, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        state.holder = Some(id);
        true
    }

    fn free(&self, id: u64) {
        let mut state = self.state();
        if state.holder == Some(id) {
            state.holder = None;
            self.freed.notify_all();
        }
    }

    /// Join the lease `id` when it holds the slot (and was not returned).
    fn join(&self, id: u64) -> bool {
        let mut state = self.state();
        if state.holder != Some(id) || state.released {
            return false;
        }
        state.joins += 1;
        true
    }

    fn unjoin(&self, id: u64) {
        let mut state = self.state();
        if state.holder != Some(id) {
            return;
        }
        state.joins = state.joins.saturating_sub(1);
        if state.joins == 0 && state.released {
            state.holder = None;
            state.released = false;
            self.freed.notify_all();
        }
    }

    /// The lease `id` is returned: the slot is free now, or when its last joined operation ends.
    fn release_lease(&self, id: u64) {
        let mut state = self.state();
        if state.holder != Some(id) {
            return;
        }
        if state.joins > 0 {
            state.released = true;
        } else {
            state.holder = None;
            self.freed.notify_all();
        }
    }
}

impl Writers {
    fn slot(&self, uid: &str) -> Arc<Slot> {
        let mut map = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(uid.to_string()).or_default().clone()
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Hold the tenant's writer for one operation, lending its guard to `f`. None: the tenant is
    /// fenced (being deleted).
    pub fn with_lock<T>(&self, uid: &str, f: impl FnOnce(&WriteGuard<'_>) -> T) -> Option<T> {
        self.with_lock_as(uid, None, None, f).ok()
    }

    /// `with_lock` for main mode: `lease` is the lease the caller says it holds (joined when it is
    /// the tenant's current holder, so the caller never waits for itself), `wait` bounds the wait
    /// (None: without limit).
    pub fn with_lock_as<T>(
        &self,
        uid: &str,
        lease: Option<u64>,
        wait: Option<Duration>,
        f: impl FnOnce(&WriteGuard<'_>) -> T,
    ) -> Result<T, Refused> {
        let slot = self.slot(uid);
        let joined = lease.filter(|l| self.lease_holds(*l, uid) && slot.join(*l));
        let (id, joined) = match joined {
            Some(l) => (l, true),
            None => {
                let id = self.id();
                if !slot.take(id, wait.map(|w| Instant::now() + w)) {
                    return Err(Refused::Busy);
                }
                (id, false)
            }
        };
        let held = WriteGuard {
            uid: uid.to_string(),
            slot,
            id,
            joined,
            _writers: std::marker::PhantomData,
        };
        // Checked under the tenant lock: a fence taken before this point is always seen.
        if self.is_fenced(uid) {
            return Err(Refused::Fenced);
        }
        Ok(f(&held))
    }

    pub fn is_fenced(&self, uid: &str) -> bool {
        let mut map = self.fenced.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, at| at.elapsed() < FENCE_TTL);
        map.contains_key(uid)
    }

    /// Python's DELETE /api/internal/tenant: refuse every later write for `uid` and wait for the
    /// one in flight (if any) to finish before Python removes the tenant root.
    pub fn fence(&self, uid: &str) {
        self.fence_as(uid, None);
    }

    /// `fence` from a caller holding `lease` (Python deletes under its writer lock): when that lease
    /// is the tenant's holder nothing else can be in flight, so it does not wait for itself.
    pub fn fence_as(&self, uid: &str, lease: Option<u64>) {
        self.fenced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uid.to_string(), Instant::now());
        let slot = self.slot(uid);
        if lease.is_some_and(|l| self.lease_holds(l, uid) && slot.join(l)) {
            slot.unjoin(lease.unwrap_or_default());
            return;
        }
        let id = self.id();
        slot.take(id, None);
        slot.free(id);
    }

    /// A lease on the tenant's writer slot for the Python child (main mode), waiting at most
    /// `wait`. A fence does not refuse it: the fence stops this process's own writes, and Python's
    /// own deletion and its 410s run under its lock.
    pub fn lease_acquire(&self, uid: &str, wait: Duration) -> Result<u64, Refused> {
        self.lease_acquire_for(uid, wait, "")
    }

    /// `lease_acquire` naming the caller's `holder` nonce, so a caller that never learnt its lease
    /// id (a lost reply) can still return it (`lease_release_holder`).
    pub fn lease_acquire_for(
        &self,
        uid: &str,
        wait: Duration,
        holder: &str,
    ) -> Result<u64, Refused> {
        let slot = self.slot(uid);
        let id = self.id();
        if !slot.take(id, Some(Instant::now() + wait)) {
            return Err(Refused::Busy);
        }
        self.leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (uid.to_string(), Instant::now(), holder.to_string()));
        Ok(id)
    }

    /// Return every live lease granted to `holder` (a non-empty nonce). The count returned.
    pub fn lease_release_holder(&self, holder: &str) -> usize {
        if holder.is_empty() {
            return 0;
        }
        let ids: Vec<u64> = self
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, (_, _, h))| h == holder)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter().filter(|id| self.lease_release(*id)).count()
    }

    /// End a lease. False when it is not live (already released, or never issued).
    pub fn lease_release(&self, id: u64) -> bool {
        let entry = self
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        match entry {
            Some((uid, _, _)) => {
                self.slot(&uid).release_lease(id);
                true
            }
            None => false,
        }
    }

    fn lease_holds(&self, id: u64, uid: &str) -> bool {
        self.leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .is_some_and(|(u, _, _)| u == uid)
    }

    /// The live leases (tenant, seconds held), longest first: a leak is visible.
    pub fn lease_report(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|(u, at, _)| (u.clone(), at.elapsed().as_secs()))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
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
        return Err(Error::other("non-canonical generation"));
    }
    let n: i64 = text.parse()?;
    Ok(n.checked_add(1)
        .ok_or_else(|| Error::other("generation overflow"))?
        .to_string())
}

/// The bookkeeping every managed save commits with its data (managed_storage.py put /
/// create_directory): generation + 1, `due = now + 3 s`, `snapshot_time = now`.
fn bump(tx: &Connection, now: f64) -> Result<(), Error> {
    let generation: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let next = next_generation(generation)?;
    let due = py_float(now + DEBOUNCE_S).ok_or_else(|| Error::other("clock"))?;
    let snap = py_float(now).ok_or_else(|| Error::other("clock"))?;
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
            return Err(Error::other("Invalid corpus path"));
        }
        let mut conn = open_rw(&self.db_path)?;
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if diary_backup_run::pending_effect(&tx)? {
            return Err(Error::settlement_refused());
        }
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
            return Err(Error::other("Invalid corpus path"));
        }
        let mut conn = open_rw(&self.db_path)?;
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if diary_backup_run::pending_effect(&tx)? {
            return Err(Error::settlement_refused());
        }
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
                Ok(Put::PathConflict(at)) => return Err(WriteFailure::Transient(Error::other(at))),
                Err(e) => return Err(WriteFailure::Infra(e)),
            }
        }
        Err(WriteFailure::Infra(Error::other(format!(
            "could not write {path} after 5 attempts (persistent conflict)"
        ))))
    }
}

#[derive(Debug)]
pub enum WriteFailure {
    /// OSError: storage outages never count towards quarantine.
    Transient(Error),
    /// PermanentApplyError: the entry can never apply (quarantined).
    Permanent(String),
    /// Anything else: the entry stays pending (attempts + 1).
    Infra(Error),
}

// ------------------------------------------------------------------ journal (index db)

pub struct Journal<'h> {
    conn: Connection,
    primary: PathBuf,
    /// The tenant's writer slot, held for as long as this Journal exists.
    _held: &'h WriteGuard<'h>,
}

pub struct Entry {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub rowid: i64,
    pub attempts: i64,
}

/// The primary file a reservation holds open: its (dev, ino) when opened, before SQLite opens the
/// same path. Holding the File keeps that inode allocated, so no other file can take its number
/// meanwhile. Necessary, not sufficient: see `Reservation::verify`.
struct Bound {
    path: PathBuf,
    file: std::fs::File,
    dev: u64,
    ino: u64,
}

impl Bound {
    #[cfg(unix)]
    fn open(path: &Path) -> Result<Self, Error> {
        use std::os::unix::fs::MetadataExt;
        // Read-only and never created: a missing primary stays missing (legacy / removed tenant).
        let file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        Ok(Bound {
            path: path.to_path_buf(),
            file,
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    #[cfg(not(unix))]
    fn open(_path: &Path) -> Result<Self, Error> {
        // No file identity to bind: fail closed.
        Err(Error::primary_replaced())
    }

    /// The path still names the held file, and the held file is still linked.
    #[cfg(unix)]
    fn current(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(held) = self.file.metadata() else {
            return false;
        };
        let Ok(now) = std::fs::metadata(&self.path) else {
            return false;
        };
        held.nlink() > 0 && now.dev() == self.dev && now.ino() == self.ino
    }

    #[cfg(not(unix))]
    fn current(&self) -> bool {
        false
    }
}

/// One Journal-owned reservation of the tenant's primary authority (`managed-diary.db`), lent to
/// the body of `Journal::primary_reservation`. Its transaction is never exposed: every primary
/// statement and every index effect inside it is a narrow method here.
pub struct Reservation<'r, 'h> {
    tx: rusqlite::Transaction<'r>,
    journal: &'r Journal<'h>,
    bound: Bound,
}

impl Reservation<'_, '_> {
    /// Fail closed when the reserved path no longer names the held primary file.
    ///
    /// This detects a replacement (rename-over, unlink and recreate) or removal that is visible at
    /// the time of the check. It does NOT prove SQLite's handle is the held File across arbitrary
    /// replacement and restoration (ABA), and it is not an atomic compare-and-swap with the effect
    /// or COMMIT that follows. Replacement is excluded during the reservation by the writer slot
    /// (`WriteGuard`) that `/internal/fence`, and so Python's tenant deletion, waits for.
    fn verify(&self) -> Result<(), Error> {
        if self.tx.is_autocommit() || !self.bound.current() {
            return Err(Error::primary_replaced());
        }
        Ok(())
    }

    /// One index/journal effect inside the reservation, after an identity check.
    fn publish<T>(&self, effect: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        self.verify()?;
        effect()
    }

    /// The paths in the primary's index outbox.
    pub fn outbox(&self) -> Result<Vec<String>, Error> {
        self.verify()?;
        let mut stmt = self.tx.prepare("SELECT path FROM workspace_index_outbox")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn mark_dirty(&self, document: &str) -> Result<(), Error> {
        self.publish(|| self.journal.insert_dirty(document))
    }

    /// Remove an outbox row whose dirty mark is durable (committed with the reservation).
    pub fn transferred(&self, path: &str) -> Result<(), Error> {
        self.verify()?;
        self.tx
            .execute("DELETE FROM workspace_index_outbox WHERE path=?", [path])?;
        Ok(())
    }
}

impl<'h> Journal<'h> {
    /// The tenant's existing index db (Python creates it when it builds the state), opened under
    /// that tenant's held writer slot.
    pub fn open(path: &Path, held: &'h WriteGuard<'h>) -> Result<Self, Error> {
        let tenant = path
            .parent()
            .ok_or_else(|| Error::other("journal parent"))?;
        if tenant.file_name().and_then(|n| n.to_str()) != Some(held.uid()) {
            return Err(Error::other("journal tenant mismatch"));
        }
        Ok(Journal {
            conn: open_rw(path)?,
            primary: tenant.join("managed-diary.db"),
            _held: held,
        })
    }

    /// The primary authority's reservation, owned by this Journal (the counterpart of Python's
    /// tenant_writer.index_publication). Holding its immediate transaction through `body` keeps
    /// a backup intent from slipping between the check and the actual effect, on a separate
    /// database. It never creates the primary, and never accepts a caller's transaction.
    ///
    /// Commits only when `body` returns Ok; otherwise the reservation rolls back. A refusal
    /// is typed: SettlementRefused (an unsettled intent, before `body` runs) or PrimaryReplaced
    /// (the reserved path no longer names the held file).
    pub fn primary_reservation<T>(
        &self,
        body: impl FnOnce(&Reservation<'_, 'h>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        // Bind the file first, so a replacement before SQLite opens the path is seen below.
        let bound = Bound::open(&self.primary)?;
        let mut primary = open_rw(&self.primary)?;
        let tx = primary.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation = Reservation {
            tx,
            journal: self,
            bound,
        };
        // Identity before the intent check: a replaced primary is never answered as BUSY.
        reservation.verify()?;
        if diary_backup_run::pending_effect(&reservation.tx)? {
            return Err(Error::settlement_refused());
        }
        let result = body(&reservation)?;
        reservation.verify()?;
        reservation.tx.commit()?;
        Ok(result)
    }

    /// One index/journal publication under its own primary reservation.
    fn publication<T>(&self, effect: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        self.primary_reservation(|r| r.publish(effect))
    }

    /// R9 S4: an index write of the embedding migration under the primary reservation (Python's
    /// `tenant_writer.index_publication`: refused while a backup effect is unsettled).
    pub fn publish_effect<T>(&self, effect: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        self.publication(effect)
    }

    pub fn enqueue(&self, kind: &str, payload: &str) -> Result<String, Error> {
        self.publication(|| {
            let id = uuid4()?;
            self.conn.execute(
                "INSERT INTO journal (id, created_at, kind, payload) VALUES (?, ?, ?, ?)",
                rusqlite::params![id, local_iso_seconds(), kind, payload],
            )?;
            Ok(id)
        })
    }

    pub fn mark_applied(&self, id: &str) -> Result<(), Error> {
        self.publication(|| {
            self.conn.execute(
                "UPDATE journal SET applied = 1, applied_at = ?, last_error = NULL WHERE id = ?",
                rusqlite::params![local_iso_seconds(), id],
            )?;
            Ok(())
        })
    }

    pub fn mark_failed(&self, id: &str, error: &str) -> Result<(), Error> {
        self.publication(|| {
            let short: String = error.chars().take(500).collect();
            self.conn.execute(
                "UPDATE journal SET attempts = attempts + 1, last_error = ? WHERE id = ?",
                rusqlite::params![short, id],
            )?;
            Ok(())
        })
    }

    pub fn quarantine(&self, id: &str, error: &str) -> Result<(), Error> {
        self.publication(|| {
        let short: String = error.chars().take(2000).collect();
        self.conn.execute(
            "UPDATE journal SET applied = 1, applied_at = ?, attempts = attempts + 1, last_error = ? WHERE id = ?",
            rusqlite::params![local_iso_seconds(), short, id],
        )?;
        Ok(())
            })
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
            "SELECT id, kind, payload, rowid, attempts FROM journal WHERE applied = 0 ORDER BY rowid LIMIT ?",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(Entry {
                id: r.get(0)?,
                kind: r.get(1)?,
                payload: r.get(2)?,
                rowid: r.get(3)?,
                attempts: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_dirty(&self, document: &str) -> Result<(), Error> {
        self.publication(|| self.insert_dirty(document))
    }

    fn insert_dirty(&self, document: &str) -> Result<(), Error> {
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

    /// The documents marked dirty (Python `journal.dirty_documents()`).
    pub fn dirty_documents(&self) -> Result<Vec<String>, Error> {
        // The same statement as Python (no ORDER BY: the planner reads the primary-key index).
        let mut stmt = self.conn.prepare("SELECT document FROM dirty_documents")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// An index effect of another connection under this tenant's primary reservation (the
    /// retrieval index's writes: Python's `index_publication`), refused when the index outbox
    /// holds paths (Python drains it first; that stays with Python).
    pub fn index_publication<T>(
        &self,
        effect: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.primary_reservation(|r| {
            if !r.outbox()?.is_empty() {
                return Err(Error::other("index outbox not drained"));
            }
            r.publish(effect)
        })
    }

    pub fn clear_dirty(&self, document: &str) -> Result<(), Error> {
        self.publication(|| {
            self.conn
                .execute("DELETE FROM dirty_documents WHERE document=?", [document])?;
            Ok(())
        })
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
