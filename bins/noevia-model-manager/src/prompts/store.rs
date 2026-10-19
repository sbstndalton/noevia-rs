#[cfg(feature = "native-db-init")]
mod schema;
use super::{
    literals::SEED, Error, Operation, BODY_CAP, NAME_CAP, RESPONSE_CAP, ROW_CAP, SEED_KEY,
};
use rusqlite::{params, types::ValueRef, Connection, OpenFlags, OptionalExtension};
use std::{
    cell::Cell,
    fs::File,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
type Result<T> = std::result::Result<T, Error>;
const DEADLINE: Duration = Duration::from_secs(5);
struct Cancel(Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct Job {
    operation: Operation,
    reply: oneshot::Sender<Result<Vec<u8>>>,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    _permit: OwnedSemaphorePermit,
    /// noevia#2014: a dropped request only sets `cancel`; this keeps its admission permit until
    /// the worker has finished the job or the job is dropped unrun.
    _admitted: crate::docker::Registration,
    #[cfg(test)]
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
}
pub struct Prompts {
    schema_initialized: bool,
    send: Option<mpsc::Sender<Job>>,
    permits: Arc<Semaphore>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Prompts {
    fn drop(&mut self) {
        self.send.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Prompts {
    /// Full schema plus prompt seeding completed on the lifetime writer worker.
    pub fn schema_initialized(&self) -> bool {
        self.schema_initialized
    }

    pub async fn open(root: &Path) -> Result<Self> {
        Self::open_with_schema(root, false).await
    }
    /// Bootstrap on this worker's secure connection, before the existing seed and child startup.
    /// The writer lock remains held by this same Store for its entire lifetime.
    pub async fn open_with_schema(root: &Path, full_schema: bool) -> Result<Self> {
        if full_schema && !cfg!(feature = "native-db-init") {
            return Err(Error::Store);
        }
        if root
            .as_os_str()
            .len()
            .saturating_add("/model_loader.db".len())
            > 4096
        {
            return Err(Error::Store);
        }
        Self::open_configured(root, timestamp, full_schema).await
    }
    #[cfg(test)]
    pub(super) async fn open_with_clock(root: &Path, clock: fn() -> Result<f64>) -> Result<Self> {
        Self::open_configured(root, clock, false).await
    }
    async fn open_configured(
        root: &Path,
        clock: fn() -> Result<f64>,
        full_schema: bool,
    ) -> Result<Self> {
        let start = Instant::now() + DEADLINE;
        let cancel = Cancel(Arc::new(AtomicBool::new(false)));
        let startup_cancel = cancel.0.clone();
        let root = root.to_owned();
        let (send, mut receive) = mpsc::channel::<Job>(4);
        let (ready, result) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("native-prompts".into())
            .spawn(move || {
                let mut store = match Store::open_budget(&root, start, startup_cancel.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                store.clock = clock;
                if let Err(e) = store.budget(start, startup_cancel) {
                    let _ = ready.send(Err(e));
                    return;
                }
                #[cfg(feature = "native-db-init")]
                if full_schema {
                    if let Err(e) = schema::initialize(&store) {
                        let _ = ready.send(Err(e));
                        return;
                    }
                }
                #[cfg(not(feature = "native-db-init"))]
                let _ = full_schema;
                if let Err(e) = store.seed() {
                    let _ = ready.send(Err(e));
                    return;
                }
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Some(job) = receive.blocking_recv() {
                    #[cfg(test)]
                    if let Some(hook) = &job.hook {
                        hook();
                    }
                    let answer = store
                        .budget(job.deadline, job.cancel.clone())
                        .and_then(|()| store.run(job.operation, job.deadline, &job.cancel));
                    let _ = job.reply.send(answer);
                    // The rest of the job, its admission registration included, drops here.
                }
            })
            .map_err(|_| Error::Store)?;
        let readiness = tokio::time::timeout_at(start.into(), result).await;
        if !matches!(readiness, Ok(Ok(Ok(())))) {
            cancel.0.store(true, Ordering::Relaxed);
            drop(send);
            let _ = worker.join();
            return match readiness {
                Ok(Ok(Err(e))) => Err(e),
                _ => Err(Error::Store),
            };
        }
        Ok(Self {
            schema_initialized: full_schema,
            send: Some(send),
            permits: Arc::new(Semaphore::new(4)),
            worker: Some(worker),
        })
    }
    pub async fn response(&self, operation: Operation) -> Result<Vec<u8>> {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let deadline = Instant::now() + DEADLINE;
        let cancel = Cancel(Arc::new(AtomicBool::new(false)));
        let (reply, receive) = oneshot::channel();
        self.send
            .as_ref()
            .ok_or(Error::Store)?
            .try_send(Job {
                operation,
                reply,
                deadline,
                cancel: cancel.0.clone(),
                _permit: permit,
                _admitted: crate::docker::Registration::new(),
                #[cfg(test)]
                hook: crate::docker::BLOCKING_HOOK.try_with(Clone::clone).ok(),
            })
            .map_err(|_| Error::Busy)?;
        tokio::time::timeout_at(deadline.into(), receive)
            .await
            .map_err(|_| Error::Store)?
            .map_err(|_| Error::Store)?
    }
}
struct Store {
    db: Connection,
    path: PathBuf,
    identities: Vec<(PathBuf, u64, u64)>,
    _lock: File,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    clock: fn() -> Result<f64>,
}
fn timestamp() -> Result<f64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Store)?
        .as_secs_f64())
}
#[cfg_attr(test, track_caller)]
fn io<T>(r: std::io::Result<T>) -> Result<T> {
    #[cfg(test)]
    let caller = std::panic::Location::caller();
    r.map_err(|_error| {
        #[cfg(test)]
        eprintln!(
            "prompts test I/O failure at {caller}: kind={:?}",
            _error.kind()
        );
        Error::Store
    })
}
#[cfg_attr(test, track_caller)]
fn sql<T>(r: rusqlite::Result<T>) -> Result<T> {
    #[cfg(test)]
    let caller = std::panic::Location::caller();
    r.map_err(|_error| {
        #[cfg(test)]
        eprintln!(
            "prompts test SQL failure at {caller}: code={:?}",
            _error.sqlite_error()
        );
        Error::Store
    })
}
fn safe_file(path: &Path, optional: bool) -> Result<Option<std::fs::Metadata>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if optional && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::Store),
    };
    let uid = rustix::process::geteuid().as_raw();
    if !metadata.is_file()
        || metadata.nlink() != 1
        || (metadata.uid() != 0 && metadata.uid() != uid)
        || metadata.mode() & 0o022 != 0
    {
        #[cfg(test)]
        eprintln!(
            "prompts test safe-file rejection: regular={} nlink={} uid={} euid={} mode={:o}",
            metadata.is_file(),
            metadata.nlink(),
            metadata.uid(),
            uid,
            metadata.mode()
        );
        return Err(Error::Store);
    }
    Ok(Some(metadata))
}
fn autoincrement_keyword(sql: &str) -> bool {
    sql_keyword(sql, b"AUTOINCREMENT")
}
fn sql_keyword(sql: &str, wanted: &[u8]) -> bool {
    if sql.len() > 8192 {
        return false;
    }
    let bytes = sql.as_bytes();
    let mut at = 0;
    while let Some(byte) = bytes.get(at).copied() {
        if byte == b'-' && bytes.get(at + 1) == Some(&b'-') {
            at += 2;
            while bytes.get(at).is_some_and(|b| *b != b'\n') {
                at += 1;
            }
        } else if byte == b'/' && bytes.get(at + 1) == Some(&b'*') {
            at += 2;
            while bytes.get(at).is_some() {
                if bytes.get(at) == Some(&b'*') && bytes.get(at + 1) == Some(&b'/') {
                    at += 2;
                    break;
                }
                at += 1;
            }
        } else if matches!(byte, b'\'' | b'"' | b'`' | b'[') {
            let closing = if byte == b'[' { b']' } else { byte };
            at += 1;
            while let Some(b) = bytes.get(at).copied() {
                at += 1;
                if b == closing {
                    if closing != b']' && bytes.get(at) == Some(&closing) {
                        at += 1;
                    } else {
                        break;
                    }
                }
            }
        } else if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 128 {
            let start = at;
            at += 1;
            while bytes
                .get(at)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$' || *b >= 128)
            {
                at += 1;
            }
            if bytes
                .get(start..at)
                .is_some_and(|token| token.eq_ignore_ascii_case(wanted))
            {
                return true;
            }
        } else {
            at += 1;
        }
    }
    false
}
impl Store {
    #[cfg(test)]
    fn open(root: &Path) -> Result<Self> {
        Self::open_budget(
            root,
            Instant::now() + DEADLINE,
            Arc::new(AtomicBool::new(false)),
        )
    }
    fn open_budget(root: &Path, deadline: Instant, cancel: Arc<AtomicBool>) -> Result<Self> {
        let text = root.to_str().ok_or(Error::Store)?;
        if text.len().saturating_add("/model_loader.db".len()) > 4096
            || !root.is_absolute()
            || text.contains('\0')
            || root
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(Error::Store);
        }
        if cfg!(all(not(target_os = "linux"), not(test))) {
            return Err(Error::Store);
        }
        #[cfg(target_os = "linux")]
        {
            let fs = rustix::fs::statfs(root).map_err(|_| Error::Store)?;
            // Local Linux ext, XFS, Btrfs, tmpfs and container overlay only.
            if !matches!(
                fs.f_type,
                0xef53 | 0x58465342 | 0x9123683e | 0x01021994 | 0x794c7630
            ) {
                return Err(Error::Store);
            }
        }
        let uid = rustix::process::geteuid().as_raw();
        let mut identities = Vec::new();
        identities
            .try_reserve(root.components().count() + 1)
            .map_err(|_| Error::Limit)?;
        let mut current = PathBuf::new();
        for component in root.components() {
            current.push(component.as_os_str());
            let metadata = io(std::fs::symlink_metadata(&current))?;
            if !metadata.is_dir() || (metadata.uid() != 0 && metadata.uid() != uid) {
                return Err(Error::Store);
            }
            // Sticky temporary parents are supported only beneath a private owned next directory.
            if metadata.mode() & 0o022 != 0 {
                if metadata.mode() & 0o1000 == 0 || metadata.uid() != 0 || current == root {
                    return Err(Error::Store);
                }
                let next = root
                    .strip_prefix(&current)
                    .map_err(|_| Error::Store)?
                    .components()
                    .next()
                    .ok_or(Error::Store)?;
                let next = io(std::fs::symlink_metadata(current.join(next.as_os_str())))?;
                if !next.is_dir() || next.uid() != uid || next.mode() & 0o077 != 0 {
                    return Err(Error::Store);
                }
            }
            identities.push((current.clone(), metadata.dev(), metadata.ino()));
        }
        let path = root.join("model_loader.db");
        for suffix in ["", "-wal", "-shm", ".prompts.lock"] {
            safe_file(&root.join(format!("model_loader.db{suffix}")), true)?;
        }
        use std::os::unix::fs::OpenOptionsExt;
        let lock = io(File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                rustix::fs::OFlags::NOFOLLOW.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32,
            )
            .open(root.join("model_loader.db.prompts.lock")))?;
        lock.try_lock().map_err(|_| Error::Busy)?;
        let lm =
            safe_file(&root.join("model_loader.db.prompts.lock"), false)?.ok_or(Error::Store)?;
        let opened = io(lock.metadata())?;
        if opened.dev() != lm.dev() || opened.ino() != lm.ino() {
            return Err(Error::Store);
        }
        identities.push((
            root.join("model_loader.db.prompts.lock"),
            lm.dev(),
            lm.ino(),
        ));
        let file = io(File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                rustix::fs::OFlags::NOFOLLOW.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32,
            )
            .open(&path))?;
        let metadata = safe_file(&path, false)?.ok_or(Error::Store)?;
        if io(file.metadata())?.ino() != metadata.ino() {
            return Err(Error::Store);
        }
        identities.push((path.clone(), metadata.dev(), metadata.ino()));
        // POSIX close of any descriptor on this inode cancels this process's
        // SQLite locks. Finish raw verification before SQLite can acquire them.
        drop(file);
        let db = sql(Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ))?;
        sql(db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH, 1_048_576))?;
        sql(db.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_SQL_LENGTH, 1_048_576))?;
        sql(db.busy_timeout(Duration::from_millis(250)))?;
        let mut store = Self {
            db,
            path,
            identities,
            _lock: lock,
            deadline,
            cancel: cancel.clone(),
            clock: timestamp,
        };
        store.budget(deadline, cancel)?;
        store.check()?;
        let mode: String = sql(store
            .db
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)))?;
        if mode != "wal" {
            return Err(Error::Store);
        }
        sql(store.db.execute_batch("CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE IF NOT EXISTS prompts(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL,body TEXT NOT NULL,created_at REAL NOT NULL);"))?;
        store.schema()?;
        store.check()?;
        Ok(store)
    }
    fn budget(&mut self, deadline: Instant, cancel: Arc<AtomicBool>) -> Result<()> {
        self.deadline = deadline;
        self.cancel = cancel.clone();
        let work = Cell::new(0usize);
        sql(self.db.progress_handler(
            1,
            Some(move || {
                // A short INSERT resumed after a busy wait must observe cancellation too.
                let n = work.get().saturating_add(1);
                work.set(n);
                n > 1_000_000 || Instant::now() >= deadline || cancel.load(Ordering::Relaxed)
            }),
        ))?;
        Ok(())
    }
    fn check(&self) -> Result<()> {
        if Instant::now() >= self.deadline || self.cancel.load(Ordering::Relaxed) {
            #[cfg(test)]
            eprintln!(
                "prompts test check budget: expired={} cancelled={}",
                Instant::now() >= self.deadline,
                self.cancel.load(Ordering::Relaxed)
            );
            return Err(Error::Store);
        }
        for (path, dev, ino) in &self.identities {
            let m = io(std::fs::symlink_metadata(path))?;
            if m.dev() != *dev || m.ino() != *ino || m.file_type().is_symlink() {
                #[cfg(test)]
                eprintln!(
                    "prompts test identity mismatch: expected_dev={dev} expected_ino={ino} actual_dev={} actual_ino={} symlink={}",
                    m.dev(), m.ino(), m.file_type().is_symlink()
                );
                return Err(Error::Store);
            }
        }
        // SQLite readers may checkpoint and remove/recreate WAL and SHM. These
        // companions have no permanent inode identity; every present companion
        // still passes the same no-symlink, regular-file, link/owner/mode checks.
        // Root ancestors, main DB and writer lock retain permanent identities.
        // As at open, DATA_DIR must be trusted and stable: these path checks do
        // not anchor SQLite against hostile concurrent same-UID rename races.
        for suffix in ["", "-wal", "-shm", ".prompts.lock"] {
            safe_file(
                &self.path.with_file_name(format!("model_loader.db{suffix}")),
                true,
            )?;
        }
        Ok(())
    }
    fn schema(&self) -> Result<()> {
        for (table, required) in [
            (
                "prompts",
                vec![
                    ("id", "INTEGER", false, 1),
                    ("name", "TEXT", true, 0),
                    ("body", "TEXT", true, 0),
                    ("created_at", "REAL", true, 0),
                ],
            ),
            (
                "kv",
                vec![("key", "TEXT", false, 1), ("value", "TEXT", true, 0)],
            ),
        ] {
            let mut statement = sql(self.db.prepare(match table {
                "prompts" => "PRAGMA table_info(prompts)",
                _ => "PRAGMA table_info(kv)",
            }))?;
            let mut rows = sql(statement.query([]))?;
            let mut found = 0usize;
            let mut total = 0usize;
            while let Some(row) = sql(rows.next())? {
                total += 1;
                if total > 256 {
                    return Err(Error::Limit);
                }
                let name = sql(row.get_ref(1))?.as_str().map_err(|_| Error::Store)?;
                let kind = sql(row.get_ref(2))?.as_str().map_err(|_| Error::Store)?;
                if name.len() > 8192 || kind.len() > 8192 {
                    return Err(Error::Limit);
                }
                let notnull: i64 = sql(row.get(3))?;
                let pk: i64 = sql(row.get(5))?;
                if let Some((_, want, nn, key)) = required.iter().find(|(n, _, _, _)| *n == name) {
                    if !kind.eq_ignore_ascii_case(want) || (notnull != 0) != *nn || pk != *key {
                        return Err(Error::Store);
                    }
                    found += 1;
                }
            }
            if found != required.len() {
                return Err(Error::Store);
            }
        }
        let autoincrement = sql(self.db.query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='prompts'",
            [],
            |r| {
                let schema = r.get_ref(0)?.as_str()?;
                Ok(autoincrement_keyword(schema))
            },
        ))?;
        if !autoincrement {
            return Err(Error::Store);
        }
        Ok(())
    }
    fn list(&self) -> Result<Vec<Prompt>> {
        let mut statement=sql(self.db.prepare("SELECT id,typeof(name),length(CAST(name AS BLOB)),typeof(body),length(CAST(body AS BLOB)),typeof(created_at),name,body,created_at FROM prompts ORDER BY name COLLATE NOCASE LIMIT ?"))?;
        let mut rows = sql(statement.query([ROW_CAP as i64 + 1]))?;
        let mut out = Vec::new();
        out.try_reserve(ROW_CAP).map_err(|_| Error::Limit)?;
        let mut bytes = 0usize;
        while let Some(row) = sql(rows.next())? {
            if out.len() >= ROW_CAP {
                return Err(Error::Limit);
            }
            let name_kind: &str = sql(row.get_ref(1))?.as_str().map_err(|_| Error::Store)?;
            let body_kind: &str = sql(row.get_ref(3))?.as_str().map_err(|_| Error::Store)?;
            let time_kind: &str = sql(row.get_ref(5))?.as_str().map_err(|_| Error::Store)?;
            let nl: i64 = sql(row.get(2))?;
            let bl: i64 = sql(row.get(4))?;
            if name_kind != "text"
                || body_kind != "text"
                || !matches!(time_kind, "integer" | "real")
                || nl < 0
                || nl > NAME_CAP as i64
                || bl < 0
                || bl > BODY_CAP as i64
            {
                return Err(Error::Limit);
            }
            bytes = bytes
                .saturating_add(nl as usize)
                .saturating_add(bl as usize);
            if bytes > RESPONSE_CAP {
                return Err(Error::Limit);
            }
            let name = copy(sql(row.get_ref(6))?)?;
            let body = copy(sql(row.get_ref(7))?)?;
            let created_at: f64 = sql(row.get(8))?;
            if !created_at.is_finite() {
                return Err(Error::Limit);
            }
            out.push(Prompt {
                id: sql(row.get(0))?,
                name,
                body,
                created_at,
            });
        }
        Ok(out)
    }
    fn insert(&mut self, name: &str, body: &str) -> Result<i64> {
        if name.len() > NAME_CAP || body.len() > BODY_CAP {
            return Err(Error::Limit);
        }
        let mut statement = sql(self.db.prepare("SELECT id FROM prompts LIMIT ?"))?;
        let mut rows = sql(statement.query([ROW_CAP as i64]))?;
        let mut count = 0;
        while sql(rows.next())?.is_some() {
            count += 1;
            if count >= ROW_CAP {
                return Err(Error::Limit);
            }
        }
        drop(rows);
        drop(statement);
        self.check()?;
        let now = (self.clock)()?;
        sql(self.db.execute(
            "INSERT INTO prompts(name, body, created_at) VALUES(?, ?, ?)",
            params![name, body, now],
        ))?;
        Ok(self.db.last_insert_rowid())
    }
    fn seed(&mut self) -> Result<()> {
        self.check()?;
        let flag: Option<(String, i64)> = sql(self
            .db
            .query_row(
                "SELECT typeof(value),length(CAST(value AS BLOB)) FROM kv WHERE key=?",
                [SEED_KEY],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional())?;
        if let Some((kind, length)) = flag {
            if kind != "text" || !(0..=1_048_576).contains(&length) {
                return Err(Error::Limit);
            }
            if length > 0 {
                return self.check();
            }
        }
        let have = self.list()?;
        for (name, body) in SEED {
            if !have.iter().any(|p| p.name == *name) {
                self.check()?;
                self.insert(name, body)?;
            }
        }
        self.check()?;
        sql(self.db.execute("INSERT INTO kv(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![SEED_KEY,"1"]))?;
        self.check()
    }
    fn run(&mut self, op: Operation, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<u8>> {
        self.check()?;
        if Instant::now() >= deadline || cancel.load(Ordering::Relaxed) {
            return Err(Error::Store);
        }
        let id = match op {
            Operation::List => None,
            Operation::Add(name, body) => Some(self.insert(&name, &body)?),
            Operation::Delete(id) => {
                if sql(self.db.execute("DELETE FROM prompts WHERE id = ?", [id]))? == 0 {
                    return Err(Error::NotFound);
                }
                None
            }
        };
        // Mutations have committed. Readback is deliberately a separate statement.
        self.check()?;
        let rows = self.list()?;
        // Readback may recreate SQLite companions; validate their safety before
        // returning success as well as before SQLite touches the named paths.
        self.check()?;
        let mut writer = Bytes {
            bytes: Vec::new(),
            deadline,
            cancel: self.cancel.clone(),
        };
        writer.bytes.try_reserve(1024).map_err(|_| Error::Limit)?;
        if let Some(id) = id {
            write!(writer, "{{\"id\":{id},\"prompts\":").map_err(|_| Error::Limit)?;
        } else {
            writer
                .write_all(b"{\"prompts\":")
                .map_err(|_| Error::Limit)?;
        }
        serde_json::to_writer(&mut writer, &rows).map_err(|_| Error::Limit)?;
        writer.write_all(b"}").map_err(|_| Error::Limit)?;
        Ok(writer.bytes)
    }
}
fn copy(value: ValueRef<'_>) -> Result<String> {
    let value = value.as_str().map_err(|_| Error::Limit)?;
    let mut text = String::new();
    text.try_reserve_exact(value.len())
        .map_err(|_| Error::Limit)?;
    text.push_str(value);
    Ok(text)
}
#[derive(serde::Serialize)]
struct Prompt {
    id: i64,
    name: String,
    body: String,
    created_at: f64,
}
struct Bytes {
    bytes: Vec<u8>,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}
impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.deadline || self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("response cancelled"));
        }
        if self.bytes.len().saturating_add(bytes.len()) > RESPONSE_CAP {
            return Err(std::io::Error::other("response cap"));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
