//! The off-site backup engine (sbstndalton/noevia#2190 round 7), ported from core
//! `offsite-backup.cjs` over the pure format in crates/offsite-backup-format: client-side encrypted,
//! deduplicated, versioned snapshots with retention and a restore test. A store Node wrote is read
//! here and the other way round.
//!
//! - `backup`: seals `config` once, then walks every backed-up root (never following a symlink,
//!   skipping `-wal`/`-shm`/`-journal` files), stores each file as sealed 4 MiB chunks named by their
//!   keyed hash (a chunk the store already has is not sent again), and last writes the sealed
//!   manifest. A live SQLite file (`.db`, `.sqlite`, `.sqlite3`) is read from a consistent copy
//!   (`VACUUM INTO` on a read-only connection, core used the online backup API); when no consistent
//!   copy can be made the file is skipped, never raw-read. A backed-up folder that is missing or
//!   unreadable fails the run. Every other skip (a file that vanished or cannot be opened or read, a
//!   name that is not UTF-8, an unreadable subfolder, a database without a consistent copy) is
//!   counted and flags the manifest `incomplete`; a store failure ends the run. A destination with
//!   no `config` is only initialised when the caller does not expect one (an earlier backup was made
//!   to it: an unmounted NAS is an empty directory).
//! - `forget`: retention (7 daily, 4 weekly, 6 monthly, always the newest and the newest complete),
//!   then the chunks no kept snapshot references. It never prunes a chunk while any snapshot listed
//!   in the store could not be read (a flaky read of a manifest must not turn its chunks into
//!   garbage) and never a chunk written in the last 24 hours (a backup of another process that has
//!   not written its manifest yet). Refused while a backup runs.
//! - `verify`: reads the newest snapshot back chunk by chunk, checking every chunk id, every file hash
//!   and size, without writing the files anywhere. The newest listed snapshot is the one tested: when
//!   it cannot be read the test fails rather than passing an older one.
//!
//! A scheduled run takes a [`Pace`]: before every file it waits out the quiet windows
//! (offsite_backup_format::schedule); an administrator's own run has none. It never pauses inside a
//! file, so no file is held open or half read across a window.
use crate::gdrive::{hex_of, random, Fail};
use crate::offsite_store::{io_fail, Store, STALE_TEMP};
use offsite_backup_format as fmt;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const POLL: Duration = Duration::from_secs(30);
/// A chunk written less than this long ago is never pruned: another process's backup may have
/// stored it and not yet written the manifest that names it.
pub const CHUNK_GRACE_MS: i64 = 24 * 3600 * 1000;
/// Scratch directories this old cannot belong to a live job (a job ends within its deadline).
const SCRATCH_STALE: Duration = Duration::from_secs(7 * 3600);
/// The names of the temporary directories this engine makes under the system temp directory.
const SCRATCH_PREFIXES: [&str; 2] = ["noevia-sqlite-", "noevia-restore-test-"];

/// A scheduled run's wait for the quiet windows. `clock` is Date.now(), replaceable in tests.
#[derive(Clone)]
pub struct Pace {
    pub zone: Option<fmt::schedule::Zone>,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub step: Duration,
    /// Called each time a run starts to wait (the service's status shows it).
    pub waiting: Arc<dyn Fn(bool) + Send + Sync>,
}

impl Pace {
    pub fn none() -> Pace {
        Pace {
            zone: None,
            clock: Arc::new(crate::gdrive::now_ms),
            step: POLL,
            waiting: Arc::new(|_| {}),
        }
    }

    /// A scheduled run's pace; `zone` is the host's zone for the appdata window (the NAS window is
    /// always read in America/New_York).
    pub fn scheduled(zone: fmt::schedule::Zone) -> Pace {
        Pace {
            zone: Some(zone),
            ..Pace::none()
        }
    }

    /// Whether this is a scheduled run (an administrator's own run is not).
    pub fn is_scheduled(&self) -> bool {
        self.zone.is_some()
    }

    /// Returns once the clock is outside every quiet window.
    pub async fn checkpoint(&self) {
        let Some(zone) = self.zone else { return };
        let mut announced = false;
        while let Some(end) = fmt::schedule::quiet_until(zone, (self.clock)()) {
            if !announced {
                (self.waiting)(true);
                announced = true;
            }
            let wait = u64::try_from(end - (self.clock)()).unwrap_or(0).max(1);
            tokio::time::sleep(self.step.min(Duration::from_millis(wait))).await;
        }
        if announced {
            (self.waiting)(false);
        }
    }
}

