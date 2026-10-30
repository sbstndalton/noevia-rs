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
//!   copy can be made the file is skipped and the manifest flagged `incomplete`, never raw-read.
//!   A file that vanishes or cannot be read is skipped; a store failure ends the run.
//! - `forget`: retention (7 daily, 4 weekly, 6 monthly, always the newest), then the chunks no kept
//!   snapshot references. Refused while a backup runs (its uploaded chunks have no manifest yet).
//! - `verify`: restores the newest snapshot into a fresh 0700 scratch directory, checking every
//!   chunk id, every file hash and size, then deletes the scratch directory.
//!
//! A scheduled run takes a [`Pace`]: before every file and every object it waits out the quiet
//! windows (offsite_backup_format::schedule); an administrator's own run has none.
use crate::gdrive::{hex_of, random, Fail};
use crate::offsite_store::Store;
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

    pub fn scheduled(zone: fmt::schedule::Zone) -> Pace {
        Pace {
            zone: Some(zone),
            ..Pace::none()
        }
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
}

pub struct Retained {
    pub kept: usize,
    pub removed: Vec<String>,
    pub pruned_chunks: usize,
}

pub struct SnapshotInfo {
    pub id: String,
    pub time: i64,
    pub files: usize,
    pub bytes: u64,
    pub incomplete: bool,
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
    }
}

pub struct Engine {
    store: Store,
    keys: Arc<fmt::Keys>,
    paths: Vec<PathBuf>,
    running: AtomicBool,
}

struct Running<'a>(&'a AtomicBool);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// `/\.(db|sqlite3?)$/`.
fn is_sqlite(path: &Path) -> bool {
    let name = path.to_string_lossy();
    [".db", ".sqlite", ".sqlite3"]
        .iter()
        .any(|e| name.ends_with(e))
}

/// Every regular file under `root`, directories in name order, symlinks and `-wal`/`-shm`/
/// `-journal` files left out.
fn walk(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = read.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                visit(&path, out);
            } else if kind.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !["-wal", "-shm", "-journal"]
                    .iter()
                    .any(|s| name.ends_with(s))
                {
                    out.push(path);
                }
            }
        }
    }
    let mut out = Vec::new();
    visit(root, &mut out);
    out
}

/// A consistent on-disk copy of a live SQLite database; `None` when one cannot be made.
fn sqlite_snapshot(abs: &Path) -> Option<(PathBuf, PathBuf)> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = std::env::temp_dir().join(format!("noevia-sqlite-{}", hex_of(&random::<8>())));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
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
        Ok(()) => Some((copy, dir)),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&dir);
            None
        }
    }
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

