//! Disposable, single-job SQLite connections. No ordinary Store/Writer controls are changed.
//! VM/row bounds do not promise cancellation of kernel I/O, initial schema allocation or OOM.
use super::{Reader, RustAuth, StoreError, WriteTx, KNOWN_SCHEMA_VERSION, REQUIRED_COLUMNS};
use rusqlite::{
    config::DbConfig,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::ValueRef,
    Connection, OpenFlags, Row, TransactionBehavior,
};
use rustix::fd::OwnedFd;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Unavailable,
    Cancelled,
    Deadline,
    Cap,
    Busy,
    SchemaTooNew,
}
/// An error can retain an unclosed connection. Its owner must keep worker admission until cleanup.
#[must_use]
pub struct Error {
    pub fault: Fault,
    pending: Option<Box<Connection>>,
}
impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedError")
            .field("fault", &self.fault)
            .field("pending_close", &self.pending.is_some())
            .finish()
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bounded database unavailable ({:?})", self.fault)
    }
}
impl std::error::Error for Error {}
impl Error {
    fn new(fault: Fault) -> Self {
        Self {
            fault,
            pending: None,
        }
    }
    pub fn has_pending_close(&self) -> bool {
        self.pending.is_some()
    }
    /// Retry only resource close, never a SQL transaction or upkeep write.
    pub fn retry_close(mut self) -> Result<(), Self> {
        if let Some(c) = self.pending.take() {
            match (*c).close() {
                Ok(()) => Ok(()),
                Err((c, _)) => {
                    self.pending = Some(Box::new(c));
                    Err(self)
                }
            }
        } else {
            Ok(())
        }
    }
}
struct Slot {
    handle: OnceLock<rusqlite::InterruptHandle>,
    active: AtomicBool,
}
struct State {
    deadline: Instant,
    cancelled: AtomicBool,
    ticks: AtomicUsize,
    tick_limit: usize,
    rows: AtomicUsize,
    bytes: AtomicUsize,
    cap: AtomicBool,
    slots: [Slot; 2],
}
#[derive(Clone)]
pub struct Budget(Arc<State>);
#[derive(Clone)]
pub struct Cancellation(Arc<State>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        for s in &self.0.slots {
            if s.active.load(Ordering::Acquire) {
                if let Some(h) = s.handle.get() {
                    h.interrupt();
                }
            }
        }
    }
}
impl Budget {
    pub fn new(deadline: Instant, vm_limit: u32) -> Result<Self, Error> {
        let now = Instant::now();
        if deadline <= now
            || deadline.duration_since(now) > Duration::from_secs(2)
            || !(1000..=2_000_000).contains(&vm_limit)
        {
            return Err(Error::new(Fault::Cap));
        }
        Ok(Self(Arc::new(State {
            deadline,
            cancelled: AtomicBool::new(false),
            ticks: AtomicUsize::new(0),
            tick_limit: (vm_limit / 1000) as usize,
            rows: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            cap: AtomicBool::new(false),
            slots: std::array::from_fn(|_| Slot {
                handle: OnceLock::new(),
                active: AtomicBool::new(false),
            }),
        })))
    }
    pub fn cancellation(&self) -> Cancellation {
        Cancellation(self.0.clone())
    }
    pub fn check(&self) -> Result<(), Error> {
        if self.0.cancelled.load(Ordering::Acquire) {
            Err(Error::new(Fault::Cancelled))
        } else if Instant::now() >= self.0.deadline {
            Err(Error::new(Fault::Deadline))
        } else if self.0.cap.load(Ordering::Acquire)
            || self.0.ticks.load(Ordering::Acquire) >= self.0.tick_limit
        {
            Err(Error::new(Fault::Cap))
        } else {
            Ok(())
        }
    }
    fn fail_cap(&self) -> StoreError {
        self.0.cap.store(true, Ordering::Release);
        StoreError::NotReady("bounded row restriction".into())
    }
    pub(super) fn store_check(&self) -> Result<(), StoreError> {
        self.check()
            .map_err(|_| StoreError::NotReady("bounded request unavailable".into()))
    }
    pub(super) fn row_check(&self, row: &Row<'_>) -> Result<(), StoreError> {
        self.store_check()?;
        let count = row.as_ref().column_count();
        if count > 64 {
            return Err(self.fail_cap());
        }
        let mut total = 0usize;
        for i in 0..count {
            let name = row.as_ref().column_name(i)?;
            if name.len() > 128 {
                return Err(self.fail_cap());
            }
            let cap = match name {
                "id" | "user_id" | "grant_id" => 1024,
                "id_hash" | "csrf_hash" | "token_hash" => 256,
                _ => 4096,
            };
            let n = match row.get_ref(i)? {
                ValueRef::Text(b) | ValueRef::Blob(b) => b.len(),
                _ => 8,
            };
            if n > cap {
                return Err(self.fail_cap());
            }
            total = total.checked_add(n).ok_or_else(|| self.fail_cap())?;
        }
        // Refuse a single oversized row before counters or the mapper can advance.
        if total > 65536 {
            return Err(self.fail_cap());
        }
        let rows = self.0.rows.load(Ordering::Acquire);
        let bytes = self.0.bytes.load(Ordering::Acquire);
        let next_rows = rows.checked_add(1).filter(|n| *n <= 64);
        let next_bytes = bytes.checked_add(total).filter(|n| *n <= 65536);
        let (Some(_), Some(_)) = (next_rows, next_bytes) else {
            return Err(self.fail_cap());
        };
        self.0
            .bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(total).filter(|n| *n <= 65536)
            })
            .map_err(|_| self.fail_cap())?;
        self.0
            .rows
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1).filter(|n| *n <= 64)
            })
            .map_err(|_| self.fail_cap())?;
        Ok(())
    }
    fn result<T>(&self, r: rusqlite::Result<T>) -> Result<T, Error> {
        r.map_err(|e| {
            self.check().err().unwrap_or_else(|| {
                Error::new(
                    if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy) {
                        Fault::Busy
                    } else {
                        Fault::Unavailable
                    },
                )
            })
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
}
fn identity(fd: &impl std::os::fd::AsFd) -> Result<Identity, Error> {
    let s = rustix::fs::fstat(fd).map_err(|_| Error::new(Fault::Unavailable))?;
    Ok(Identity {
        dev: s.st_dev as u64,
        ino: s.st_ino as u64,
    })
}
fn directory(path: &Path) -> Result<OwnedFd, Error> {
    rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| Error::new(Fault::Unavailable))
}
fn regular_identity(root: &OwnedFd, name: &str) -> Result<Identity, Error> {
    // Never open/close a raw database descriptor: POSIX close could release another
    // SQLite connection's process locks. Stat only; safe path opening is not FD anchoring.
    let s = rustix::fs::statat(root, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| Error::new(Fault::Unavailable))?;
    if rustix::fs::FileType::from_raw_mode(s.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(Error::new(Fault::Unavailable));
    }
    Ok(Identity {
        dev: s.st_dev as u64,
        ino: s.st_ino as u64,
    })
}
/// Retained identity checks; safe SQLite paths still require stable operator-controlled parents.
pub struct QualifiedRoot {
    path: PathBuf,
    root: OwnedFd,
    root_id: Identity,
    db_id: Identity,
}
impl QualifiedRoot {
    pub fn open(path: &Path, budget: &Budget) -> Result<Self, Error> {
        budget.check()?;
        let root = directory(path)?;
        let root_id = identity(&root)?;
        let db_id = regular_identity(&root, "cowork.db")?;
        let this = Self {
            path: std::fs::canonicalize(path).map_err(|_| Error::new(Fault::Unavailable))?,
            root,
            root_id,
            db_id,
        };
        this.check(budget)?;
        Ok(this)
    }
    fn check(&self, budget: &Budget) -> Result<(), Error> {
        budget.check()?;
        let current = directory(&self.path)?;
        if identity(&current)? != self.root_id
            || identity(&self.root)? != self.root_id
            || regular_identity(&current, "cowork.db")? != self.db_id
        {
            return Err(Error::new(Fault::Unavailable));
        }
        for n in ["cowork.db-wal", "cowork.db-shm"] {
            regular_identity(&current, n)?;
        }
        match rustix::fs::statat(
            &current,
            "cowork.db-journal",
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Err(rustix::io::Errno::NOENT) => {}
            _ => return Err(Error::new(Fault::Unavailable)),
        }
        budget.check()
    }
}
struct Dedicated {
    conn: Option<Connection>,
    budget: Budget,
    slot: usize,
}
impl Dedicated {
    fn open(root: &QualifiedRoot, budget: &Budget, writer: bool) -> Result<Self, Error> {
        root.check(budget)?;
        let slot = usize::from(!writer);
        let s = budget
            .0
            .slots
            .get(slot)
            .ok_or_else(|| Error::new(Fault::Cap))?;
        if s.handle.get().is_some() {
            return Err(Error::new(Fault::Cap));
        }
        let flags = if writer {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let c = budget.result(Connection::open_with_flags(
            root.path.join("cowork.db"),
            flags | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        ))?;
        // Every publication is immutable and belongs to this connection/job only.
        if s.handle.set(c.get_interrupt_handle()).is_err() {
            return close_error(c, Fault::Cap);
        }
        s.active.store(true, Ordering::Release);
        let mut this = Self {
            conn: Some(c),
            budget: budget.clone(),
            slot,
        };
        let result = this.configure(writer).and_then(|_| root.check(budget));
        if let Err(e) = result {
            return this.fail_close(e.fault);
        }
        Ok(this)
    }
    fn connection(&self) -> Result<&Connection, Error> {
        self.conn
            .as_ref()
            .ok_or_else(|| Error::new(Fault::Unavailable))
    }
    fn configure(&mut self, writer: bool) -> Result<(), Error> {
        self.budget.check()?;
        let c = self.connection()?;
        let b = &self.budget;
        b.result(c.busy_timeout(Duration::ZERO))?;
        let progress = b.clone();
        b.result(c.progress_handler(
            1000,
            Some(move || {
                progress.0.ticks.fetch_add(1, Ordering::AcqRel);
                progress.check().is_err()
            }),
        ))?;
        for (l, n) in [
            (Limit::SQLITE_LIMIT_LENGTH, 1048576),
            (Limit::SQLITE_LIMIT_SQL_LENGTH, 65536),
            (Limit::SQLITE_LIMIT_COLUMN, 64),
            (Limit::SQLITE_LIMIT_EXPR_DEPTH, 64),
            (Limit::SQLITE_LIMIT_VDBE_OP, 100000),
            (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 16),
            (Limit::SQLITE_LIMIT_ATTACHED, 0),
            (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
        ] {
            b.result(c.set_limit(l, n))?;
            if b.result(c.limit(l))? > n {
                return Err(Error::new(Fault::Cap));
            }
        }
        if !b.result(c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true))?
            || b.result(c.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false))?
        {
            return Err(Error::new(Fault::Unavailable));
        }
        b.result(c.execute_batch(
            "PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=MEMORY;",
        ))?;
        b.result(c.pragma_update(
            None,
            if writer { "foreign_keys" } else { "query_only" },
            true,
        ))?;
        for (name, expected) in [
            ("cache_size", -1024),
            ("mmap_size", 0),
            ("temp_store", 2),
            (if writer { "foreign_keys" } else { "query_only" }, 1),
        ] {
            if b.result(c.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0)))?
                != expected
            {
                return Err(Error::new(Fault::Unavailable));
            }
        }
        b.result(c.authorizer(Some(move |ctx: AuthContext<'_>| authorize(ctx, writer))))?;
        b.check()
    }
    fn fail_close<T>(self, fault: Fault) -> Result<T, Error> {
        match self.close() {
            Ok(()) => Err(Error::new(fault)),
            Err(mut e) => {
                e.fault = fault;
                Err(e)
            }
        }
    }
    fn close(mut self) -> Result<(), Error> {
        if let Some(s) = self.budget.0.slots.get(self.slot) {
            s.active.store(false, Ordering::Release);
        }
        let c = self
            .conn
            .take()
            .ok_or_else(|| Error::new(Fault::Unavailable))?;
        // Disposable connection only: cleanup must run even after terminal cancellation.
        let _ = c.progress_handler(0, None::<fn() -> bool>);
        match c.close() {
            Ok(()) => Ok(()),
            Err((c, _)) => Err(Error {
                fault: Fault::Unavailable,
                pending: Some(Box::new(c)),
            }),
        }
    }
}
fn close_error<T>(c: Connection, fault: Fault) -> Result<T, Error> {
    match c.close() {
        Ok(()) => Err(Error::new(fault)),
        Err((c, _)) => Err(Error {
            fault,
            pending: Some(Box::new(c)),
        }),
    }
}
fn authorize(ctx: AuthContext<'_>, writer: bool) -> Authorization {
    if ctx.accessor.is_some() || ctx.database_name.is_some_and(|n| n != "main") {
        return Authorization::Deny;
    }
    let allow = match ctx.action {
        AuthAction::Select | AuthAction::Transaction { .. } => true,
        AuthAction::Read { table_name, .. } => {
            REQUIRED_COLUMNS.iter().any(|(t, _)| *t == table_name)
                || matches!(table_name, "sqlite_master" | "sqlite_schema")
        }
        AuthAction::Function { function_name } => matches!(
            function_name,
            "max" | "count" | "length" | "typeof" | "substr"
        ),
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => match pragma_name {
            "journal_mode" | "query_only" | "foreign_keys" | "schema_version" => {
                pragma_value.is_none()
            }
            "table_info" | "table_xinfo" | "index_list" | "index_xinfo" | "foreign_key_list" => {
                pragma_value.is_some()
            }
            _ => false,
        },
        AuthAction::Update {
            table_name,
            column_name,
        } => {
            writer
                && matches!(
                    (table_name, column_name),
                    ("sessions", "last_seen_at") | ("device_grants", "last_used_at")
                )
        }
        AuthAction::Delete { table_name } => writer && table_name == "sessions",
        _ => false,
    };
    if allow {
        Authorization::Allow
    } else {
        Authorization::Deny
    }
}
fn value_text<'a>(row: &'a Row<'_>, i: usize, cap: usize) -> Result<&'a str, Error> {
    match row.get_ref(i).map_err(|_| Error::new(Fault::Unavailable))? {
        ValueRef::Text(v) if v.len() <= cap => {
            std::str::from_utf8(v).map_err(|_| Error::new(Fault::Unavailable))
        }
        _ => Err(Error::new(Fault::Cap)),
    }
}
fn metadata_add(metadata: &mut usize, n: usize) -> Result<(), Error> {
    *metadata = metadata
        .checked_add(n)
        .filter(|n| *n <= 1048576)
        .ok_or_else(|| Error::new(Fault::Cap))?;
    Ok(())
}
fn verify_fk_topology(c: &Connection, b: &Budget, metadata: &mut usize) -> Result<(), Error> {
    let mut statement = b.result(c.prepare("SELECT name,length(CAST(sql AS BLOB)) FROM sqlite_schema WHERE type='table' AND substr(name,1,7)!='sqlite_' LIMIT 65"))?;
    let mut rows = b.result(statement.query([]))?;
    let mut tables = Vec::new();
    while let Some(row) = b.result(rows.next())? {
        b.check()?;
        if tables.len() >= 64 {
            return Err(Error::new(Fault::Cap));
        }
        let name = value_text(row, 0, 128)?;
        let length = b.result(row.get::<_, i64>(1))?;
        if !(0..=65536).contains(&length) {
            return Err(Error::new(Fault::Cap));
        }
        metadata_add(metadata, name.len() + length as usize)?;
        tables.push(name.to_owned());
    }
    let mut total = 0usize;
    for table in tables {
        b.check()?;
        let mut s = b.result(c.prepare(&format!(
            "PRAGMA foreign_key_list('{}')",
            table.replace('\'', "''")
        )))?;
        let mut rows = b.result(s.query([]))?;
        let mut count = 0;
        while let Some(row) = b.result(rows.next())? {
            b.check()?;
            count += 1;
            total += 1;
            if count > 64 || total > 256 {
                return Err(Error::new(Fault::Cap));
            }
            let id = b.result(row.get::<_, i64>(0))?;
            let seq = b.result(row.get::<_, i64>(1))?;
            let target = value_text(row, 2, 128)?;
            let from = value_text(row, 3, 128)?;
            let to = match b.result(row.get_ref(4))? {
                ValueRef::Null => None,
                ValueRef::Text(bytes) if bytes.len() <= 128 => {
                    Some(std::str::from_utf8(bytes).map_err(|_| Error::new(Fault::Unavailable))?)
                }
                _ => return Err(Error::new(Fault::Cap)),
            };
            let update = value_text(row, 5, 16)?;
            let delete = value_text(row, 6, 16)?;
            let matching = value_text(row, 7, 16)?;
            metadata_add(
                metadata,
                16 + target.len()
                    + from.len()
                    + to.map_or(0, str::len)
                    + update.len()
                    + delete.len()
                    + matching.len(),
            )?;
            let exact = |parent: &str, child: &str| {
                id == 0
                    && seq == 0
                    && target.eq_ignore_ascii_case(parent)
                    && from.eq_ignore_ascii_case(child)
                    && to.is_some_and(|v| v.eq_ignore_ascii_case("id"))
                    && update == "NO ACTION"
                    && delete == "CASCADE"
                    && matching == "NONE"
            };
            let token_edge =
                table.eq_ignore_ascii_case("device_tokens") && exact("device_grants", "grant_id");
            if target.eq_ignore_ascii_case("sessions")
                || (target.eq_ignore_ascii_case("device_grants") && !token_edge)
                || ((table.eq_ignore_ascii_case("sessions")
                    || table.eq_ignore_ascii_case("device_grants"))
                    && !exact("users", "user_id"))
                || (table.eq_ignore_ascii_case("device_tokens") && !token_edge)
            {
                return Err(Error::new(Fault::Unavailable));
            }
        }
        if ["sessions", "device_grants", "device_tokens"]
            .iter()
            .any(|v| table.eq_ignore_ascii_case(v))
            && count != 1
        {
            return Err(Error::new(Fault::Unavailable));
        }
    }
    b.check()
}
fn verify(c: &Connection, b: &Budget) -> Result<(), Error> {
    b.check()?;
    if b.result(c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)))? != "wal" {
        return Err(Error::new(Fault::Unavailable));
    }
    let version = b.result(
        c.query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get::<_, Option<i64>>(0)
        }),
    )?;
    if version.is_some_and(|v| v > KNOWN_SCHEMA_VERSION) {
        return Err(Error::new(Fault::SchemaTooNew));
    }
    if version != Some(KNOWN_SCHEMA_VERSION) {
        return Err(Error::new(Fault::Unavailable));
    }
    let mut metadata = 0usize;
    for (table, required) in REQUIRED_COLUMNS {
        let mut stmt=b.result(c.prepare("SELECT type,length(CAST(sql AS BLOB)),sql FROM sqlite_schema WHERE tbl_name=?1 LIMIT 257"))?;
        let mut rows = b.result(stmt.query([table]))?;
        let mut n = 0;
        let mut found = false;
        while let Some(r) = b.result(rows.next())? {
            b.check()?;
            n += 1;
            if n > 256 {
                return Err(Error::new(Fault::Cap));
            }
            let kind = value_text(r, 0, 32)?;
            let len = b.result(r.get::<_, Option<i64>>(1))?.unwrap_or(0);
            if !(0..=65536).contains(&len) {
                return Err(Error::new(Fault::Cap));
            }
            metadata = metadata
                .checked_add(len as usize)
                .ok_or_else(|| Error::new(Fault::Cap))?;
            if metadata > 1048576 {
                return Err(Error::new(Fault::Cap));
            }
            if kind == "table" {
                if found
                    || !value_text(r, 2, 65536)?
                        .to_ascii_lowercase()
                        .starts_with("create table ")
                    || value_text(r, 2, 65536)?
                        .to_ascii_lowercase()
                        .contains("collate")
                {
                    return Err(Error::new(Fault::Unavailable));
                }
                found = true;
            } else if kind != "index" {
                return Err(Error::new(Fault::Unavailable));
            }
        }
        if !found {
            return Err(Error::new(Fault::Unavailable));
        }
        let mut stmt = b.result(c.prepare(&format!("PRAGMA table_xinfo('{table}')")))?;
        let mut rows = b.result(stmt.query([]))?;
        let mut have = Vec::new();
        while let Some(r) = b.result(rows.next())? {
            b.check()?;
            if have.len() >= 64 {
                return Err(Error::new(Fault::Cap));
            }
            if b.result(r.get::<_, i64>(6))? != 0 {
                return Err(Error::new(Fault::Unavailable));
            }
            let name = value_text(r, 1, 128)?;
            metadata = metadata
                .checked_add(name.len() + 32)
                .ok_or_else(|| Error::new(Fault::Cap))?;
            if metadata > 1048576 {
                return Err(Error::new(Fault::Cap));
            }
            have.push(name.to_owned());
        }
        if required.iter().any(|name| !have.iter().any(|v| v == name)) {
            return Err(Error::new(Fault::Unavailable));
        }
    }
    verify_fk_topology(c, b, &mut metadata)?;
    for (table, key) in [
        ("sessions", "id_hash"),
        ("users", "id"),
        ("user_features", "user_id"),
        ("settings", "key"),
        ("device_tokens", "token_hash"),
        ("device_grants", "id"),
    ] {
        let mut s = b.result(c.prepare(&format!("PRAGMA index_list('{table}')")))?;
        let mut rows = b.result(s.query([]))?;
        let mut count = 0;
        let mut valid = false;
        while let Some(r) = b.result(rows.next())? {
            b.check()?;
            count += 1;
            if count > 64 {
                return Err(Error::new(Fault::Cap));
            }
            if value_text(r, 3, 16)? != "pk"
                || b.result(r.get::<_, i64>(2))? != 1
                || b.result(r.get::<_, i64>(4))? != 0
            {
                continue;
            }
            let name = value_text(r, 1, 128)?;
            metadata = metadata
                .checked_add(name.len() + 32)
                .ok_or_else(|| Error::new(Fault::Cap))?;
            if metadata > 1048576 {
                return Err(Error::new(Fault::Cap));
            }
            let mut is = b.result(c.prepare(&format!(
                "PRAGMA index_xinfo('{}')",
                name.replace('\'', "''")
            )))?;
            let mut ir = b.result(is.query([]))?;
            let mut keys = 0;
            let mut good = true;
            let mut entries = 0;
            while let Some(i) = b.result(ir.next())? {
                entries += 1;
                metadata = metadata
                    .checked_add(288)
                    .ok_or_else(|| Error::new(Fault::Cap))?;
                if metadata > 1048576 {
                    return Err(Error::new(Fault::Cap));
                }
                if entries > 64 {
                    return Err(Error::new(Fault::Cap));
                }
                if b.result(i.get::<_, i64>(5))? == 1 {
                    keys += 1;
                    good &= value_text(i, 2, 128)? == key && value_text(i, 4, 128)? == "BINARY";
                }
            }
            valid |= keys == 1 && good;
        }
        if !valid {
            return Err(Error::new(Fault::Unavailable));
        }
        let mut s = b.result(c.prepare(&format!(
            "SELECT 1 FROM {table} WHERE {key} IS NULL LIMIT 1"
        )))?;
        let mut rows = b.result(s.query([]))?;
        if b.result(rows.next())?.is_some() {
            return Err(Error::new(Fault::Unavailable));
        }
    }
    b.check()
}
/// Not shared/reused across jobs. Explicit close reports retained close errors.
#[must_use]
pub struct BoundedStore(Dedicated);
impl BoundedStore {
    pub fn open(root: &QualifiedRoot, b: &Budget) -> Result<Self, Error> {
        Dedicated::open(root, b, false).map(Self)
    }
    pub fn read<T>(
        &mut self,
        f: impl FnOnce(&Reader<'_>) -> Result<T, StoreError>,
    ) -> Result<T, Error> {
        let b = self.0.budget.clone();
        b.check()?;
        let c = self
            .0
            .conn
            .as_mut()
            .ok_or_else(|| Error::new(Fault::Unavailable))?;
        let tx = b.result(c.transaction_with_behavior(TransactionBehavior::Deferred))?;
        verify(&tx, &b)?;
        let out = f(&Reader {
            conn: &tx,
            budget: Some(&b),
        })
        .map_err(|_| {
            b.check()
                .err()
                .unwrap_or_else(|| Error::new(Fault::Unavailable))
        });
        let finish = b.result(tx.finish());
        b.check()?;
        finish?;
        out
    }
    pub fn close(self) -> Result<(), Error> {
        self.0.close()
    }
}
/// Existing RustAuth ownership, narrowed to session upkeep columns/deletion only.
#[must_use]
pub struct BoundedUpkeepWriter(Dedicated);
impl BoundedUpkeepWriter {
    pub fn open(root: &QualifiedRoot, _auth: RustAuth, b: &Budget) -> Result<Self, Error> {
        Dedicated::open(root, b, true).map(Self)
    }
    pub fn write<T>(
        &mut self,
        f: impl FnOnce(&WriteTx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, Error> {
        let b = self.0.budget.clone();
        b.check()?;
        let c = self
            .0
            .conn
            .as_mut()
            .ok_or_else(|| Error::new(Fault::Unavailable))?;
        let tx = b.result(c.transaction_with_behavior(TransactionBehavior::Immediate))?;
        verify(&tx, &b)?;
        let window = AtomicBool::new(false);
        let out = f(&WriteTx {
            conn: &tx,
            settings_open: &window,
            budget: Some(&b),
        })
        .map_err(|_| {
            b.check()
                .err()
                .unwrap_or_else(|| Error::new(Fault::Unavailable))
        });
        match out {
            Ok(v) => {
                b.check()?;
                b.result(tx.commit())?;
                b.check()?;
                Ok(v)
            }
            Err(e) => {
                let _ = tx.rollback();
                Err(e)
            }
        }
    }
    pub fn close(self) -> Result<(), Error> {
        self.0.close()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn owned_close_error_remains_explicit() -> Result<(), super::Error> {
        let c = super::Connection::open_in_memory()
            .map_err(|_| super::Error::new(super::Fault::Unavailable))?;
        // Synthetic representation control; not a claim sqlite3_close failure was provoked.
        let e = super::Error {
            fault: super::Fault::Unavailable,
            pending: Some(Box::new(c)),
        };
        assert!(e.has_pending_close(), "ROUTING_ASSERT/store/owned_close");
        e.retry_close()
    }
}
