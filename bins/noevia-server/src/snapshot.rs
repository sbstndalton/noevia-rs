//! Snapshots of Node-live state that Node publishes for the front (sbstndalton/noevia#2190 close-out):
//! `rag-status.json` (`GET /api/health`) and `auto-roles-snapshot.json` (`GET /api/auto-roles`).
//!
//! The file is the only channel: Node writes it atomically (a temporary file, fsync, rename, mode
//! 0600; core `published-snapshot.cjs`) on a timer and whenever the state changes, and the front reads
//! it on every request. A reader never trusts what it cannot prove current. The file carries
//! `{ version, kind, writtenAt, everyMs, ... }`; it is **unusable** when it is missing, is not a
//! regular file, does not parse, is of another version or kind, has no `writtenAt` or `everyMs`, was
//! written more than two intervals ago ([`STALE_AFTER_INTERVALS`], so one missed tick is not an outage
//! but a stopped Node is) or is dated in the future. An unusable snapshot makes the caller answer
//! "unavailable", never an old value.
//!
//! **Publishing is demand-driven** (review of the close-out, M1): Node's publishers probe the
//! embedder and the model engines, which an idle installation must not have probed around the
//! clock. On every signed-in `GET /api/health` and `GET /api/auto-roles` the front touches a demand
//! marker ([`DEMAND_FILE`] in the data directory, mode 0600, replaced atomically, rewritten at most
//! every [`TOUCH_EVERY_MS`]); Node publishes only while the marker is younger than
//! [`DEMAND_WINDOW_MS`]. A request that finds the marker idle (missing or older than the window) and
//! the snapshot unusable waits up to [`WAIT`] for a fresh one, in a task of its own that stops when
//! the request does ([`Reading`]); when none arrives the answer is the unusable-snapshot one, as
//! always. A snapshot that is unusable while the marker is live (Node is down) is answered at once.

use serde_json::Value;
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// The only format this reader knows (core `published-snapshot.cjs` `VERSION`).
pub const VERSION: u64 = 1;
/// A snapshot older than this many publishing intervals is stale.
pub const STALE_AFTER_INTERVALS: i64 = 2;
/// The interval a snapshot may declare: 1 s to 10 minutes. Outside it the file is corrupt.
const EVERY_MS: std::ops::RangeInclusive<i64> = 1_000..=600_000;
/// A clock a little ahead of Node's is not a future file.
const SKEW_MS: i64 = 5_000;
/// A snapshot is a few KiB; anything bigger is not one.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The demand marker, in the data directory (core `published-snapshot.cjs` `DEMAND_FILE`).
pub const DEMAND_FILE: &str = "snapshot-demand";
/// Node publishes while the marker is younger than this (core `DEMAND_WINDOW_MS`).
pub const DEMAND_WINDOW_MS: i64 = 60_000;
/// A marker younger than this is not rewritten: the touch of a busy page stays a `stat`.
pub const TOUCH_EVERY_MS: i64 = 5_000;
/// How long a request waits for Node to publish after the marker was idle.
pub const WAIT: Duration = Duration::from_secs(2);
/// How often the waiting request looks again.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// The files Node publishes for the front and the marker: state of the running process that never
/// belongs in a backup (the off-site walk leaves them out; so it does their `<name>.<pid>.<hex>.tmp`).
pub const PUBLISHED_STATE: [&str; 3] = ["rag-status.json", "auto-roles-snapshot.json", DEMAND_FILE];

/// Whether `name` is one of [`PUBLISHED_STATE`] or a temporary file of one.
pub fn is_published_state(name: &str) -> bool {
    PUBLISHED_STATE.iter().any(|f| {
        name == *f
            || (name.ends_with(".tmp")
                && name
                    .strip_prefix(f)
                    .is_some_and(|rest| rest.starts_with('.')))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    Missing,
    Corrupt,
    Stale,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// The parsed file (serde_json sorts keys: use [`Snapshot::raw`] where order is the answer's).
    pub body: Value,
    /// The checked file's bytes, for a reader that must keep Node's key order (js_json).
    pub raw: Vec<u8>,
    pub written_at: i64,
    pub every_ms: i64,
}

/// Reads and checks `file`, which must hold a snapshot of `kind`, at `now_ms`.
pub fn read(file: &Path, kind: &str, now_ms: i64) -> Result<Snapshot, Unusable> {
    let meta = match std::fs::symlink_metadata(file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Unusable::Missing),
        Err(_) => return Err(Unusable::Corrupt),
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(Unusable::Corrupt);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| Unusable::Corrupt)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Unusable::Corrupt);
    }
    parse(&bytes, kind, now_ms)
}

