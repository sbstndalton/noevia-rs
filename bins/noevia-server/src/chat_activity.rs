//! Chat port S3: who is active, for Node's chat change journal reader (core chat-change-journal.cjs,
//! chat-activity.cjs). That reader tails the journals of the users active within its `ACTIVE_MS`
//! (15 minutes), and learned who they are from the requests reaching Node (its `touch` at every
//! request start). A user who only uses routes this front answers itself never reaches Node, so their
//! Diary mirror and chat brain would wait for some other request. The front sees every request:
//! the session upkeep that runs before each one (upkeep.rs) names the account it authenticated, and
//! this records it in
//!
//! `UI_DATA_DIR/chat-activity.json` = `{"version":1,"seen":{"<userId>":<epoch ms>}}`
//!
//! the user's latest authenticated `/api/` request. Node merges each entry on its 2 s tick as a
//! request start of that user (a time newer than the one it merged before).
//!
//! The file is written at once the first time a user is seen by this process (and whenever their
//! last write is [`DEBOUNCE_MS`] old), so a new user is picked up on Node's next tick, and otherwise at
//! most once per [`DEBOUNCE_MS`] by a trailing flush that carries the latest times. The file
//! therefore holds each user's last request at most [`DEBOUNCE_MS`] late, and only while that request
//! is still recent: Node's 15-minute stop is the same as for a request it saw itself. Users not seen
//! for [`KEEP_MS`] (Node's `ACTIVE_MS`) are dropped from the file. Best effort: a failed write is
//! logged without detail (at most once a minute) and retried by the next flush; it never changes a
//! request's answer.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// The file in UI_DATA_DIR (core chat-activity.cjs `ACTIVITY_FILE`).
pub const FILE: &str = "chat-activity.json";
/// The most a user's recorded time lags their latest request.
pub const DEBOUNCE_MS: i64 = 30_000;
/// Core chat-change-journal.cjs `ACTIVE_MS`: older entries mean nothing to the reader.
pub const KEEP_MS: i64 = 15 * 60 * 1000;
const WARN_EVERY_MS: i64 = 60_000;

#[derive(Default)]
struct State {
    /// The latest request per user.
    latest: HashMap<String, i64>,
    /// When each user's entry was last written.
    written: HashMap<String, i64>,
    /// A newer time than the file holds is waiting for the trailing flush.
    dirty: bool,
    flusher: bool,
    warned_at: Option<i64>,
}

pub struct Activity {
    dir: Option<PathBuf>,
    state: Mutex<State>,
    /// How often the trailing flush runs ([`DEBOUNCE_MS`]; shorter in tests).
    every: Duration,
}

impl Activity {
    pub fn new(dir: Option<PathBuf>) -> Arc<Self> {
        Self::with_flush_every(dir, Duration::from_millis(DEBOUNCE_MS.unsigned_abs()))
    }

    /// [`Activity::new`] with another trailing-flush interval (tests).
    pub fn with_flush_every(dir: Option<PathBuf>, every: Duration) -> Arc<Self> {
        Arc::new(Activity {
            dir,
            state: Mutex::new(State::default()),
            every,
        })
    }

    /// `user` made an authenticated request at `now` (blocking: may write the file).
    pub fn record(self: &Arc<Self>, user: &str, now: i64) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        if user.is_empty() {
            return;
        }
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let latest = st.latest.entry(user.to_owned()).or_insert(now);
        *latest = (*latest).max(now);
        let due = st
            .written
            .get(user)
            .is_none_or(|at| now - *at >= DEBOUNCE_MS);
        if due {
            write(dir, &mut st, now);
        } else {
            st.dirty = true;
        }
        // Waiting times, or a write that failed: the trailing flush writes them (retrying a failure)
        // even if no further request comes.
        if st.dirty {
            self.start_flusher(&mut st);
        }
    }

    fn start_flusher(self: &Arc<Self>, st: &mut State) {
        if st.flusher {
            return;
        }
        st.flusher = true;
        let weak = Arc::downgrade(self);
        let every = self.every;
        let spawned = std::thread::Builder::new()
            .name("chat-activity".into())
            .spawn(move || flush_loop(&weak, every));
        if spawned.is_err() {
            st.flusher = false;
        }
    }

    /// Writes the waiting times now, if any (the trailing flush; tests call it directly).
    pub fn flush(&self, now: i64) {
        let Some(dir) = self.dir.as_deref() else {
            return;
        };
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if st.dirty {
            write(dir, &mut st, now);
        }
    }
}