pub fn log(event: &str, fields: Vec<(&str, js_json::JValue)>) {
    let mut pairs = vec![("event", js_json::JValue::Str(event.to_owned()))];
    pairs.extend(fields);
    println!(
        "[backup] {}",
        js_json::stringify(&js_json::JValue::obj(pairs)).unwrap_or_default()
    );
}

pub struct BackupResult {
    pub id: String,
    pub time: i64,
    pub files: usize,
    pub uploaded_chunks: usize,
    pub uploaded_bytes: u64,
    pub incomplete: bool,
    /// Files and folders left out because they could not be read (each also set `incomplete`).
    pub skipped: usize,
}

pub struct Retained {
    pub kept: usize,
    pub removed: Vec<String>,
    pub pruned_chunks: usize,
    /// Unreferenced chunks left alone because they are under [`CHUNK_GRACE_MS`] old (or undated).
    pub young_chunks: usize,
    /// Snapshots the store lists but that could not be read; while there are any, no chunk is pruned.
    pub unreadable: usize,
}

pub struct SnapshotInfo {
    pub id: String,
    pub time: i64,
    pub files: usize,
    pub bytes: u64,
    pub incomplete: bool,
}

/// Every readable snapshot, newest first, and the ids the store lists that could not be read.
pub struct Snapshots {
    pub list: Vec<SnapshotInfo>,
    pub failed: Vec<String>,
}

impl std::ops::Deref for Snapshots {
    type Target = [SnapshotInfo];
    fn deref(&self) -> &[SnapshotInfo] {
        &self.list
    }
}

impl IntoIterator for Snapshots {
    type Item = SnapshotInfo;
    type IntoIter = std::vec::IntoIter<SnapshotInfo>;
    fn into_iter(self) -> Self::IntoIter {
        self.list.into_iter()
    }
}

fn fail(message: &str, status: u16) -> Fail {
    Fail::new(message, status)
}

fn format_fail(e: fmt::Error) -> Fail {
    match e {
        fmt::Error::KeyLength => fail("The backup key must be 32 bytes (64 hex characters).", 400),
        fmt::Error::Truncated => fail("A backup object is truncated.", 422),
        fmt::Error::Authentication => fail(
            "A backup object failed authentication (wrong key or tampered data).",
            422,
        ),
        fmt::Error::Invalid => fail("The snapshot manifest is invalid.", 422),
        fmt::Error::Seal => fail("A backup object could not be encrypted.", 500),
    }
}

pub struct Engine {
    store: Store,
    keys: Arc<fmt::Keys>,
    paths: Vec<PathBuf>,
    running: AtomicBool,
    lock_file: Option<PathBuf>,
}

struct Running<'a>(&'a AtomicBool);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Holds the cross-process lock of a backup job until dropped.
pub struct JobLock {
    _file: std::fs::File,
}

/// `/\.(db|sqlite3?)$/`.
fn is_sqlite(path: &Path) -> bool {
    let name = path.to_string_lossy();
    [".db", ".sqlite", ".sqlite3"]
        .iter()
        .any(|e| name.ends_with(e))
}

/// What a walk of one root found.
#[derive(Default)]
struct Walked {
    files: Vec<PathBuf>,
    /// Entries left out because they could not be listed, typed or named in UTF-8.
    skipped: usize,
}