fn parse(bytes: &[u8], kind: &str, now_ms: i64) -> Result<Snapshot, Unusable> {
    let body: Value = serde_json::from_slice(bytes).map_err(|_| Unusable::Corrupt)?;
    let Some(object) = body.as_object() else {
        return Err(Unusable::Corrupt);
    };
    if object.get("version").and_then(Value::as_u64) != Some(VERSION)
        || object.get("kind").and_then(Value::as_str) != Some(kind)
    {
        return Err(Unusable::Corrupt);
    }
    let int = |key: &str| object.get(key).and_then(Value::as_i64);
    let (Some(written_at), Some(every_ms)) = (int("writtenAt"), int("everyMs")) else {
        return Err(Unusable::Corrupt);
    };
    if written_at <= 0 || !EVERY_MS.contains(&every_ms) {
        return Err(Unusable::Corrupt);
    }
    let age = now_ms.saturating_sub(written_at);
    if age < -SKEW_MS || age > every_ms.saturating_mul(STALE_AFTER_INTERVALS) {
        return Err(Unusable::Stale);
    }
    Ok(Snapshot {
        body,
        raw: bytes.to_vec(),
        written_at,
        every_ms,
    })
}

/// A snapshot read running in a task of its own. Awaiting it is cancel-safe: dropping it (a client
/// that left) stops the task, so nothing outlives the request.
pub struct Reading<T>(tokio::task::JoinHandle<Option<T>>);

impl<T> Future for Reading<T> {
    type Output = Option<T>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|joined| joined.ok().flatten())
    }
}

impl<T> Drop for Reading<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The marker's age in ms at `now_ms`, or `None` when there is no marker (missing, or not a plain
/// file). Negative: the marker is dated in the future.
fn marker_age(file: &Path, now_ms: i64) -> Option<i64> {
    let meta = std::fs::symlink_metadata(file).ok()?;
    if !meta.is_file() {
        return None;
    }
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(now_ms.saturating_sub(i64::try_from(since.as_millis()).ok()?))
}

/// Whether a marker of this age says Node should be publishing (core: `-skew <= age < window`, with
/// the same few seconds of clock skew as a snapshot's).
fn live(age: Option<i64>) -> bool {
    age.is_some_and(|a| (-SKEW_MS..DEMAND_WINDOW_MS).contains(&a))
}

/// The demand a running front puts on Node's publishers. One per [`App`](crate::App).
pub struct Demand {
    wait: Duration,
    /// Until when the front itself is waiting for Node to start publishing: a touch that found the
    /// marker idle opens it, and every reader that finds its snapshot unusable inside it waits too
    /// (the marker is live by then, so only this tells the second request that Node is waking).
    waiting_until: Mutex<Option<Instant>>,
}

impl Default for Demand {
    fn default() -> Self {
        Demand::with_wait(WAIT)
    }
}

impl Demand {
    pub fn with_wait(wait: Duration) -> Self {
        Demand {
            wait,
            waiting_until: Mutex::new(None),
        }
    }

    /// Marks that a signed-in request wants fresh snapshots. Returns whether the marker was idle
    /// (publishers likely stopped), which opens the wait. A marker that cannot be written reports
    /// `false`: nobody is going to publish, so nobody is waited for.
    pub fn touch(&self, dir: &Path, now_ms: i64) -> bool {
        let age = marker_age(&dir.join(DEMAND_FILE), now_ms);
        let idle = !live(age);
        // Rewritten once it is a few seconds old; a recent one is only looked at.
        let recent = age.is_some_and(|a| (0..TOUCH_EVERY_MS).contains(&a));
        if !recent && write_marker(dir, now_ms).is_err() {
            return false;
        }
        if idle {
            if let Ok(mut until) = self.waiting_until.lock() {
                *until = Some(Instant::now() + self.wait);
            }
        }
        idle
    }

