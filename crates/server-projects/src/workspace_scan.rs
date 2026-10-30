//! Which listed sources have lost their text file (`textMissing: true` in `GET /api/workspace`,
//! sbstndalton/noevia#1815/#1818), the list side of core source-text.cjs ported.
//!
//! The list never touches the disk per source: it reports what is known. The first list of a
//! workspace shows nothing flagged (unknown is not flagged) and starts a background scan of the
//! sources it showed; the scan runs at most every [`SCAN_EVERY`] and records each file it finds gone
//! or unreadable. A flagged source is looked at again by the next list (a file written back clears
//! the flag at once; core cleared it when a text was written or read again).
//!
//! The state is per data directory in this process; a restart forgets it, as a Node restart did.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// core `SCAN_MS`.
pub const SCAN_EVERY: Duration = Duration::from_secs(3 * 60);

type Key = (String, String);

struct State {
    at: Option<Instant>,
    running: bool,
    seen: HashSet<Key>,
    missing: HashSet<Key>,
}

struct Scans {
    every: Duration,
    by_dir: HashMap<PathBuf, State>,
}

fn scans() -> &'static Mutex<Scans> {
    static SCANS: OnceLock<Mutex<Scans>> = OnceLock::new();
    SCANS.get_or_init(|| {
        Mutex::new(Scans {
            every: SCAN_EVERY,
            by_dir: HashMap::new(),
        })
    })
}

fn with<T>(f: impl FnOnce(&mut Scans) -> T) -> T {
    let mut guard = scans().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

fn blank() -> State {
    State {
        at: None,
        running: false,
        seen: HashSet::new(),
        missing: HashSet::new(),
    }
}

/// core `textMissing(file)` for an external source: whether its text is known to be gone. It also
/// remembers the source for the workspace's next scan (when one is due and none is running).
pub fn known_missing(dir: &Path, project: &str, reference: &str) -> bool {
    let key = (project.to_owned(), reference.to_owned());
    let flagged = with(|s| {
        let every = s.every;
        let st = s.by_dir.entry(dir.to_path_buf()).or_insert_with(blank);
        if !st.running && st.at.is_none_or(|at| at.elapsed() >= every) {
            st.seen.insert(key.clone());
        }
        st.missing.contains(&key)
    });
    if !flagged {
        return false;
    }
    // A flagged source is looked at again: the file may be back.
    if crate::sources::text_readable(dir, project, reference) == Some(true) {
        with(|s| {
            if let Some(st) = s.by_dir.get_mut(dir) {
                st.missing.remove(&key);
            }
        });
        return false;
    }
    true
}

/// Starts the workspace's scan of the sources the list showed, when one is due.
pub fn kick(dir: &Path) {
    let batch = with(|s| {
        let every = s.every;
        let st = s.by_dir.entry(dir.to_path_buf()).or_insert_with(blank);
        if st.running || st.seen.is_empty() || st.at.is_some_and(|at| at.elapsed() < every) {
            return None;
        }
        st.at = Some(Instant::now());
        st.running = true;
        Some(std::mem::take(&mut st.seen))
    });
    let Some(batch) = batch else {
        return;
    };
    let dir = dir.to_path_buf();
    let work = {
        let dir = dir.clone();
        move || {
            for (project, reference) in &batch {
                let state = crate::sources::text_readable(&dir, project, reference);
                with(|s| {
                    if let Some(st) = s.by_dir.get_mut(&dir) {
                        let key = (project.clone(), reference.clone());
                        match state {
                            Some(true) => {
                                st.missing.remove(&key);
                            }
                            Some(false) => {
                                st.missing.insert(key);
                            }
                            None => {}
                        }
                    }
                });
            }
            with(|s| {
                if let Some(st) = s.by_dir.get_mut(&dir) {
                    st.running = false;
                }
            });
        }
    };
    // A thread of its own, so a slow disk never holds the request that started it.
    if std::thread::Builder::new()
        .name("workspace-scan".into())
        .spawn(work)
        .is_err()
    {
        with(|s| {
            if let Some(st) = s.by_dir.get_mut(&dir) {
                st.running = false;
            }
        });
    }
}

/// Waits until the workspace's scan has finished (core `scanSettled`). For tests.
pub fn settled(dir: &Path) {
    for _ in 0..2000 {
        let busy = with(|s| s.by_dir.get(dir).is_some_and(|st| st.running));
        if !busy {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Makes the next list due again (the interval has passed). For tests; core `_setScanMs(0)`.
pub fn due_again(dir: &Path) {
    with(|s| {
        if let Some(st) = s.by_dir.get_mut(dir) {
            st.at = None;
        }
    });
}