impl Engine {
    pub fn new(store: Store, key: &[u8], paths: Vec<PathBuf>) -> Result<Engine, Fail> {
        Ok(Engine {
            store,
            keys: Arc::new(fmt::Keys::derive(key).map_err(format_fail)?),
            paths,
            running: AtomicBool::new(false),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn seal(&self, plain: Vec<u8>) -> Vec<u8> {
        let keys = Arc::clone(&self.keys);
        tokio::task::spawn_blocking(move || keys.seal(&plain, &random::<12>()))
            .await
            .unwrap_or_default()
    }

    async fn open(&self, sealed: Vec<u8>) -> Result<zeroize::Zeroizing<Vec<u8>>, Fail> {
        let keys = Arc::clone(&self.keys);
        tokio::task::spawn_blocking(move || keys.open(&sealed))
            .await
            .map_err(|_| fail("The backup destination could not be reached.", 502))?
            .map_err(format_fail)
    }

    async fn ensure_config(&self, now: i64) -> Result<(), Fail> {
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
        let sealed = self.seal(fmt::config_json(now).into_bytes()).await;
        self.store.put("config", sealed).await
    }

    pub async fn backup(&self, pace: &Pace) -> Result<BackupResult, Fail> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err(fail("A backup is already running.", 409));
        }
        let _running = Running(&self.running);
        self.ensure_config(crate::gdrive::now_ms()).await?;
        let mut known: HashSet<String> = self
            .store
            .list("data/")
            .await?
            .into_iter()
            .map(|k| k.rsplit('/').next().unwrap_or_default().to_owned())
            .collect();
        let mut files = Vec::new();
        let (mut uploaded, mut bytes, mut incomplete) = (0usize, 0u64, false);
        for (index, root) in self.paths.iter().enumerate() {
            let root_path = root.clone();
            let list = tokio::task::spawn_blocking(move || walk(&root_path))
                .await
                .unwrap_or_default();
            for abs in list {
                pace.checkpoint().await;
                // A live SQLite file is read from a consistent copy, never raw: without one the
                // copy would be torn and could pass a restore test while being unusable.
                let mut snap: Option<(PathBuf, PathBuf)> = None;
                if is_sqlite(&abs) {
                    let a = abs.clone();
                    snap = tokio::task::spawn_blocking(move || sqlite_snapshot(&a))
                        .await
                        .ok()
                        .flatten();
                    if snap.is_none() {
                        log(
                            "offsite.snapshot.skip",
                            vec![(
                                "reason",
                                js_json::JValue::Str("sqlite-snapshot-failed".into()),
                            )],
                        );
                        incomplete = true;
                        continue;
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
                        pace,
                    )
                    .await;
                if let Some((_, dir)) = &snap {
                    let _ = std::fs::remove_dir_all(dir);
                }
                match result? {
                    Stored::Entry(entry) => files.push(entry),
                    Stored::Skipped => {
                        if snap.is_some() {
                            incomplete = true;
                        }
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
        let sealed = self.seal(manifest.to_json().into_bytes()).await;
        self.store.put(&format!("snapshots/{id}"), sealed).await?;
        log(
            "offsite.snapshot",
            vec![
                ("id", js_json::JValue::Str(id.clone())),
                ("files", js_json::JValue::Num(count as f64)),
                ("uploadedChunks", js_json::JValue::Num(uploaded as f64)),
            ],
        );
        Ok(BackupResult {
            id,
            time,
            files: count,
            uploaded_chunks: uploaded,
            uploaded_bytes: bytes,
            incomplete,
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
        pace: &Pace,
    ) -> Result<Stored, Fail> {
        let stat = std::fs::metadata(abs).ok();
        let source = read_from.unwrap_or(abs).to_path_buf();
        let Ok(file) = tokio::task::spawn_blocking(move || std::fs::File::open(source))
            .await
            .map_err(|_| fail("The backup destination could not be reached.", 502))?
        else {
            // Vanished or unreadable: skipped.
            return Ok(Stored::Skipped);
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
                .map_err(|_| fail("The backup destination could not be reached.", 502))?;
            st = next;
            let (plain, id) = match piece {
                Ok(Some(p)) => p,
                Ok(None) => break,
                // A file that became unreadable is skipped; its uploaded chunks are pruned by forget().
                Err(_) => return Ok(Stored::Skipped),
            };
            if !known.contains(&id) {
                pace.checkpoint().await;
                let len = plain.len() as u64;
                let sealed = self.seal(plain).await;
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
        fmt::Manifest::parse(&plain).map_err(format_fail)
    }

    /// Every readable snapshot, newest first. One corrupt manifest is skipped, not fatal.
    pub async fn snapshots(&self) -> Result<Vec<SnapshotInfo>, Fail> {
        let ids: Vec<String> = self
            .store
            .list("snapshots/")
            .await?
            .into_iter()
            .filter_map(|k| k.strip_prefix("snapshots/").map(str::to_owned))
            .collect();
        let mut out = Vec::new();
        for id in ids {
            match self.read_manifest(&id).await {
                Ok(m) => out.push(SnapshotInfo {
                    id,
                    time: m.time,
                    files: m.files.len(),
                    bytes: m.files.iter().map(|f| f.size).sum(),
                    incomplete: m.incomplete,
                }),
                Err(_) => log(
                    "offsite.snapshot.corrupt",
                    vec![("id", js_json::JValue::Str(id))],
                ),
            }
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.time));
        Ok(out)
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
                .map_err(|_| fail("The backup destination could not be reached.", 502))?;
        }
        let base = std::fs::canonicalize(target)
            .map_err(|_| fail("The backup destination could not be reached.", 502))?;
        let count = manifest.files.len();
        for file in &manifest.files {
            if !fmt::safe_relative_path(&file.path) {
                return Err(fail("The snapshot names an unsafe path.", 422));
            }
            let mut dest = base.join(file.root.to_string());
            for seg in file.path.split('/') {
                dest.push(seg);
            }
            if !dest.starts_with(&base) {
                return Err(fail("The snapshot names an unsafe path.", 422));
            }
            if file.chunks.iter().any(|c| !fmt::valid_chunk_id(c)) {
                return Err(fail("The snapshot names an invalid chunk.", 422));
            }
            self.restore_file(file, &dest).await?;
        }
        Ok(count)
    }

    async fn restore_file(&self, file: &fmt::FileEntry, dest: &Path) -> Result<(), Fail> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        let rel = file.path.clone();
        let io = |_: std::io::Error| fail("The backup destination could not be reached.", 502);
        if let Some(dir) = dest.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(io)?;
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if file.mode == 0 { 0o600 } else { file.mode })
            .open(dest)
            .map_err(io)?;
        let mut ok = false;
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
                out.write_all(&plain).map_err(io)?;
            }
            if hex_of(&whole.finalize()) != file.sha256 || size != file.size {
                return Err(fail(&format!("{rel} does not match the snapshot."), 422));
            }
            Ok(())
        }
        .await;
        if result.is_ok() {
            ok = true;
        }
        drop(out);
        if !ok {
            let _ = std::fs::remove_file(dest);
        }
        result
    }

    /// Restore test: the newest snapshot into a scratch directory, verified, then deleted.
    pub async fn verify(&self, scratch_root: &Path) -> Result<(String, usize, i64), Fail> {
        let latest = self
            .snapshots()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| fail("There is no snapshot to verify yet.", 409))?;
        // A fresh 0700 directory only this process can write into; the restore goes into a new
        // child of it, so no other local user can plant a link at a guessable path (#140).
        let scratch = {
            use std::os::unix::fs::DirBuilderExt as _;
            let p = scratch_root.join(format!("noevia-restore-test-{}", hex_of(&random::<8>())));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&p)
                .map_err(|_| fail("The backup destination could not be reached.", 502))?;
            p
        };
        let result = self.restore(&latest.id, &scratch.join("restore")).await;
        let _ = std::fs::remove_dir_all(&scratch);
        let files = result?;
        log(
            "offsite.verify",
            vec![
                ("id", js_json::JValue::Str(latest.id.clone())),
                ("files", js_json::JValue::Num(files as f64)),
            ],
        );
        Ok((latest.id, files, crate::gdrive::now_ms()))
    }

    /// Apply retention, then delete chunks no remaining snapshot references.
    pub async fn forget(&self) -> Result<Retained, Fail> {
        if self.is_running() {
            return Err(fail(
                "A backup is running; apply retention after it finishes.",
                409,
            ));
        }
        let all = self.snapshots().await?;
        let listed: Vec<(String, i64)> = all.iter().map(|s| (s.id.clone(), s.time)).collect();
        let keep = fmt::retain(&listed, fmt::RETENTION);
        let mut removed = Vec::new();
        for s in &all {
            if !keep.contains(&s.id) {
                self.store.delete(&format!("snapshots/{}", s.id)).await?;
                removed.push(s.id.clone());
            }
        }
        let mut referenced: HashSet<String> = HashSet::new();
        for id in &keep {
            for f in self.read_manifest(id).await?.files {
                referenced.extend(f.chunks);
            }
        }
        let mut pruned = 0;
        for k in self.store.list("data/").await? {
            if !referenced.contains(k.rsplit('/').next().unwrap_or_default()) {
                self.store.delete(&k).await?;
                pruned += 1;
            }
        }
        log(
            "offsite.forget",
            vec![
                ("removed", js_json::JValue::Num(removed.len() as f64)),
                ("prunedChunks", js_json::JValue::Num(pruned as f64)),
            ],
        );
        Ok(Retained {
            kept: keep.len(),
            removed,
            pruned_chunks: pruned,
        })
    }
}

enum Stored {
    Entry(fmt::FileEntry),
    Skipped,
}