    /// How long a reader may still wait (`None`: not at all).
    fn remaining(&self) -> Option<Duration> {
        let until = (*self.waiting_until.lock().ok()?)?;
        until.checked_duration_since(Instant::now())
    }

    /// Touches the marker and reads the snapshot with `read(now_ms)`; while `read` finds nothing
    /// usable and the front is waiting for Node to wake, looks again until the wait ends. The answer
    /// is `None` when no usable snapshot came: the caller reports "unavailable", never an old value.
    pub fn read<T, F>(self: &std::sync::Arc<Self>, dir: PathBuf, read: F) -> Reading<T>
    where
        T: Send + 'static,
        F: Fn(i64) -> Option<T> + Send + Sync + 'static,
    {
        let me = std::sync::Arc::clone(self);
        let read = std::sync::Arc::new(read);
        Reading(tokio::spawn(async move {
            let first = {
                let (me, read) = (std::sync::Arc::clone(&me), std::sync::Arc::clone(&read));
                tokio::task::spawn_blocking(move || {
                    me.touch(&dir, crate::identity::now_ms());
                    read(crate::identity::now_ms())
                })
                .await
                .ok()?
            };
            if first.is_some() {
                return first;
            }
            while me.remaining().is_some() {
                tokio::time::sleep(LOOK_EVERY).await;
                let read = std::sync::Arc::clone(&read);
                let found = tokio::task::spawn_blocking(move || read(crate::identity::now_ms()))
                    .await
                    .ok()?;
                if found.is_some() {
                    return found;
                }
            }
            None
        }))
    }
}