/// Every regular file under `root`, directories in name order, symlinks and `-wal`/`-shm`/
/// `-journal` files left out. A root that cannot be listed is an error; anything below it that
/// cannot be read is counted in `skipped`. A name that is not UTF-8 cannot be written to a manifest
/// (two such names would collapse into one path and make every restore refuse), so it is skipped.
fn walk(root: &Path, leave_out: Option<&Path>) -> Result<Walked, std::io::Error> {
    fn visit(dir: &Path, out: &mut Walked, leave_out: Option<&Path>) {
        let read = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => {
                out.skipped += 1;
                return;
            }
        };
        let mut entries = Vec::new();
        for entry in read {
            match entry {
                Ok(e) => entries.push(e),
                Err(_) => out.skipped += 1,
            }
        }
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                out.skipped += 1;
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                out.skipped += 1;
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                visit(&path, out, leave_out);
            } else if kind.is_file() {
                if ["-wal", "-shm", "-journal"]
                    .iter()
                    .any(|s| name.ends_with(s))
                {
                    continue;
                }
                if leave_out.is_some_and(|l| l == path) {
                    continue;
                }
                out.files.push(path);
            }
        }
    }
    // The root itself must be listable: a missing folder is not an empty backup.
    drop(std::fs::read_dir(root)?);
    let mut out = Walked::default();
    visit(root, &mut out, leave_out);
    Ok(out)
}

/// Why a consistent copy of a live SQLite database could not be made.
enum NoCopy {
    ScratchFull,
    Failed,
}

/// A consistent on-disk copy of a live SQLite database: `(copy, scratch directory)`.
fn sqlite_snapshot(abs: &Path) -> Result<(PathBuf, PathBuf), NoCopy> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = std::env::temp_dir().join(format!("noevia-sqlite-{}", hex_of(&random::<8>())));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| {
            if crate::offsite_store::is_full(&e) {
                NoCopy::ScratchFull
            } else {
                NoCopy::Failed
            }
        })?;
    let copy = dir.join("copy.db");
    let made = (|| -> Result<(), server_store::rusqlite::Error> {
        let conn = server_store::rusqlite::Connection::open_with_flags(
            abs,
            server_store::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | server_store::rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute("VACUUM INTO ?1", [copy.to_string_lossy().as_ref()])?;
        Ok(())
    })();
    match made {
        Ok(()) => Ok((copy, dir)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(
                if matches!(&e, server_store::rusqlite::Error::SqliteFailure(f, _)
                    if f.code == server_store::rusqlite::ErrorCode::DiskFull)
                {
                    NoCopy::ScratchFull
                } else {
                    NoCopy::Failed
                },
            )
        }
    }
}

/// Removes the scratch directories of runs that died (a kill, a power cut) from the system temp
/// directory; only ours by name, only once too old to belong to a live job.
fn sweep_scratch(tmp: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !SCRATCH_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let stale = e
            .metadata()
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= SCRATCH_STALE);
        if stale && std::fs::remove_dir_all(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

struct Piecewise {
    file: std::fs::File,
    whole: Sha256,
    size: u64,
    first: bool,
    done: bool,
}

/// Reads the next piece (an empty file yields one empty piece, so it still has a chunk list).
type Piece = std::io::Result<Option<(Vec<u8>, String)>>;

fn next_piece(mut st: Piecewise, keys: &fmt::Keys) -> (Piecewise, Piece) {
    let mut buf = vec![0u8; fmt::CHUNK];
    let mut filled = 0;
    while filled < fmt::CHUNK {
        match st.file.read(buf.get_mut(filled..).unwrap_or_default()) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return (st, Err(e)),
        }
    }
    buf.truncate(filled);
    if filled == 0 && !st.first {
        st.done = true;
        return (st, Ok(None));
    }
    st.first = false;
    if filled < fmt::CHUNK {
        st.done = true;
    }
    st.whole.update(&buf);
    st.size += filled as u64;
    let id = keys.chunk_id(&buf);
    (st, Ok(Some((buf, id))))
}

fn unreachable_fail() -> Fail {
    fail("The backup destination could not be reached.", 502)
}

/// What read of the store's snapshot manifests found.
struct ReadAll {
    ok: Vec<(String, fmt::Manifest)>,
    failed: Vec<String>,
}

impl Engine {
    pub fn new(store: Store, key: &[u8], paths: Vec<PathBuf>) -> Result<Engine, Fail> {
        Ok(Engine {
            store,
            keys: Arc::new(fmt::Keys::derive(key).map_err(format_fail)?),
            paths,
            running: AtomicBool::new(false),
            lock_file: None,
        })
    }

