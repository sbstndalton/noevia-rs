//! The file operations the front is running, for core to see (sbstndalton/noevia#2191 round 8).
//!
//! core's chat waits for a file it was just sent (#1441: a turn sent right after an upload was accepted
//! must see that file) and its source-text collector does not sweep while a sync or a folder import runs
//! (#1815). Both used to read Node's memory, where the operation ran. The operation is the front's now, so
//! it keeps the list in `users/<id>/source-file-ops.json`, which core re-reads when its signal changes
//! (core `rust-source-ops.cjs`):
//!
//! ```text
//! { "version": 1, "ops": [ { "id", "project", "kind": "upload"|"import"|"sync", "name", "state":
//!   "running"|"ready"|"failed", "at" } ] }
//! ```
//!
//! An operation is `running` from its start to its end, then `ready` or `failed` for [`KEEP_MS`] so a turn
//! that waited can still say which. A `running` entry a dead process left is `failed` the first time this
//! process writes the file; core also forgets a `running` entry older than [`STALE_MS`].
//! Written atomically (temp file, rename) under this process's lock; this process is the only writer.
use js_json::{wtf16, JValue};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const FILE: &str = "source-file-ops.json";
/// How long a finished operation stays listed.
pub const KEEP_MS: i64 = 5 * 60 * 1000;
/// A `running` entry older than this is not waited for (core's own bound is the same).
pub const STALE_MS: i64 = 6 * 60 * 60 * 1000;

static WRITER: Mutex<()> = Mutex::new(());
/// The users this process has already cleaned a previous process's `running` entries for.
static CLEANED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