/// Replaces the marker atomically: a temporary file of the same directory (mode 0600), renamed over
/// the old one, so Node sees the old marker or the new one and never a half-written file.
fn write_marker(dir: &Path, now_ms: i64) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        "{DEMAND_FILE}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(now_ms.to_string().as_bytes())?;
        std::fs::rename(&tmp, dir.join(DEMAND_FILE))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn file(dir: &Path, text: &str) -> std::path::PathBuf {
        let path = dir.join("snap.json");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn good(extra: &str) -> String {
        format!(r#"{{"version":1,"kind":"k","writtenAt":{NOW},"everyMs":10000{extra}}}"#)
    }

    #[test]
    fn a_fresh_snapshot_of_the_kind_is_read() {
        let d = tempfile::tempdir().unwrap();
        let s = read(&file(d.path(), &good(r#","x":1"#)), "k", NOW + 4_000).unwrap();
        assert_eq!(s.body["x"], 1);
        assert_eq!((s.written_at, s.every_ms), (NOW, 10_000));
        // Two intervals old is still current; a hair past is stale.
        assert!(read(&file(d.path(), &good("")), "k", NOW + 20_000).is_ok());
        assert_eq!(
            read(&file(d.path(), &good("")), "k", NOW + 20_001),
            Err(Unusable::Stale)
        );
    }

    #[test]
    fn nothing_unprovable_is_usable() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            read(&d.path().join("none.json"), "k", NOW),
            Err(Unusable::Missing)
        );
        for (name, text) in [
            ("empty", String::new()),
            ("garbage", "{not json".to_owned()),
            ("array", "[]".to_owned()),
            (
                "version",
                good("").replace(r#""version":1"#, r#""version":2"#),
            ),
            ("no version", good("").replace(r#""version":1,"#, "")),
            (
                "kind",
                good("").replace(r#""kind":"k""#, r#""kind":"other""#),
            ),
            ("no writtenAt", good("").replace("writtenAt", "wroteAt")),
            ("float writtenAt", good("").replace(&NOW.to_string(), "1.5")),
            ("zero writtenAt", good("").replace(&NOW.to_string(), "0")),
            ("no everyMs", good("").replace("everyMs", "every")),
            ("tiny everyMs", good("").replace("10000", "10")),
            ("huge everyMs", good("").replace("10000", "99999999999")),
            (
                "text writtenAt",
                good("").replace(&NOW.to_string(), "\"now\""),
            ),
        ] {
            assert_eq!(
                read(&file(d.path(), &text), "k", NOW + 1_000),
                Err(Unusable::Corrupt),
                "{name}"
            );
        }
    }

    #[test]
    fn a_future_file_and_a_symlink_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = file(d.path(), &good(""));
        assert_eq!(read(&p, "k", NOW - 5_001), Err(Unusable::Stale));
        assert!(read(&p, "k", NOW - 5_000).is_ok());
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert_eq!(read(&link, "k", NOW), Err(Unusable::Corrupt));
        let big = d.path().join("big.json");
        std::fs::write(&big, vec![b' '; (MAX_BYTES + 1) as usize]).unwrap();
        assert_eq!(read(&big, "k", NOW), Err(Unusable::Corrupt));
        assert_eq!(read(d.path(), "k", NOW), Err(Unusable::Corrupt));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod demand_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    fn age_marker(dir: &Path, ms: u64) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(DEMAND_FILE))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_millis(ms))
            .unwrap();
    }

    fn marker_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_first_touch_finds_it_idle_and_leaves_a_private_marker_and_nothing_else() {
        let d = tempfile::tempdir().unwrap();
        let demand = Demand::default();
        assert!(demand.touch(d.path(), crate::identity::now_ms()));
        assert_eq!(marker_files(d.path()), [DEMAND_FILE]);
        let mode = std::fs::metadata(d.path().join(DEMAND_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // A second touch right away finds it live and does not rewrite it (the file keeps its mtime).
        let before = std::fs::metadata(d.path().join(DEMAND_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert!(!demand.touch(d.path(), crate::identity::now_ms()));
        let after = std::fs::metadata(d.path().join(DEMAND_FILE))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_marker_is_idle_only_from_the_window_and_a_touch_renews_it() {
        let d = tempfile::tempdir().unwrap();
        let demand = Demand::default();
        assert!(demand.touch(d.path(), crate::identity::now_ms()));
        age_marker(d.path(), 30_000);
        // Live (the window is 60 s), but old enough to be rewritten.
        assert!(!demand.touch(d.path(), crate::identity::now_ms()));
        assert!(
            marker_age(&d.path().join(DEMAND_FILE), crate::identity::now_ms()).unwrap() < 5_000
        );
        age_marker(d.path(), 59_000);
        assert!(!demand.touch(d.path(), crate::identity::now_ms()));
        age_marker(d.path(), 61_000);
        assert!(demand.touch(d.path(), crate::identity::now_ms()));
        // A marker dated far in the future is not demand either.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(d.path().join(DEMAND_FILE))
            .unwrap();
        f.set_modified(std::time::SystemTime::now() + Duration::from_secs(3_600))
            .unwrap();
        assert!(demand.touch(d.path(), crate::identity::now_ms()));
        assert!(live(marker_age(
            &d.path().join(DEMAND_FILE),
            crate::identity::now_ms()
        )));
    }

    #[test]
    fn a_marker_that_is_not_a_plain_file_is_replaced_and_a_directory_that_cannot_be_written_waits_for_nothing(
    ) {
        let d = tempfile::tempdir().unwrap();
        let demand = Demand::default();
        // A symlink in the marker's place is replaced by the rename, never written through.
        let outside = d.path().join("outside");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, d.path().join(DEMAND_FILE)).unwrap();
        assert!(demand.touch(d.path(), crate::identity::now_ms()));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert!(std::fs::symlink_metadata(d.path().join(DEMAND_FILE))
            .unwrap()
            .is_file());
        // No directory: nothing is written, nobody is told to wait.
        let none = Demand::default();
        assert!(!none.touch(&d.path().join("no-such-dir"), crate::identity::now_ms()));
        assert!(none.remaining().is_none());
    }

    #[test]
    fn the_published_files_and_their_temporaries_are_the_ones_a_backup_leaves_out() {
        for name in [
            "rag-status.json",
            "auto-roles-snapshot.json",
            "snapshot-demand",
            "rag-status.json.123.deadbeef.tmp",
            "auto-roles-snapshot.json.9.00ff.tmp",
            "snapshot-demand.4242.7.tmp",
        ] {
            assert!(is_published_state(name), "{name}");
        }
        for name in [
            "rag-status.json.bak",
            "rag-status.jsonx.tmp",
            "my-rag-status.json",
            "snapshot-demand2",
            "cowork.db",
            "auto-roles.json",
            ".tmp",
        ] {
            assert!(!is_published_state(name), "{name}");
        }
    }

    fn good(written_at: i64) -> String {
        format!(r#"{{"version":1,"kind":"k","writtenAt":{written_at},"everyMs":10000}}"#)
    }

    #[tokio::test]
    async fn a_request_after_idle_waits_for_a_fresh_snapshot_and_then_answers_with_it() {
        let d = tempfile::tempdir().unwrap();
        let demand = Arc::new(Demand::with_wait(Duration::from_millis(1_500)));
        let file = d.path().join("snap.json");
        // The old file is stale (a Node that stopped publishing); a new one lands 300 ms in.
        std::fs::write(&file, good(crate::identity::now_ms() - 120_000)).unwrap();
        let writer = {
            let file = file.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::fs::write(&file, good(crate::identity::now_ms())).unwrap();
            })
        };
        let started = Instant::now();
        let read_from = file.clone();
        let got = demand
            .read(d.path().to_path_buf(), move |now| {
                read(&read_from, "k", now).ok()
            })
            .await;
        writer.await.unwrap();
        let snap = got.expect("the fresh snapshot");
        assert!(snap.written_at > crate::identity::now_ms() - 5_000);
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(started.elapsed() < Duration::from_millis(1_400));
        assert!(live(marker_age(
            &d.path().join(DEMAND_FILE),
            crate::identity::now_ms()
        )));
    }

    #[tokio::test]
    async fn no_snapshot_in_the_wait_is_none_and_an_old_one_is_never_returned() {
        let d = tempfile::tempdir().unwrap();
        let demand = Arc::new(Demand::with_wait(Duration::from_millis(400)));
        let file = d.path().join("snap.json");
        std::fs::write(&file, good(crate::identity::now_ms() - 120_000)).unwrap();
        let started = Instant::now();
        let read_from = file.clone();
        let got = demand
            .read(d.path().to_path_buf(), move |now| {
                read(&read_from, "k", now).ok()
            })
            .await;
        assert!(got.is_none());
        assert!(started.elapsed() >= Duration::from_millis(350));
        assert!(started.elapsed() < Duration::from_millis(1_500));
    }

    #[tokio::test]
    async fn a_live_marker_means_no_wait_and_a_second_request_in_the_wait_shares_it() {
        let d = tempfile::tempdir().unwrap();
        let demand = Arc::new(Demand::with_wait(Duration::from_millis(600)));
        let file = d.path().join("snap.json");
        let ask = |demand: &Arc<Demand>| {
            let file = file.clone();
            demand.read(d.path().to_path_buf(), move |now| {
                read(&file, "k", now).ok()
            })
        };
        // The first request finds the marker idle and waits out its 600 ms.
        let first = ask(&demand);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The second finds the marker live (the first touched it) but joins the wait that is open.
        let started = Instant::now();
        assert!(ask(&demand).await.is_none());
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "{:?}",
            started.elapsed()
        );
        assert!(first.await.is_none());
        // Once the wait has closed and the marker is live (Node is down), the answer is immediate.
        let started = Instant::now();
        assert!(ask(&demand).await.is_none());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_request_that_goes_away_stops_its_reader() {
        let d = tempfile::tempdir().unwrap();
        let demand = Arc::new(Demand::with_wait(Duration::from_secs(30)));
        // The reader's closure holds the token: it is the only thing left holding it once the task is gone.
        let token = Arc::new(());
        let held = Arc::clone(&token);
        let file = d.path().join("none.json");
        let reading = demand.read(d.path().to_path_buf(), move |now| {
            let _ = &held;
            read(&file, "k", now).ok()
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(Arc::strong_count(&token), 2, "the reader is running");
        drop(reading);
        let started = Instant::now();
        while Arc::strong_count(&token) > 1 {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the reader outlived its request (it would wait 30 s)"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