    /// Serialises backup jobs across processes on this file (see [`Engine::lock_job`]); the file
    /// itself is left out of the backup.
    pub fn with_lock_file(mut self, path: PathBuf) -> Engine {
        self.lock_file = Some(path);
        self
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// An exclusive `flock` on the lock file, held for a whole job (a backup and its retention, or
    /// a restore test). A second process, or a second instance started beside the first, is
    /// refused instead of racing it: retention of one must never see the other's chunks without
    /// their manifest.
    pub fn lock_job(&self) -> Result<Option<JobLock>, Fail> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let Some(path) = &self.lock_file else {
            return Ok(None);
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(|_| fail("The backup lock could not be opened.", 502))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(JobLock { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Err(fail(
                "Another backup job is running in this installation.",
                409,
            )),
            Err(std::fs::TryLockError::Error(_)) => {
                Err(fail("The backup lock could not be taken.", 502))
            }
        }
    }

    async fn seal(&self, plain: Vec<u8>) -> Result<Vec<u8>, Fail> {
        let keys = Arc::clone(&self.keys);
        tokio::task::spawn_blocking(move || keys.seal(&plain, &random::<12>()))
            .await
            .map_err(|_| fail("A backup object could not be encrypted.", 500))?
            .map_err(format_fail)
    }

    async fn open(&self, sealed: Vec<u8>) -> Result<zeroize::Zeroizing<Vec<u8>>, Fail> {
        let keys = Arc::clone(&self.keys);
        tokio::task::spawn_blocking(move || keys.open(&sealed))
            .await
            .map_err(|_| unreachable_fail())?
            .map_err(format_fail)
    }

    /// Opens `config`, or creates it. With `expect_store` an absent `config` is an error instead:
    /// backups were made here before, so an empty destination is far more likely an unmounted
    /// disk or the wrong bucket than a new store (initialising it would start a second, unrelated
    /// backup chain and make the Drive copy start a `noevia-offsite (2)` beside the real one).
    async fn ensure_config(&self, now: i64, expect_store: bool) -> Result<(), Fail> {
        if let Some(existing) = self.store.get("config").await? {
            let plain = self.open(existing).await?;
            if !fmt::config_is_current(&plain).map_err(format_fail)? {
                return Err(fail(
                    "The destination holds a different backup format.",
                    409,
                ));
            }
            return Ok(());
        }
        if expect_store {
            return Err(fail(
                "The backup destination is empty, but backups were made to it before. Check that the destination is mounted and is the right folder or bucket; nothing was written.",
                409,
            ));
        }
        let sealed = self.seal(fmt::config_json(now).into_bytes()).await?;
        self.store.put("config", sealed).await
    }

    /// Every backed-up folder must exist and be listable before anything is written.
    fn check_roots(&self) -> Result<(), Fail> {
        for root in &self.paths {
            if std::fs::read_dir(root).is_err() {
                return Err(fail(
                    &format!(
                        "The backed-up folder {} is missing or cannot be read.",
                        root.display()
                    ),
                    409,
                ));
            }
        }
        Ok(())
    }

    /// Clears what a killed run left behind: half-written store files and scratch directories.
    async fn sweep(&self) {
        let removed = self.store.sweep_temp(STALE_TEMP).await;
        let tmp = std::env::temp_dir();
        let scratch = tokio::task::spawn_blocking(move || sweep_scratch(&tmp))
            .await
            .unwrap_or(0);
        if removed + scratch > 0 {
            log(
                "offsite.sweep",
                vec![
                    ("storeTemp", js_json::JValue::Num(removed as f64)),
                    ("scratch", js_json::JValue::Num(scratch as f64)),
                ],
            );
        }
    }

    pub async fn backup(&self, pace: &Pace) -> Result<BackupResult, Fail> {
        self.backup_expecting(pace, false).await
    }

    /// [`Engine::backup`]; `expect_store` says an earlier backup was made to this destination (see
    /// [`Engine::ensure_config`]).
    pub async fn backup_expecting(
        &self,
        pace: &Pace,
        expect_store: bool,
    ) -> Result<BackupResult, Fail> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(fail("A backup is already running.", 409));
        }
        let _running = Running(&self.running);
        self.sweep().await;
        pace.checkpoint().await;
        self.check_roots()?;
        self.ensure_config(crate::gdrive::now_ms(), expect_store)
            .await?;
        let mut known: HashSet<String> = self
            .store
            .list("data/")
            .await?
            .into_iter()
            .map(|k| k.rsplit('/').next().unwrap_or_default().to_owned())
            .collect();
        let mut files = Vec::new();
        let (mut uploaded, mut bytes, mut incomplete, mut skipped) = (0usize, 0u64, false, 0usize);
        for (index, root) in self.paths.iter().enumerate() {
            let (root_path, leave_out) = (root.clone(), self.lock_file.clone());
            let walked =
                tokio::task::spawn_blocking(move || walk(&root_path, leave_out.as_deref()))
                    .await
                    .map_err(|_| unreachable_fail())?
                    .map_err(|_| {
                        fail(
                            &format!(
                                "The backed-up folder {} is missing or cannot be read.",
                                root.display()
                            ),
                            409,
                        )
                    })?;
            if walked.skipped > 0 {
                incomplete = true;
                skipped += walked.skipped;
                log(
                    "offsite.snapshot.skip",
                    vec![
                        ("reason", js_json::JValue::Str("unreadable-entries".into())),
                        ("count", js_json::JValue::Num(walked.skipped as f64)),
                    ],
                );
            }
            for abs in walked.files {
                pace.checkpoint().await;
                // A live SQLite file is read from a consistent copy, never raw: without one the
                // copy would be torn and could pass a restore test while being unusable.
                let mut snap: Option<(PathBuf, PathBuf)> = None;
                if is_sqlite(&abs) {
                    let a = abs.clone();
                    let made = tokio::task::spawn_blocking(move || sqlite_snapshot(&a))
                        .await
                        .map_err(|_| unreachable_fail())?;
                    match made {
                        Ok(pair) => snap = Some(pair),
                        Err(why) => {
                            log(
                                "offsite.snapshot.skip",
                                vec![
                                    (
                                        "reason",
                                        js_json::JValue::Str(
                                            match why {
                                                NoCopy::ScratchFull => "scratch-disk-full",
                                                NoCopy::Failed => "sqlite-snapshot-failed",
                                            }
                                            .into(),
                                        ),
                                    ),
                                    (
                                        "path",
                                        js_json::JValue::Str(
                                            abs.strip_prefix(root)
                                                .unwrap_or(&abs)
                                                .to_string_lossy()
                                                .into_owned(),
                                        ),
                                    ),
                                ],
                            );
                            incomplete = true;
                            skipped += 1;
                            continue;
                        }
                    }
                }
                let result = self
                    .store_file(
                        index,
                        root,
                        &abs,
                        snap.as_ref().map(|(c, _)| c.as_path()),
                        &mut known,
                        &mut uploaded,
                        &mut bytes,
                    )
                    .await;
                if let Some((_, dir)) = &snap {
                    let _ = std::fs::remove_dir_all(dir);
                }
                match result? {
                    Stored::Entry(entry) => files.push(entry),
                    Stored::Skipped => {
                        incomplete = true;
                        skipped += 1;
                    }
                }
            }
        }
        let time = crate::gdrive::now_ms();
        let id = fmt::snapshot_id(time, random::<4>());
        let manifest = fmt::Manifest {
            time,
            roots: self.paths.len(),
            files,
            incomplete,
        };
        let count = manifest.files.len();
        let sealed = self.seal(manifest.to_json().into_bytes()).await?;
        self.store.put(&format!("snapshots/{id}"), sealed).await?;
        log(
            "offsite.snapshot",
            vec![
                ("id", js_json::JValue::Str(id.clone())),
                ("files", js_json::JValue::Num(count as f64)),
                ("uploadedChunks", js_json::JValue::Num(uploaded as f64)),
                ("incomplete", js_json::JValue::Bool(incomplete)),
                ("skipped", js_json::JValue::Num(skipped as f64)),
            ],
        );
        Ok(BackupResult {
            id,
            time,
            files: count,
            uploaded_chunks: uploaded,
            uploaded_bytes: bytes,
            incomplete,
            skipped,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_file(
        &self,
        index: usize,
        root: &Path,
        abs: &Path,
        read_from: Option<&Path>,
        known: &mut HashSet<String>,
        uploaded: &mut usize,
        bytes: &mut u64,
    ) -> Result<Stored, Fail> {
        let skip = |why: &str| {
            log(
                "offsite.snapshot.skip",
                vec![
                    ("reason", js_json::JValue::Str(why.into())),
                    (
                        "path",
                        js_json::JValue::Str(
                            abs.strip_prefix(root)
                                .unwrap_or(abs)
                                .to_string_lossy()
                                .into_owned(),
                        ),
                    ),
                ],
            );
            Ok(Stored::Skipped)
        };
        let stat = std::fs::metadata(abs).ok();
        let source = read_from.unwrap_or(abs).to_path_buf();
        let Ok(file) = tokio::task::spawn_blocking(move || std::fs::File::open(source))
            .await
            .map_err(|_| unreachable_fail())?
        else {
            // Vanished or unreadable: skipped, and the snapshot says so.
            return skip("unreadable");
        };
        let mut st = Piecewise {
            file,
            whole: Sha256::new(),
            size: 0,
            first: true,
            done: false,
        };
        let mut chunks = Vec::new();
        while !st.done {
            let keys = Arc::clone(&self.keys);
            let (next, piece) = tokio::task::spawn_blocking(move || next_piece(st, &keys))
                .await
                .map_err(|_| unreachable_fail())?;
            st = next;
            let (plain, id) = match piece {
                Ok(Some(p)) => p,
                Ok(None) => break,
                // A file that became unreadable is skipped; its uploaded chunks are pruned by forget().
                Err(_) => return skip("read-failed"),
            };
            if !known.contains(&id) {
                let len = plain.len() as u64;
                let sealed = self.seal(plain).await?;
                self.store.put(&fmt::data_key(&id), sealed).await?;
                known.insert(id.clone());
                *uploaded += 1;
                *bytes += len;
            }
            chunks.push(id);
        }
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(abs)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let mtime = stat.as_ref().map_or(0, |m| {
            m.mtime() * 1000 + (m.mtime_nsec() + 500_000) / 1_000_000
        });
        Ok(Stored::Entry(fmt::FileEntry {
            root: index,
            path: rel,
            size: st.size,
            mode: stat.as_ref().map_or(0o600, |m| m.mode() & 0o777),
            mtime,
            sha256: hex_of(&st.whole.finalize()),
            chunks,
        }))
    }

    async fn read_manifest(&self, id: &str) -> Result<fmt::Manifest, Fail> {
        if !fmt::valid_snapshot_id(id) {
            return Err(fail("Invalid snapshot id.", 400));
        }
        let sealed = self
            .store
            .get(&format!("snapshots/{id}"))
            .await?
            .ok_or_else(|| fail("No such snapshot.", 404))?;
        let plain = self.open(sealed).await?;
        let mut manifest = fmt::Manifest::parse(&plain).map_err(format_fail)?;
        if manifest.time == 0 {
            // An older manifest without its own time: the id begins with it.
            manifest.time = fmt::snapshot_time(id).unwrap_or(0);
        }
        Ok(manifest)
    }

    /// Every snapshot object the store lists, read: the manifests that opened and the ids that did
    /// not (a transient read error, an object gone between the listing and the read, a damaged
    /// one). A name that is not a snapshot id is not a snapshot and is left out.
    async fn read_all(&self) -> Result<ReadAll, Fail> {
        let ids: Vec<String> = self
            .store
            .list("snapshots/")
            .await?
            .into_iter()
            .filter_map(|k| k.strip_prefix("snapshots/").map(str::to_owned))
            .filter(|id| fmt::valid_snapshot_id(id))
            .collect();
        let (mut ok, mut failed) = (Vec::new(), Vec::new());
        for id in ids {
            match self.read_manifest(&id).await {
                Ok(m) => ok.push((id, m)),
                Err(_) => {
                    log(
                        "offsite.snapshot.corrupt",
                        vec![("id", js_json::JValue::Str(id.clone()))],
                    );
                    failed.push(id);
                }
            }
        }
        Ok(ReadAll { ok, failed })
    }

    /// Every readable snapshot, newest first, and the ids that could not be read: one corrupt
    /// manifest is skipped, not fatal, but it is reported so retention and the restore test can
    /// refuse to act as if it were not there.
    pub async fn snapshots(&self) -> Result<Snapshots, Fail> {
        let all = self.read_all().await?;
        let mut list: Vec<SnapshotInfo> = all
            .ok
            .into_iter()
            .map(|(id, m)| SnapshotInfo {
                id,
                time: m.time,
                files: m.files.len(),
                bytes: m.files.iter().map(|f| f.size).sum(),
                incomplete: m.incomplete,
            })
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.time));
        Ok(Snapshots {
            list,
            failed: all.failed,
        })
    }

    /// Restore into an empty or new directory; never overwrites. Verifies every chunk and file hash.
    pub async fn restore(&self, id: &str, target: &Path) -> Result<usize, Fail> {
        let manifest = self.read_manifest(id).await?;
        if target.exists()
            && std::fs::read_dir(target)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        {
            return Err(fail("Restore needs an empty target directory.", 409));
        }
        {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(target)
                .map_err(io_fail)?;
        }
        let base = std::fs::canonicalize(target).map_err(io_fail)?;
        self.restore_manifest(&manifest, Some(&base)).await
    }

    /// Walks a manifest's files: writing each under `base`, or (None) only reading and checking.
    async fn restore_manifest(
        &self,
        manifest: &fmt::Manifest,
        base: Option<&Path>,
    ) -> Result<usize, Fail> {
        let count = manifest.files.len();
        for file in &manifest.files {
            if !fmt::safe_relative_path(&file.path) {
                return Err(fail("The snapshot names an unsafe path.", 422));
            }
            let dest = match base {
                Some(base) => {
                    let mut dest = base.join(file.root.to_string());
                    for seg in file.path.split('/') {
                        dest.push(seg);
                    }
                    if !dest.starts_with(base) {
                        return Err(fail("The snapshot names an unsafe path.", 422));
                    }
                    Some(dest)
                }
                None => None,
            };
            if file.chunks.iter().any(|c| !fmt::valid_chunk_id(c)) {
                return Err(fail("The snapshot names an invalid chunk.", 422));
            }
            self.restore_file(file, dest.as_deref()).await?;
        }
        Ok(count)
    }

    async fn restore_file(&self, file: &fmt::FileEntry, dest: Option<&Path>) -> Result<(), Fail> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        let rel = file.path.clone();
        let mut out = match dest {
            Some(dest) => {
                if let Some(dir) = dest.parent() {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(dir)
                        .map_err(io_fail)?;
                }
                Some(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(if file.mode == 0 { 0o600 } else { file.mode })
                        .open(dest)
                        .map_err(io_fail)?,
                )
            }
            None => None,
        };
        let result: Result<(), Fail> = async {
            let mut whole = Sha256::new();
            let mut size = 0u64;
            for cid in &file.chunks {
                let sealed = self.store.get(&fmt::data_key(cid)).await?.ok_or_else(|| {
                    fail(
                        &format!("A chunk of {rel} is missing from the destination."),
                        422,
                    )
                })?;
                let plain = self.open(sealed).await?;
                if self.keys.chunk_id(&plain) != *cid {
                    return Err(fail(
                        &format!("A chunk of {rel} does not match its id."),
                        422,
                    ));
                }
                whole.update(plain.as_slice());
                size += plain.len() as u64;
                if size > file.size {
                    return Err(fail(&format!("{rel} does not match the snapshot."), 422));
                }
                if let Some(out) = out.as_mut() {
                    out.write_all(&plain).map_err(io_fail)?;
                }
            }
            if hex_of(&whole.finalize()) != file.sha256 || size != file.size {
                return Err(fail(&format!("{rel} does not match the snapshot."), 422));
            }
            Ok(())
        }
        .await;
        drop(out);
        if result.is_err() {
            if let Some(dest) = dest {
                let _ = std::fs::remove_file(dest);
            }
        }
        result
    }

