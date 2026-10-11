//! Dedicated synthetic-qualified read service. Shared HTTP/auth integration remains pending.
//! Ordinary SQLite paths require trusted stable parents; Root is not an FD-anchored VFS.
use crate::{feature_env, Control, Error, Root, Service, ROW_BYTES};
use rusqlite::{
    config::DbConfig,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::ValueRef,
    Connection, OpenFlags,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
const OWNER_BYTES: usize = 1024;
const GLOBAL: usize = 8;
const SQL_CAP: i64 = 65536;
static ADMISSION: Mutex<Admission> = Mutex::new(Admission {
    count: 0,
    owners: Vec::new(),
});
struct Admission {
    count: usize,
    owners: Vec<String>,
}
struct GlobalLease {
    owner: Option<String>,
}
impl GlobalLease {
    fn acquire(owner: Option<&str>) -> Result<Self, Error> {
        let mut a = lock(&ADMISSION)?;
        if a.count >= GLOBAL || owner.is_some_and(|o| a.owners.iter().any(|s| s == o)) {
            return Err(Error::Unavailable);
        }
        let owner = owner.map(str::to_owned);
        if let Some(o) = &owner {
            a.owners.push(o.clone());
        }
        a.count += 1;
        Ok(Self { owner })
    }
}
impl Drop for GlobalLease {
    fn drop(&mut self) {
        if let Ok(mut a) = ADMISSION.lock() {
            a.count = a.count.saturating_sub(1);
            if let Some(o) = &self.owner {
                a.owners.retain(|s| s != o);
            }
        }
    }
}
type Answer = Result<(u16, String), Error>;
fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, Error> {
    m.lock().map_err(|_| Error::Unavailable)
}
fn sql<T>(r: rusqlite::Result<T>) -> Result<T, Error> {
    r.map_err(|_| Error::Unavailable)
}
fn text(row: &rusqlite::Row<'_>, column: usize, cap: usize) -> Result<String, Error> {
    match sql(row.get_ref(column))? {
        ValueRef::Text(b) if b.len() <= cap => std::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| Error::Unavailable),
        _ => Err(Error::Unavailable),
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
}
fn identity(fd: &impl std::os::fd::AsFd) -> Result<Identity, Error> {
    let s = rustix::fs::fstat(fd).map_err(|_| Error::Unavailable)?;
    Ok(Identity {
        dev: s.st_dev as u64,
        ino: s.st_ino as u64,
    })
}
struct Database {
    root: Root,
    path: PathBuf,
    root_id: Identity,
    db_id: Identity,
}
impl Database {
    fn new(path: &Path) -> Result<Self, Error> {
        let root = Root::open(path)?;
        let root_id = identity(&root.fd)?;
        let db_id = identity(&root.regular("cowork.db")?.ok_or(Error::Unavailable)?)?;
        let this = Self {
            root,
            path: path.to_owned(),
            root_id,
            db_id,
        };
        this.check()?;
        Ok(this)
    }
    fn check(&self) -> Result<(), Error> {
        let current = Root::open(&self.path)?;
        if identity(&current.fd)? != self.root_id || identity(&self.root.fd)? != self.root_id {
            return Err(Error::Unavailable);
        }
        if identity(&current.regular("cowork.db")?.ok_or(Error::Unavailable)?)? != self.db_id {
            return Err(Error::Unavailable);
        }
        // Required Node-created companions: no repair/creation by this service.
        for name in ["cowork.db-wal", "cowork.db-shm"] {
            current.regular(name)?.ok_or(Error::Unavailable)?;
        }
        if current.regular("cowork.db-journal")?.is_some() {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
}
struct Active {
    owners: HashSet<String>,
    count: usize,
    closed: bool,
    jobs: Vec<Arc<Task>>,
}
struct Inner {
    db: Database,
    policy: Service,
    product_override: Option<bool>,
    active: Mutex<Active>,
}
struct Task {
    control: Control,
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
    handle: Mutex<Option<JoinHandle<Answer>>>,
    result: Mutex<Option<Answer>>,
    lease: Mutex<Option<Lease>>,
    finished: AtomicBool,
    joined: AtomicBool,
    #[cfg(test)]
    join_started: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
}
impl Task {
    fn cancel(&self) {
        self.control.cancel();
        if let Ok(i) = self.interrupt.lock() {
            if let Some(i) = i.as_ref() {
                i.interrupt();
            }
        }
    }
    fn join(&self) -> Answer {
        let mut h = lock(&self.handle)?;
        if let Some(handle) = h.take() {
            #[cfg(test)]
            if let Some(tx) = lock(&self.join_started)?.take() {
                let _ = tx.send(());
            }
            let result = handle.join().unwrap_or(Err(Error::Unavailable));
            *lock(&self.result)? = Some(result);
            // Only join confirms worker completion, including panic/error/close.
            drop(lock(&self.lease)?.take());
            self.joined.store(true, Ordering::Release);
        }
        lock(&self.result)?
            .as_ref()
            .cloned()
            .ok_or(Error::Unavailable)?
    }
}
/// Joinable job: dropping it cancels and joins, retaining the lease until confirmed completion.
pub struct StatusJob {
    task: Arc<Task>,
}
/// Independent cancellation signal for a caller whose join is supervised elsewhere.
/// It never owns/releases an admission lease or detaches a worker.
#[derive(Clone)]
pub struct StatusCancellation {
    task: Arc<Task>,
}
impl StatusCancellation {
    pub fn cancel(&self) {
        self.task.cancel();
    }
}
impl StatusJob {
    pub fn cancellation(&self) -> StatusCancellation {
        StatusCancellation {
            task: self.task.clone(),
        }
    }
    pub fn cancel(&self) {
        self.task.cancel();
    }
    pub fn finish(self) -> Answer {
        self.task.join()
    }
}
impl Drop for StatusJob {
    fn drop(&mut self) {
        if !self.task.joined.load(Ordering::Acquire) {
            self.task.cancel();
            let _ = self.task.join();
        }
    }
}
struct Lease {
    inner: std::sync::Weak<Inner>,
    owner: String,
    _global: GlobalLease,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            if let Ok(mut a) = inner.active.lock() {
                a.count = a.count.saturating_sub(1);
                a.owners.remove(&self.owner);
            }
        }
    }
}
/// Keys snapshot once at startup. Only already-authenticated owner IDs are accepted.
/// Shared Store/auth deadline and Node feature refresh seams are deliberately not present.
pub struct SqliteService {
    inner: Arc<Inner>,
}
impl SqliteService {
    pub fn open(path: &Path, previous: Option<&str>, env: Option<&str>) -> Result<Self, Error> {
        // Startup is synchronous: one global startup lease before keys/open and held to return.
        // Global admission and owner leases also span simultaneous service instances.
        let _startup_lease = GlobalLease::acquire(None)?;
        let product_override = feature_env(env)?;
        let db = Database::new(path)?;
        let policy = Service::new(&db.root, previous)?;
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                policy,
                product_override,
                active: Mutex::new(Active {
                    owners: HashSet::new(),
                    count: 0,
                    closed: false,
                    jobs: Vec::new(),
                }),
            }),
        })
    }
    /// Caller supplies arrival time, so pre-admission time cannot silently reset the budget.
    pub fn submit(&self, owner: &str, arrival: Instant) -> Result<StatusJob, Error> {
        if owner.is_empty()
            || owner.len() > OWNER_BYTES
            || arrival > Instant::now()
            || Instant::now().saturating_duration_since(arrival) >= Duration::from_secs(2)
        {
            return Err(Error::Unavailable);
        }
        // Reap only after joining outside the admission mutex (join releases the lease).
        let finished: Vec<_> = {
            let a = lock(&self.inner.active)?;
            a.jobs
                .iter()
                .filter(|t| t.finished.load(Ordering::Acquire))
                .cloned()
                .collect()
        };
        for t in finished {
            let _ = t.join();
        }
        if Instant::now().saturating_duration_since(arrival) >= Duration::from_secs(2) {
            return Err(Error::Unavailable);
        }
        let global = GlobalLease::acquire(Some(owner))?;
        let task = Arc::new(Task {
            control: Control::default(),
            interrupt: Mutex::new(None),
            handle: Mutex::new(None),
            result: Mutex::new(None),
            lease: Mutex::new(None),
            finished: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            #[cfg(test)]
            join_started: Mutex::new(None),
        });
        let mut h = lock(&task.handle)?;
        let mut a = lock(&self.inner.active)?;
        a.jobs.retain(|t| !t.joined.load(Ordering::Acquire));
        if a.closed || a.count >= GLOBAL || a.owners.contains(owner) {
            return Err(Error::Unavailable);
        }
        let owner = owner.to_owned();
        a.owners.insert(owner.clone());
        a.count += 1;
        *lock(&task.lease)? = Some(Lease {
            inner: Arc::downgrade(&self.inner),
            owner: owner.clone(),
            _global: global,
        });
        a.jobs.push(task.clone());
        drop(a);
        let inner = self.inner.clone();
        let worker_task = task.clone();
        match thread::Builder::new()
            .name("chatgpt-status".into())
            .spawn(move || {
                let result = read(&inner, &worker_task, &owner);
                worker_task.finished.store(true, Ordering::Release);
                result
            }) {
            Ok(handle) => *h = Some(handle),
            Err(_) => {
                *lock(&task.result)? = Some(Err(Error::Unavailable));
                drop(lock(&task.lease)?.take());
                task.finished.store(true, Ordering::Release);
                task.joined.store(true, Ordering::Release);
                return Err(Error::Unavailable);
            }
        }
        drop(h);
        Ok(StatusJob { task })
    }

    /// No detached workers. If regular-file I/O stalls, shutdown continues retaining leases.
    pub fn shutdown(&self) -> Result<(), Error> {
        let jobs = {
            let mut a = lock(&self.inner.active)?;
            a.closed = true;
            std::mem::take(&mut a.jobs)
        };
        for t in &jobs {
            t.cancel();
        }
        for t in jobs {
            let _ = t.join();
        }
        Ok(())
    }
}
impl Drop for SqliteService {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Base,
    OAuth,
    Status,
}
fn authorization(ctx: AuthContext<'_>, phase: Phase) -> Authorization {
    let metadata = !matches!(phase, Phase::Status);
    if ctx.accessor.is_some() || ctx.database_name.is_some_and(|d| d != "main") {
        return Authorization::Deny;
    }
    let allowed = match ctx.action {
        AuthAction::Select | AuthAction::Transaction { .. } => true,
        AuthAction::Read {
            table_name,
            column_name,
        } => match table_name {
            "settings" => !matches!(phase, Phase::Status) && matches!(column_name, "key" | "value"),
            "chatgpt_oauth_tokens" => {
                !matches!(phase, Phase::Base)
                    && matches!(column_name, "user_id" | "state" | "data_enc")
            }
            "schema_migrations" => metadata && matches!(column_name, "version" | ""),
            "sqlite_master" | "sqlite_schema" => {
                metadata && matches!(column_name, "type" | "name" | "tbl_name" | "sql")
            }
            _ => false,
        },
        AuthAction::Function { function_name } => {
            matches!(function_name, "typeof" | "length")
                || metadata && matches!(function_name, "max" | "substr")
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => {
            metadata
                && match pragma_name {
                    "schema_version" | "journal_mode" | "query_only" => pragma_value.is_none(),
                    "table_xinfo" | "index_list" | "index_xinfo" => pragma_value.is_some(),
                    _ => false,
                }
        }
        _ => false,
    };
    if allowed {
        Authorization::Allow
    } else {
        Authorization::Deny
    }
}
fn configure(c: &Connection, control: &Control) -> Result<(), Error> {
    configure_ticks(c, control, 2000)
}
fn configure_ticks(c: &Connection, control: &Control, tick_limit: usize) -> Result<(), Error> {
    sql(c.busy_timeout(Duration::ZERO))?;
    for (limit, n) in [
        (Limit::SQLITE_LIMIT_LENGTH, 1048576),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, 65536),
        (Limit::SQLITE_LIMIT_COLUMN, 64),
        (Limit::SQLITE_LIMIT_EXPR_DEPTH, 64),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, 4),
        (Limit::SQLITE_LIMIT_VDBE_OP, 100000),
        (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 16),
        (Limit::SQLITE_LIMIT_FUNCTION_ARG, 8),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
        (Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 0),
        (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
    ] {
        sql(c.set_limit(limit, n))?;
        if sql(c.limit(limit))? > n {
            return Err(Error::Unavailable);
        }
    }
    if !sql(c.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true))? {
        return Err(Error::Unavailable);
    }
    if sql(c.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false))? {
        return Err(Error::Unavailable);
    }
    let control = control.clone();
    let mut ticks = 0usize;
    sql(c.progress_handler(
        1000,
        Some(move || {
            ticks += 1;
            ticks >= tick_limit || control.check().is_err()
        }),
    ))?;
    sql(c.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=MEMORY;"))?;
    for (name, expected) in [
        ("query_only", 1),
        ("cache_size", -1024),
        ("mmap_size", 0),
        ("temp_store", 2),
    ] {
        if sql(c.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0)))? != expected {
            return Err(Error::Unavailable);
        }
    }
    sql(c.authorizer(Some(|ctx: AuthContext<'_>| authorization(ctx, Phase::Base))))?;
    Ok(())
}
fn read(inner: &Inner, task: &Task, owner: &str) -> Answer {
    task.control.check()?;
    inner.db.check()?;
    let c = sql(Connection::open_with_flags(
        inner.db.path.join("cowork.db"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ))?;
    *lock(&task.interrupt)? = Some(c.get_interrupt_handle());
    configure(&c, &task.control)?;
    task.control.check()?;
    inner.db.check()?;
    sql(c.execute_batch("BEGIN DEFERRED"))?;
    schema(&c, &task.control, false)?;
    let product = product(&c, inner.product_override)?;
    let answer = if !product {
        inner
            .policy
            .read_with(&task.control, false, owner, || Err(Error::Unavailable))?
    } else {
        sql(c.authorizer(Some(|ctx: AuthContext<'_>| {
            authorization(ctx, Phase::OAuth)
        })))?;
        schema(&c, &task.control, true)?;
        sql(c.authorizer(Some(|ctx: AuthContext<'_>| {
            authorization(ctx, Phase::Status)
        })))?;
        inner
            .policy
            .read_with(&task.control, true, owner, || row(&c, owner))?
    };
    task.control.check()?;
    sql(c.execute_batch("ROLLBACK"))?;
    inner.db.check()?;
    *lock(&task.interrupt)? = None;
    drop(c);
    Ok(answer)
}
fn row(c: &Connection, owner: &str) -> Result<Option<(Zeroizing<String>, String)>, Error> {
    let mut s = sql(c.prepare("SELECT CASE WHEN typeof(user_id)='text' AND length(CAST(user_id AS BLOB))<=1024 THEN user_id ELSE NULL END, CASE WHEN typeof(state)='text' AND length(CAST(state AS BLOB))<=64 THEN state ELSE NULL END, CASE WHEN typeof(data_enc)='text' AND length(CAST(data_enc AS BLOB))<=262144 THEN data_enc ELSE NULL END FROM chatgpt_oauth_tokens WHERE user_id=?1 COLLATE BINARY"))?;
    let mut rows = sql(s.query([owner]))?;
    let Some(r) = sql(rows.next())? else {
        return Ok(None);
    };
    if text(r, 0, OWNER_BYTES)? != owner {
        return Err(Error::Unavailable);
    }
    let state = text(r, 1, 64)?;
    let enc = Zeroizing::new(text(r, 2, ROW_BYTES)?);
    if sql(rows.next())?.is_some() {
        return Err(Error::Unavailable);
    }
    Ok(Some((enc, state)))
}
fn schema(c: &Connection, control: &Control, oauth: bool) -> Result<(), Error> {
    control.check()?;
    if sql(c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)))? != "wal"
        || sql(c.query_row("PRAGMA query_only", [], |r| r.get::<_, i64>(0)))? != 1
    {
        return Err(Error::Unavailable);
    }
    let _: i64 = sql(c.query_row("PRAGMA schema_version", [], |r| r.get(0)))?;
    // No persistent statement/schema cache: validate each bounded request snapshot.
    let query = if oauth {
        "SELECT type,name,tbl_name,length(CAST(sql AS BLOB)),substr(sql,1,64) FROM sqlite_schema LIMIT 257"
    } else {
        "SELECT type,name,tbl_name,length(CAST(sql AS BLOB)),substr(sql,1,64) FROM sqlite_schema WHERE tbl_name IN ('settings','schema_migrations') LIMIT 257"
    };
    let mut s = sql(c.prepare(query))?;
    let mut rows = sql(s.query([]))?;
    let mut n = 0;
    let mut bytes = 0i64;
    let mut tables = HashSet::new();
    while let Some(r) = sql(rows.next())? {
        control.check()?;
        n += 1;
        if n > 256 {
            return Err(Error::Unavailable);
        }
        let kind = text(r, 0, 256)?;
        let name = text(r, 1, 256)?;
        let table = text(r, 2, 256)?;
        let length = sql(r.get::<_, Option<i64>>(3))?.unwrap_or(0);
        if !(0..=SQL_CAP).contains(&length) {
            return Err(Error::Unavailable);
        }
        bytes += length;
        if bytes > 1048576 {
            return Err(Error::Unavailable);
        }
        if matches!(table.as_str(), "settings" | "schema_migrations")
            || oauth && table == "chatgpt_oauth_tokens"
        {
            if kind == "table" && name == table {
                if !text(r, 4, 256)?
                    .to_ascii_lowercase()
                    .starts_with("create table ")
                {
                    return Err(Error::Unavailable);
                }
                tables.insert(table);
            } else if kind != "index" {
                return Err(Error::Unavailable);
            }
        }
    }
    for name in ["schema_migrations", "settings"] {
        if !tables.contains(name) {
            return Err(Error::Unavailable);
        }
    }
    table(
        c,
        "schema_migrations",
        &[
            ("version", "INTEGER", false, 1),
            ("applied_at", "INTEGER", true, 0),
        ],
        None,
    )?;
    if sql(
        c.query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get::<_, Option<i64>>(0)
        }),
    )? != Some(5)
    {
        return Err(Error::Unavailable);
    }
    table(
        c,
        "settings",
        &[("key", "TEXT", false, 1), ("value", "TEXT", true, 0)],
        Some("key"),
    )?;
    match c.query_row(
        "SELECT key FROM settings WHERE key IS NULL LIMIT 1",
        [],
        |_| Ok(()),
    ) {
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        _ => return Err(Error::Unavailable),
    }
    if oauth {
        if !tables.contains("chatgpt_oauth_tokens") {
            return Err(Error::Unavailable);
        }
        table(
            c,
            "chatgpt_oauth_tokens",
            &[
                ("user_id", "TEXT", false, 1),
                ("data_enc", "TEXT", true, 0),
                ("state", "TEXT", true, 0),
                ("updated_at", "INTEGER", true, 0),
            ],
            Some("user_id"),
        )?;
        // Actual SQLite TEXT PRIMARY KEY allows NULL. Refuse NULL at runtime explicitly.
        match c.query_row(
            "SELECT user_id FROM chatgpt_oauth_tokens WHERE user_id IS NULL LIMIT 1",
            [],
            |_| Ok(()),
        ) {
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            _ => return Err(Error::Unavailable),
        }
    }
    Ok(())
}
fn table(
    c: &Connection,
    name: &str,
    expected: &[(&str, &str, bool, i64)],
    key: Option<&str>,
) -> Result<(), Error> {
    let mut s = sql(c.prepare(&format!("PRAGMA table_xinfo('{name}')")))?;
    let mut rows = sql(s.query([]))?;
    let mut n = 0;
    while let Some(r) = sql(rows.next())? {
        let e = expected.get(n).ok_or(Error::Unavailable)?;
        if text(r, 1, 256)? != e.0
            || text(r, 2, 256)? != e.1
            || sql(r.get::<_, i64>(3))? != i64::from(e.2)
            || sql(r.get::<_, i64>(5))? != e.3
            || sql(r.get::<_, i64>(6))? != 0
        {
            return Err(Error::Unavailable);
        }
        if name == "chatgpt_oauth_tokens" && e.0 == "state" && text(r, 4, 256)? != "'connected'" {
            return Err(Error::Unavailable);
        }
        n += 1;
    }
    if n != expected.len() {
        return Err(Error::Unavailable);
    }
    if let Some(key) = key {
        let mut s = sql(c.prepare(&format!("PRAGMA index_list('{name}')")))?;
        let mut rows = sql(s.query([]))?;
        let mut n = 0;
        let mut pk = false;
        while let Some(r) = sql(rows.next())? {
            n += 1;
            if n > 64 {
                return Err(Error::Unavailable);
            }
            let index = text(r, 1, 256)?;
            let origin = text(r, 3, 256)?;
            if origin == "pk" {
                if pk || sql(r.get::<_, i64>(2))? != 1 || sql(r.get::<_, i64>(4))? != 0 {
                    return Err(Error::Unavailable);
                }
                // Quote SQLite-produced identifier with bounded doubled quotes.
                let mut s = sql(c.prepare(&format!(
                    "PRAGMA index_xinfo('{}')",
                    index.replace('\'', "''")
                )))?;
                let mut ir = sql(s.query([]))?;
                let mut entries = 0;
                let mut keys = 0;
                while let Some(i) = sql(ir.next())? {
                    entries += 1;
                    if entries > 64 {
                        return Err(Error::Unavailable);
                    }
                    if sql(i.get::<_, i64>(5))? == 1 {
                        keys += 1;
                        if text(i, 2, 256)? != key
                            || text(i, 4, 256)? != "BINARY"
                            || sql(i.get::<_, i64>(3))? != 0
                        {
                            return Err(Error::Unavailable);
                        }
                    }
                }
                if keys != 1 {
                    return Err(Error::Unavailable);
                }
                pk = true;
            }
        }
        if !pk {
            return Err(Error::Unavailable);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod focused {
    use super::*;
    #[test]
    fn authorizer_and_finite_vm_cancel() {
        // Real file needed: in-memory SQLite has no mmap_size value.
        let d = tempfile::tempdir().unwrap();
        let c = Connection::open(d.path().join("f.db")).unwrap();
        c.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            .unwrap();
        configure(&c, &Control::default()).unwrap();
        for statement in [
            "UPDATE settings SET value='false'",
            "CREATE TABLE writer(x)",
            "ATTACH ':memory:' AS other",
            "SELECT random()",
            "SELECT * FROM chatgpt_oauth_tokens",
            "PRAGMA user_version=2",
        ] {
            assert!(
                c.prepare(statement).is_err(),
                "ROUTING_ASSERT/sqlite/authorizer {statement}"
            );
        }
        // Test-only arbitrary SQL is never exposed by production API. Disable only authorizer;
        // keep production progress/cancellation/compile limits and query_only.
        c.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let start = Instant::now();
        let result=c.query_row("WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<1000000000) SELECT sum(n) FROM x",[],|r|r.get::<_,i64>(0));
        assert_eq!(
            result.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted),
            "ROUTING_ASSERT/sqlite/vm_budget"
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "ROUTING_ASSERT/sqlite/vm_budget"
        );
        let ctrl = Control::default();
        configure(&c, &ctrl).unwrap();
        ctrl.cancel();
        c.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert!(c.query_row("WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<100000) SELECT sum(n) FROM x",[],|r|r.get::<_,i64>(0)).is_err());
    }
    #[test]
    fn startup_error_reclaims_global() {
        let baseline = ADMISSION.lock().unwrap().count;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().canonicalize().unwrap();
        let c = Connection::open(path.join("cowork.db")).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE x(y)")
            .unwrap();
        assert!(SqliteService::open(&path, None, None).is_err());
        assert_eq!(ADMISSION.lock().unwrap().count, baseline);
    }
    #[test]
    fn global_eight_owner_one_and_join_retains_lease() {
        let mut guards = Vec::new();
        for i in 0..8 {
            guards.push(GlobalLease::acquire(Some(&format!("unit-{i}"))).unwrap());
        }
        assert!(GlobalLease::acquire(None).is_err());
        drop(guards);
        let g = GlobalLease::acquire(Some("retained-unit")).unwrap();
        assert!(GlobalLease::acquire(Some("retained-unit")).is_err());
        let task = Arc::new(Task {
            control: Control::default(),
            interrupt: Mutex::new(None),
            handle: Mutex::new(None),
            result: Mutex::new(None),
            lease: Mutex::new(Some(Lease {
                inner: std::sync::Weak::new(),
                owner: "retained-unit".into(),
                _global: g,
            })),
            finished: AtomicBool::new(false),
            joined: AtomicBool::new(false),
            join_started: Mutex::new(None),
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (join_tx, join_rx) = std::sync::mpsc::sync_channel(1);
        *task.join_started.lock().unwrap() = Some(join_tx);
        let t = task.clone();
        *task.handle.lock().unwrap() = Some(thread::spawn(move || {
            tx.send(()).unwrap();
            if release_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                t.finished.store(true, Ordering::Release);
                return Err(Error::Unavailable);
            }
            t.finished.store(true, Ordering::Release);
            Ok((200, "synthetic".into()))
        }));
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        task.cancel();
        let joining_task = task.clone();
        let joiner = thread::spawn(move || joining_task.join());
        join_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            GlobalLease::acquire(Some("retained-unit")).is_err(),
            "ROUTING_ASSERT/sqlite/retained_lease"
        );
        release_tx.send(()).unwrap();
        assert_eq!(joiner.join().unwrap().unwrap().0, 200);
        assert!(GlobalLease::acquire(Some("retained-unit")).is_ok());
        assert_eq!(task.join().unwrap().0, 200);
    }
}

/// Third immutable interrupt publication, solely for an externally-owned status job.
#[derive(Clone)]
pub struct ExternalContext {
    control: Control,
    handle: Arc<std::sync::OnceLock<rusqlite::InterruptHandle>>,
    active: Arc<AtomicBool>,
}
impl ExternalContext {
    pub fn new(deadline: Instant) -> Self {
        Self {
            control: Control::with_deadline(deadline),
            handle: Arc::new(std::sync::OnceLock::new()),
            active: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn cancel(&self) {
        self.control.cancel();
        if self.active.load(Ordering::Acquire) {
            if let Some(h) = self.handle.get() {
                h.interrupt();
            }
        }
    }
    pub fn check(&self) -> Result<(), Error> {
        self.control.check()
    }
}
/// Explicit failed close ownership; retry never repeats a query or transaction.
#[must_use]
pub struct ExternalFailure {
    pub error: Error,
    pending: Option<Box<Connection>>,
}
impl std::fmt::Debug for ExternalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalFailure")
            .field("error", &self.error)
            .field("pending_close", &self.pending.is_some())
            .finish()
    }
}
impl ExternalFailure {
    pub fn has_pending_close(&self) -> bool {
        self.pending.is_some()
    }
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
fn external_close(c: Connection, error: Error) -> Result<(), ExternalFailure> {
    c.close().map_err(|(c, _)| ExternalFailure {
        error,
        pending: Some(Box::new(c)),
    })
}
fn stat_regular(root: &Root, name: &str) -> Result<Identity, Error> {
    let s = rustix::fs::statat(&root.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| Error::Unavailable)?;
    if rustix::fs::FileType::from_raw_mode(s.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(Error::Unavailable);
    }
    Ok(Identity {
        dev: s.st_dev as u64,
        ino: s.st_ino as u64,
    })
}
/// Startup keys snapshot; no admission, worker or raw SQLite verification descriptor.
pub struct ExternalStatus {
    root: Root,
    path: PathBuf,
    root_id: Identity,
    db_id: Identity,
    policy: Service,
    product_override: Option<bool>,
}
impl ExternalStatus {
    pub fn open(path: &Path, previous: Option<&str>, env: Option<&str>) -> Result<Self, Error> {
        let root = Root::open(path)?;
        let root_id = identity(&root.fd)?;
        let db_id = stat_regular(&root, "cowork.db")?;
        let policy = Service::new(&root, previous)?;
        let product_override = feature_env(env)?;
        let this = Self {
            root,
            path: path.to_owned(),
            root_id,
            db_id,
            policy,
            product_override,
        };
        this.check()?;
        Ok(this)
    }
    fn check(&self) -> Result<(), Error> {
        let current = Root::open(&self.path)?;
        if identity(&current.fd)? != self.root_id
            || identity(&self.root.fd)? != self.root_id
            || stat_regular(&current, "cowork.db")? != self.db_id
        {
            return Err(Error::Unavailable);
        }
        for n in ["cowork.db-wal", "cowork.db-shm"] {
            stat_regular(&current, n)?;
        }
        if !matches!(
            rustix::fs::statat(
                &current.fd,
                "cowork.db-journal",
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW
            ),
            Err(rustix::io::Errno::NOENT)
        ) {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    pub fn job(&self, context: ExternalContext) -> Result<ExternalJob<'_>, ExternalFailure> {
        let pre = (|| {
            context.check()?;
            self.check()?;
            if context.handle.get().is_some() {
                return Err(Error::Unavailable);
            }
            sql(Connection::open_with_flags(
                self.path.join("cowork.db"),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            ))
        })();
        let c = pre.map_err(|error| ExternalFailure {
            error,
            pending: None,
        })?;
        let initialized = (|| {
            context
                .handle
                .set(c.get_interrupt_handle())
                .map_err(|_| Error::Unavailable)?;
            context.active.store(true, Ordering::Release);
            context.check()?;
            configure_ticks(&c, &context.control, 1000)?;
            self.check()?;
            context.check()
        })();
        if let Err(error) = initialized {
            context.active.store(false, Ordering::Release);
            return match external_close(c, error) {
                Ok(()) => Err(ExternalFailure {
                    error,
                    pending: None,
                }),
                Err(e) => Err(e),
            };
        }
        Ok(ExternalJob {
            service: self,
            context,
            conn: Some(c),
            read: false,
        })
    }
}
/// One synchronous connection and read, held/closed by the owning worker.
#[must_use]
pub struct ExternalJob<'a> {
    service: &'a ExternalStatus,
    context: ExternalContext,
    conn: Option<Connection>,
    read: bool,
}
impl ExternalJob<'_> {
    pub fn read(&mut self, owner: &str) -> Answer {
        self.context.check()?;
        if self.read || owner.is_empty() || owner.len() > OWNER_BYTES {
            return Err(Error::Unavailable);
        }
        self.read = true;
        let c = self.conn.as_ref().ok_or(Error::Unavailable)?;
        sql(c.execute_batch("BEGIN DEFERRED"))?;
        let answer = (|| {
            schema(c, &self.context.control, false)?;
            let product = product(c, self.service.product_override)?;
            if !product {
                return self
                    .service
                    .policy
                    .read_with(&self.context.control, false, owner, || {
                        Err(Error::Unavailable)
                    });
            }
            sql(c.authorizer(Some(|ctx: AuthContext<'_>| {
                authorization(ctx, Phase::OAuth)
            })))?;
            schema(c, &self.context.control, true)?;
            sql(c.authorizer(Some(|ctx: AuthContext<'_>| {
                authorization(ctx, Phase::Status)
            })))?;
            self.service
                .policy
                .read_with(&self.context.control, true, owner, || row(c, owner))
        })();
        let rollback = sql(c.execute_batch("ROLLBACK"));
        self.context.check()?;
        rollback?;
        self.service.check()?;
        answer
    }
    pub fn close(mut self) -> Result<(), ExternalFailure> {
        self.context.active.store(false, Ordering::Release);
        let c = self.conn.take().ok_or(ExternalFailure {
            error: Error::Unavailable,
            pending: None,
        })?;
        let _ = c.progress_handler(0, None::<fn() -> bool>);
        external_close(c, Error::Unavailable)
    }
}
fn product(c: &Connection, override_value: Option<bool>) -> Result<bool, Error> {
    if let Some(value) = override_value {
        return Ok(value);
    }
    let mut s=sql(c.prepare("SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1 COLLATE BINARY"))?;
    let mut rows = sql(s.query(["feature:chatgptOAuth"]))?;
    let value = match sql(rows.next())? {
        None => false,
        Some(r) => match sql(r.get_ref(0))? {
            ValueRef::Null => false,
            ValueRef::Text(b) => b == b"true",
            _ => return Err(Error::Unavailable),
        },
    };
    if sql(rows.next())?.is_some() {
        return Err(Error::Unavailable);
    }
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod external_controls {
    use super::*;
    fn setup() -> (tempfile::TempDir, Connection, ExternalStatus) {
        let d = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(d.path()).unwrap();
        std::fs::write(path.join("secrets.key"), [7; 32]).unwrap();
        let c = Connection::open(path.join("cowork.db")).unwrap();
        c.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY,applied_at INTEGER NOT NULL);INSERT INTO schema_migrations VALUES(5,0);CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);INSERT INTO settings VALUES('feature:chatgptOAuth','true');CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL);").unwrap();
        let s = ExternalStatus::open(&path, None, None).unwrap();
        (d, c, s)
    }
    #[test]
    fn external_status_cancel_snapshot_slot_and_ordinary_wal() {
        let (_d, c, s) = setup();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        let control = ExternalContext::new(Instant::now() + Duration::from_secs(2));
        let mut job = s.job(control.clone()).unwrap();
        let answer = job.read("u").unwrap();
        assert_eq!(answer.0, 200);
        assert!(answer.1.contains("disconnected"));
        job.close().unwrap();
        assert!(s.job(control.clone()).is_err());
        let probe = Connection::open(s.path.join("cowork.db")).unwrap();
        probe.busy_timeout(Duration::ZERO).unwrap();
        assert!(
            probe.execute_batch("BEGIN IMMEDIATE").is_err(),
            "ROUTING_ASSERT/external/wal_lock"
        );
        c.execute_batch("ROLLBACK").unwrap();
        let cancelled = ExternalContext::new(Instant::now() + Duration::from_secs(2));
        cancelled.cancel();
        assert!(s.job(cancelled).is_err());
        let expired = ExternalContext::new(Instant::now() - Duration::from_millis(1));
        assert!(s.job(expired).is_err());
        let fresh = ExternalContext::new(Instant::now() + Duration::from_secs(2));
        control.cancel();
        let mut job = s.job(fresh).unwrap();
        assert_eq!(job.read("u").unwrap().0, 200);
        job.close().unwrap();
        c.execute_batch("UPDATE settings SET value='false';DROP TABLE chatgpt_oauth_tokens;")
            .unwrap();
        let mut job = s
            .job(ExternalContext::new(
                Instant::now() + Duration::from_secs(2),
            ))
            .unwrap();
        assert_eq!(job.read("u").unwrap().0, 404);
        job.close().unwrap();
    }
    #[test]
    fn external_vm_static_allotment_does_not_reset() {
        let (_d, c, _s) = setup();
        c.execute_batch("CREATE TABLE t(x INTEGER);WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<50) INSERT INTO t SELECT x FROM n;").unwrap();
        configure_ticks(&c, &Control::default(), 1000).unwrap();
        c.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        for _ in 0..3 {
            assert_eq!(
                c.query_row("SELECT count(*) FROM t a,t b,t c", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                125000
            );
        }
        assert!(
            c.query_row("SELECT count(*) FROM t a,t b,t c", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "ROUTING_ASSERT/external/vm_no_reset"
        );
    }
}