fn flush_loop(weak: &Weak<Activity>, every: Duration) {
    loop {
        std::thread::sleep(every);
        let Some(activity) = weak.upgrade() else {
            return;
        };
        activity.flush(crate::identity::now_ms());
    }
}

fn write(dir: &Path, st: &mut State, now: i64) {
    st.latest.retain(|_, at| now - *at < KEEP_MS);
    let mut users: Vec<(&String, &i64)> = st.latest.iter().collect();
    users.sort();
    let seen = serde_json::Map::from_iter(
        users
            .into_iter()
            .map(|(u, at)| (u.clone(), serde_json::Value::from(*at))),
    );
    let text = serde_json::json!({"version": 1, "seen": seen}).to_string();
    match write_private(&dir.join(FILE), &text) {
        Ok(()) => {
            st.written = st.latest.keys().map(|u| (u.clone(), now)).collect();
            st.dirty = false;
        }
        Err(_) => {
            st.dirty = true;
            if st.warned_at.is_none_or(|at| now - at >= WARN_EVERY_MS) {
                st.warned_at = Some(now);
                eprintln!("noevia-server: chat activity could not be written");
            }
        }
    }
}

fn write_private(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    out.write_all(text.as_bytes())?;
    drop(out);
    std::fs::rename(&tmp, file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn read(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).unwrap()).unwrap()
    }

    #[test]
    fn first_sight_is_written_at_once_then_the_latest_time_trails() {
        let d = tempfile::tempdir().unwrap();
        let a = Activity::new(Some(d.path().to_path_buf()));
        let t = 1_800_000_000_000;
        a.record("u1", t);
        assert_eq!(
            read(d.path()),
            serde_json::json!({"version":1,"seen":{"u1":t}})
        );
        // Within the debounce: not written yet, but waiting.
        a.record("u1", t + 1000);
        assert_eq!(read(d.path())["seen"]["u1"], t);
        // Another user's first request is written at once, with u1's latest time too.
        a.record("u2", t + 2000);
        assert_eq!(
            read(d.path()),
            serde_json::json!({"version":1,"seen":{"u1":t + 1000,"u2":t + 2000}})
        );
        a.record("u1", t + 3000);
        a.flush(t + 4000);
        assert_eq!(read(d.path())["seen"]["u1"], t + 3000);
        // Nothing waiting: a flush writes nothing.
        std::fs::remove_file(d.path().join(FILE)).unwrap();
        a.flush(t + 5000);
        assert!(!d.path().join(FILE).exists());
        // A debounce later the next request is written at once again.
        a.record("u1", t + 3000 + DEBOUNCE_MS + 4000);
        assert_eq!(read(d.path())["seen"]["u1"], t + 3000 + DEBOUNCE_MS + 4000);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join(FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn users_quiet_for_fifteen_minutes_are_dropped_and_nothing_is_written_without_a_data_dir() {
        let d = tempfile::tempdir().unwrap();
        let a = Activity::new(Some(d.path().to_path_buf()));
        let t = 1_800_000_000_000;
        a.record("old", t);
        a.record("new", t + KEEP_MS);
        assert_eq!(
            read(d.path()),
            serde_json::json!({"version":1,"seen":{"new":t + KEEP_MS}})
        );
        let none = Activity::new(None);
        none.record("u1", t);
        none.flush(t);
        a.record("", t + KEEP_MS);
        assert_eq!(read(d.path())["seen"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_first_write_is_retried_by_the_flusher_without_another_request() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("not-yet");
        let a = Activity::with_flush_every(Some(missing.clone()), Duration::from_millis(20));
        let t = crate::identity::now_ms();
        // The first sight writes at once, and fails: the directory is not there yet.
        a.record("u1", t);
        assert!(!missing.join(FILE).exists());
        // No further request and no manual flush: the flusher's own retry writes it.
        std::fs::create_dir(&missing).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !missing.join(FILE).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the flusher never retried"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(read(&missing)["seen"]["u1"], t);
    }

    #[test]
    fn the_flusher_writes_a_trailing_time_on_its_own() {
        let d = tempfile::tempdir().unwrap();
        let a = Activity::with_flush_every(Some(d.path().to_path_buf()), Duration::from_millis(20));
        let t = crate::identity::now_ms();
        a.record("u1", t);
        a.record("u1", t + 1000);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while read(d.path())["seen"]["u1"] != t + 1000 {
            assert!(
                std::time::Instant::now() < deadline,
                "the trailing time was never written"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