    /// Restore test: the newest listed snapshot is read back chunk by chunk and every chunk id, file
    /// hash and size checked, without writing the files anywhere (the old test restored into the
    /// system temp directory, which filled it and left directories behind when killed). When the
    /// newest listed snapshot cannot be read the test fails: passing an older one would report a
    /// healthy backup while the latest one is damaged.
    pub async fn verify(&self) -> Result<(String, usize, i64), Fail> {
        let all = self.read_all().await?;
        let time_of = |id: &str, m: Option<&fmt::Manifest>| {
            m.map_or_else(|| fmt::snapshot_time(id).unwrap_or(0), |m| m.time)
        };
        let newest_ok = all
            .ok
            .iter()
            .max_by_key(|(id, m)| (time_of(id, Some(m)), id.clone()));
        let newest_failed = all
            .failed
            .iter()
            .max_by_key(|id| (time_of(id, None), (*id).clone()));
        let Some((id, manifest)) = newest_ok else {
            return Err(if newest_failed.is_some() {
                fail(
                    "The newest snapshot cannot be read, so the restore test cannot pass.",
                    422,
                )
            } else {
                fail("There is no snapshot to verify yet.", 409)
            });
        };
        if let Some(bad) = newest_failed {
            if (time_of(bad, None), bad.clone()) > (time_of(id, Some(manifest)), id.clone()) {
                return Err(fail(
                    "The newest snapshot cannot be read, so the restore test cannot pass.",
                    422,
                ));
            }
        }
        let files = self.restore_manifest(manifest, None).await?;
        log(
            "offsite.verify",
            vec![
                ("id", js_json::JValue::Str(id.clone())),
                ("files", js_json::JValue::Num(files as f64)),
                (
                    "unreadableOlder",
                    js_json::JValue::Num(all.failed.len() as f64),
                ),
            ],
        );
        Ok((id.clone(), files, crate::gdrive::now_ms()))
    }