fn read(dir: &Path) -> Vec<JValue> {
    let Ok(text) = std::fs::read_to_string(path(dir)) else {
        return Vec::new();
    };
    match wtf16::parse(&text) {
        Ok(doc) => match doc.get("ops") {
            JValue::Arr(a) => a.clone(),
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

fn write(dir: &Path, ops: Vec<JValue>) -> std::io::Result<()> {
    let doc = JValue::obj([("version", JValue::Num(1.0)), ("ops", JValue::Arr(ops))]);
    let text = wtf16::stringify(&doc).unwrap_or_default();
    let temp = dir.join(format!("{FILE}.{}.tmp", crate::js::random36(12)));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&temp, path(dir))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn mutate(dir: &Path, now_ms: i64, change: impl FnOnce(&mut Vec<JValue>)) {
    let _held = match WRITER.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    // The user's directory is never created here (a removed account stays removed).
    if !dir.is_dir() {
        return;
    }
    let mut ops = read(dir);
    let first = match CLEANED.lock() {
        Ok(mut seen) => {
            if seen.iter().any(|d| d == dir) {
                false
            } else {
                seen.push(dir.to_path_buf());
                true
            }
        }
        Err(_) => false,
    };
    if first {
        // A previous process's operations are over.
        for op in &mut ops {
            if op.get("state").as_str() == Some("running") {
                if let JValue::Obj(items) = op {
                    wtf16::set(items, "state", JValue::Str("failed".into()));
                    wtf16::set(items, "at", JValue::Num(now_ms as f64));
                }
            }
        }
    }
    change(&mut ops);
    ops.retain(|op| {
        op.get("state").as_str() == Some("running")
            || now_ms - op.get("at").as_num().unwrap_or(0.0) as i64 <= KEEP_MS
    });
    if let Err(e) = write(dir, ops) {
        eprintln!(
            "noevia-server: the source operations list could not be written: {}",
            e.kind()
        );
    }
}

/// A running operation, listed until it ends; dropping it without [`Op::end`] lists it as failed.
pub struct Op {
    dir: PathBuf,
    id: String,
    done: bool,
}

/// `kind`: `upload`, `import` (a folder import's file) or `sync`; `name` the file's name (`""` for a sync).
/// Stamped with the wall clock now: a job that waited its turn must not be listed at the time its request
/// arrived, or core would forget it as stale and prune it at once.
pub fn begin(dir: &Path, project: &str, kind: &str, name: &str) -> Op {
    begin_at(dir, project, kind, name, crate::wall_ms())
}

fn begin_at(dir: &Path, project: &str, kind: &str, name: &str, now_ms: i64) -> Op {
    let id = crate::source_jobs::new_id();
    let entry = JValue::obj([
        ("id", JValue::Str(id.clone())),
        ("project", JValue::Str(project.to_owned())),
        ("kind", JValue::Str(kind.to_owned())),
        ("name", JValue::Str(wtf16::slice(name, 200))),
        ("state", JValue::Str("running".into())),
        ("at", JValue::Num(now_ms as f64)),
    ]);
    mutate(dir, now_ms, |ops| ops.push(entry));
    Op {
        dir: dir.to_path_buf(),
        id,
        done: false,
    }
}

impl Op {
    /// The operation ended: `ok` is `ready`, else `failed`.
    pub fn end(self, ok: bool) {
        self.end_at(ok, crate::wall_ms());
    }
    fn end_at(mut self, ok: bool, now_ms: i64) {
        self.finish(ok, now_ms);
    }
    fn finish(&mut self, ok: bool, now_ms: i64) {
        if self.done {
            return;
        }
        self.done = true;
        let id = self.id.clone();
        mutate(&self.dir, now_ms, |ops| {
            for op in ops.iter_mut() {
                if op.get("id").as_str() == Some(id.as_str()) {
                    if let JValue::Obj(items) = op {
                        wtf16::set(
                            items,
                            "state",
                            JValue::Str(if ok { "ready" } else { "failed" }.into()),
                        );
                        wtf16::set(items, "at", JValue::Num(now_ms as f64));
                    }
                }
            }
        });
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        self.finish(false, crate::wall_ms());
    }
}

/// For a test: the listed operations as `(kind, name, state)`.
pub fn listed(dir: &Path) -> Vec<(String, String, String)> {
    read(dir)
        .iter()
        .map(|o| {
            (
                o.get("kind").as_str().unwrap_or("").to_owned(),
                o.get("name").as_str().unwrap_or("").to_owned(),
                o.get("state").as_str().unwrap_or("").to_owned(),
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn an_operation_is_listed_while_it_runs_and_for_a_while_after() {
        let d = tempfile::tempdir().unwrap();
        let op = begin_at(d.path(), "p", "upload", "a.pdf", 1_000);
        assert_eq!(
            listed(d.path()),
            vec![("upload".into(), "a.pdf".into(), "running".into())]
        );
        op.end_at(true, 2_000);
        assert_eq!(listed(d.path())[0].2, "ready");
        // Past the keep time the next write drops it; a running one is never dropped.
        let other = begin_at(d.path(), "p", "sync", "", 2_000 + KEEP_MS + 1);
        let l = listed(d.path());
        assert_eq!(l, vec![("sync".into(), "".into(), "running".into())]);
        drop(other);
        assert_eq!(
            listed(d.path())[0].2,
            "failed",
            "an operation dropped unfinished failed"
        );
    }

    #[test]
    fn a_dead_processs_running_entries_fail_at_the_first_write() {
        let d = tempfile::tempdir().unwrap();
        let stale = r#"{"version":1,"ops":[{"id":"x","project":"p","kind":"upload","name":"old.pdf","state":"running","at":5}]}"#;
        std::fs::write(path(d.path()), stale).unwrap();
        let _op = begin_at(d.path(), "p", "upload", "new.pdf", 10);
        let l = listed(d.path());
        assert_eq!(l[0], ("upload".into(), "old.pdf".into(), "failed".into()));
        assert_eq!(l[1].2, "running");
    }

    /// #2191 round 8 review: an operation is listed at the moment it begins and ends, not at the time its
    /// request arrived (a job that waited, a sync that ran for hours): core forgets a `running` entry by
    /// its `at` and prunes a finished one by it, so a stale stamp lost the operation at once.
    #[test]
    fn an_operation_is_stamped_with_the_wall_clock_when_it_begins_and_ends() {
        let d = tempfile::tempdir().unwrap();
        let before = crate::wall_ms();
        let op = begin(d.path(), "p", "upload", "a.pdf");
        let at = |d: &Path| read(d)[0].get("at").as_num().unwrap() as i64;
        let began = at(d.path());
        assert!(
            (before..=crate::wall_ms()).contains(&began),
            "stamped {began}, now {}",
            crate::wall_ms()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        op.end(false);
        assert_eq!(listed(d.path())[0].2, "failed");
        assert!(at(d.path()) > began, "ended later than it began");
        // Still listed, as a finished one is for KEEP_MS from its end.
        assert_eq!(listed(d.path()).len(), 1);
    }

    #[test]
    fn a_missing_user_directory_is_not_created() {
        let d = tempfile::tempdir().unwrap();
        let gone = d.path().join("users").join("gone");
        let op = begin(&gone, "p", "upload", "a");
        op.end(true);
        assert!(!gone.exists());
    }
}