    /// Apply retention, then delete chunks no remaining snapshot references. `must_keep` are the
    /// ids that must survive whatever retention says (the snapshot the caller just wrote).
    pub async fn forget(&self, must_keep: &[String]) -> Result<Retained, Fail> {
        if self.is_running() {
            return Err(fail(
                "A backup is running; apply retention after it finishes.",
                409,
            ));
        }
        self.sweep().await;
        let ReadAll { ok, mut failed } = self.read_all().await?;
        let listed: Vec<(String, i64, bool)> = ok
            .iter()
            .map(|(id, m)| (id.clone(), m.time, !m.incomplete))
            .collect();
        let mut keep = fmt::retain_complete(&listed, fmt::RETENTION);
        keep.extend(must_keep.iter().cloned());
        let mut removed = Vec::new();
        let mut kept: Vec<(String, fmt::Manifest)> = Vec::new();
        for (id, m) in ok {
            if keep.contains(&id) {
                kept.push((id, m));
            } else {
                self.store.delete(&format!("snapshots/{id}")).await?;
                removed.push(id);
            }
        }
        // The snapshot just written is kept and its chunks are referenced even when the listing did
        // not show it yet (a store that lists late); if it cannot be read, nothing is pruned.
        for id in must_keep {
            if !kept.iter().any(|(k, _)| k == id) {
                match self.read_manifest(id).await {
                    Ok(m) => kept.push((id.clone(), m)),
                    Err(_) => failed.push(id.clone()),
                }
            }
        }
        let (mut pruned, mut young) = (0usize, 0usize);
        if failed.is_empty() {
            let referenced: HashSet<&str> = kept
                .iter()
                .flat_map(|(_, m)| m.files.iter())
                .flat_map(|f| f.chunks.iter().map(String::as_str))
                .collect();
            let now = crate::gdrive::now_ms();
            for (k, modified) in self.store.list_aged("data/").await? {
                if referenced.contains(k.rsplit('/').next().unwrap_or_default()) {
                    continue;
                }
                // Only a chunk known to be a day old is garbage; an undated one is kept.
                if !modified.is_some_and(|t| now - t >= CHUNK_GRACE_MS) {
                    young += 1;
                    continue;
                }
                self.store.delete(&k).await?;
                pruned += 1;
            }
        } else {
            log(
                "offsite.forget.skipped-prune",
                vec![("unreadable", js_json::JValue::Num(failed.len() as f64))],
            );
        }
        log(
            "offsite.forget",
            vec![
                ("removed", js_json::JValue::Num(removed.len() as f64)),
                ("prunedChunks", js_json::JValue::Num(pruned as f64)),
                ("youngChunks", js_json::JValue::Num(young as f64)),
                ("unreadable", js_json::JValue::Num(failed.len() as f64)),
            ],
        );
        Ok(Retained {
            kept: keep.len(),
            removed,
            pruned_chunks: pruned,
            young_chunks: young,
            unreadable: failed.len(),
        })
    }
}

enum Stored {
    Entry(fmt::FileEntry),
    Skipped,
}
