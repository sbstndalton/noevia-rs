//! The durable extraction queue (sbstndalton/noevia#2191 round 5): `users/<id>/extraction-jobs.db`,
//! one row per project document whose extraction could not finish because Docling (or the OCR worker
//! of its completion pass) was unavailable, busy or not configured.
//!
//! - Key `["extract", project, file]`: one job per source. Queuing a key that is already there
//!   replaces its target (`byte_hash`, `version`) and keeps it pending, so the same work is never
//!   queued twice and a newer upload of the same name supersedes the older one.
//! - A job is claimed (`running`), run, then deleted only at the version it was claimed at; a crash
//!   or restart between claim and delete leaves a `running` row, which [`recover`] makes pending
//!   again at the next start (at least once; a run is idempotent: it re-reads the stored original and
//!   the source as it is now, and the result is content-addressed).
//! - A run that fails for a passing reason waits `min(30 s x 2^(attempts-1), 30 min)` before the
//!   next attempt; there is no last attempt: the document stays queued until Docling reads it.
//! - A run whose source is gone, renamed or holds other bytes by now is dropped: there is nothing
//!   left to extract.
//!
//! Only the signed-in user's own directory is opened; nothing here creates that directory.
use crate::document_ingest::{self, Ingested};
use crate::extract::{Engine, Limits};
use crate::{outbox, store, Ctx};
use js_json::JValue;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const QUEUE_FILE: &str = "extraction-jobs.db";
/// The first wait, doubled per attempt, and its cap.
pub const BASE_MS: i64 = 30_000;
pub const CAP_MS: i64 = 30 * 60_000;
/// The source record's marker while its job is queued (`document.extraction`).
pub const QUEUED: &str = "queued";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
  key TEXT PRIMARY KEY,
  project TEXT NOT NULL,
  file TEXT NOT NULL,
  byte_hash TEXT NOT NULL,
  version TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0,
  next_at INTEGER NOT NULL,
  last_error TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_due ON jobs(state, next_at);
";

pub fn path(dir: &Path) -> PathBuf {
    dir.join(QUEUE_FILE)
}

fn open(dir: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path(dir))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// The key of a source's job.
pub fn key(project: &str, file: &str) -> String {
    let mut out = String::from("[\"extract\",");
    js_json::quote(project, &mut out);
    out.push(',');
    js_json::quote(file, &mut out);
    out.push(']');
    out
}

/// The wait after `attempts` attempts.
pub fn backoff_ms(attempts: i64) -> i64 {
    let shift = (attempts.max(1) - 1).min(20) as u32;
    BASE_MS.saturating_mul(1i64 << shift).min(CAP_MS)
}

/// One queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub key: String,
    pub project: String,
    pub file: String,
    pub byte_hash: String,
    pub version: String,
    pub state: String,
    pub attempts: i64,
    pub next_at: i64,
    pub last_error: Option<String>,
}

/// Queues the source after its first attempt (which counts): due after [`backoff_ms`]`(1)`.
pub fn enqueue(
    dir: &Path,
    project: &str,
    file: &str,
    byte_hash: &str,
    version: &str,
    error: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let conn = open(dir)?;
    conn.execute(
        "INSERT INTO jobs (key, project, file, byte_hash, version, state, attempts, next_at, last_error, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 1, ?6, ?7, ?8)
         ON CONFLICT(key) DO UPDATE SET byte_hash = excluded.byte_hash, version = excluded.version,
           state = 'pending', attempts = 1, next_at = excluded.next_at, last_error = excluded.last_error",
        params![
            key(project, file),
            project,
            file,
            byte_hash,
            version,
            now_ms + backoff_ms(1),
            error,
            now_ms
        ],
    )?;
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        key: r.get(0)?,
        project: r.get(1)?,
        file: r.get(2)?,
        byte_hash: r.get(3)?,
        version: r.get(4)?,
        state: r.get(5)?,
        attempts: r.get(6)?,
        next_at: r.get(7)?,
        last_error: r.get(8)?,
    })
}

const COLUMNS: &str =
    "key, project, file, byte_hash, version, state, attempts, next_at, last_error";

/// Every job (tests, the result's evidence).
pub fn jobs(dir: &Path) -> rusqlite::Result<Vec<Job>> {
    if !path(dir).exists() {
        return Ok(Vec::new());
    }
    let conn = open(dir)?;
    let mut st = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM jobs ORDER BY created_at, key"
    ))?;
    let out = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}

/// At a process start: every `running` job was cut short; it is pending again, due now.
pub fn recover(dir: &Path, now_ms: i64) -> rusqlite::Result<usize> {
    if !path(dir).exists() {
        return Ok(0);
    }
    let conn = open(dir)?;
    conn.execute(
        "UPDATE jobs SET state = 'pending', next_at = MIN(next_at, ?1) WHERE state = 'running'",
        params![now_ms],
    )
}

fn claim(conn: &Connection, now_ms: i64) -> rusqlite::Result<Option<Job>> {
    let job = conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM jobs WHERE state = 'pending' AND next_at <= ?1 ORDER BY next_at, key LIMIT 1"
            ),
            params![now_ms],
            row,
        )
        .optional()?;
    if let Some(j) = &job {
        conn.execute(
            "UPDATE jobs SET state = 'running' WHERE key = ?1 AND version = ?2 AND state = 'pending'",
            params![j.key, j.version],
        )?;
    }
    Ok(job)
}

fn done(conn: &Connection, job: &Job) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM jobs WHERE key = ?1 AND version = ?2 AND byte_hash = ?3",
        params![job.key, job.version, job.byte_hash],
    )?;
    // Superseded meanwhile (a newer upload of the name): its row is pending again.
    conn.execute(
        "UPDATE jobs SET state = 'pending' WHERE key = ?1 AND state = 'running'",
        params![job.key],
    )?;
    Ok(())
}

fn again(conn: &Connection, job: &Job, error: &str, now_ms: i64) -> rusqlite::Result<()> {
    let attempts = job.attempts + 1;
    conn.execute(
        "UPDATE jobs SET state = 'pending', attempts = ?3, next_at = ?4, last_error = ?5
         WHERE key = ?1 AND version = ?2",
        params![
            job.key,
            job.version,
            attempts,
            now_ms + backoff_ms(attempts),
            error
        ],
    )?;
    Ok(())
}

/// What one run came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Extracted (or finished for good): the source record was replaced and its index job queued.
    Stored,
    /// Still unavailable: queued again with backoff.
    Waiting,
    /// The source is gone or changed: dropped.
    Dropped,
}

fn find_source<'a>(projects: &'a [JValue], job: &Job) -> Option<&'a JValue> {
    let p = store::find(projects, &job.project)?;
    let JValue::Arr(files) = p.get("files") else {
        return None;
    };
    files.iter().find(|f| {
        f.get("name").as_str() == Some(job.file.as_str())
            && f.get("document").get("byteHash").as_str() == Some(job.byte_hash.as_str())
    })
}

/// The source record as the project lists it now, with its text (inline or from its text file).
fn with_text(dir: &Path, project: &str, file: &JValue) -> JValue {
    let mut f = file.clone();
    if f.get("content").as_str().is_none() {
        let text = crate::sources::text(dir, project, file).unwrap_or_default();
        if let JValue::Obj(items) = &mut f {
            items.retain(|(k, _)| k != "textRef" && k != "chars" && k != "skillHeader");
            js_json::wtf16::set(items, "content", JValue::Str(text));
        }
    }
    f
}

/// Runs every due job of the user whose directory is `dir`, in order, until none is due.
pub fn run_due(ctx: Ctx<'_>, engine: &dyn Engine, limits: &Limits) -> Vec<(Job, Outcome)> {
    let Ok(dir) = store::user_dir(ctx.data_dir, ctx.user_id) else {
        return Vec::new();
    };
    if !path(&dir).exists() || !dir.is_dir() {
        return Vec::new();
    }
    let Ok(conn) = open(&dir) else {
        return Vec::new();
    };
    let mut ran = Vec::new();
    while let Ok(Some(job)) = claim(&conn, ctx.now_ms) {
        let outcome = run_one(ctx, &dir, &conn, &job, engine, limits);
        ran.push((job, outcome));
        if ran.len() >= 64 {
            break;
        }
    }
    ran
}

fn run_one(
    ctx: Ctx<'_>,
    dir: &Path,
    conn: &Connection,
    job: &Job,
    engine: &dyn Engine,
    limits: &Limits,
) -> Outcome {
    let drop_it = || {
        let _ = done(conn, job);
        Outcome::Dropped
    };
    // The record exists but its original cannot be read again: the record says "will be read automatically",
    // which would stay true forever. It is rewritten as failed with the real reason before the job goes.
    // If the record cannot be rewritten (a disk fault), the job stays: dropping it would leave a record that says
    // "will be read automatically" and nothing that reads it (#2245).
    let give_up = |reason: &str| {
        if !fail_record(ctx, dir, job, reason) {
            let _ = again(conn, job, "projects save failed", ctx.now_ms);
            return Outcome::Waiting;
        }
        drop_it()
    };
    let Ok(projects) = store::read(dir) else {
        let _ = again(conn, job, "projects unreadable", ctx.now_ms);
        return Outcome::Waiting;
    };
    let Some(previous) = find_source(&projects, job).map(|f| with_text(dir, &job.project, f))
    else {
        return drop_it();
    };
    let Some(original) = document_ingest::version_path(dir, &job.project, &job.byte_hash, ".pdf")
    else {
        return give_up(ORIGINAL_GONE);
    };
    let Ok(bytes) = std::fs::read(&original) else {
        return give_up(ORIGINAL_GONE);
    };
    if crate::extract::sha_hex(&bytes) != job.byte_hash {
        return give_up(ORIGINAL_GONE);
    }
    // The queued record's own failure is not "the last readable text": its predecessor's is.
    let prior = previous.clone();
    let ingested = match document_ingest::ingest(
        dir,
        &job.project,
        &job.file,
        &bytes,
        Some(&prior),
        engine,
        limits,
    ) {
        Ok(i) => i,
        Err(e) => {
            let _ = again(conn, job, &format!("store: {}", e.kind()), ctx.now_ms);
            return Outcome::Waiting;
        }
    };
    let Ingested {
        mut file,
        retryable,
        open,
        ..
    } = ingested;
    if retryable {
        let error = file
            .get("document")
            .get("error")
            .as_str()
            .unwrap_or("")
            .to_string();
        let _ = again(conn, job, &error, ctx.now_ms);
        return Outcome::Waiting;
    }
    if open {
        mark_queued(&mut file, false);
    }
    document_ingest::mark_indexing(&mut file);
    // `Gone`: the project or the record is not there any more, nothing is left to read. `Disk`: the text
    // could not be written; the record still says it is waiting, so the job must too (#2245).
    enum Saved {
        Stored,
        Gone,
        Disk,
    }
    let saved = store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, &job.project) else {
            return (Saved::Gone, false);
        };
        let JValue::Arr(mut files) = p.get("files").clone() else {
            return (Saved::Gone, false);
        };
        let Some(i) = files.iter().position(|f| {
            f.get("name").as_str() == Some(job.file.as_str())
                && f.get("document").get("byteHash").as_str() == Some(job.byte_hash.as_str())
        }) else {
            return (Saved::Gone, false);
        };
        // noevia#2245: the record as it is now takes the new fields, so what the worker did not read
        // (a synced document's `source`, an `attachment`) is kept. Only the old text reference goes:
        // `content` is the new text and `externalize` writes it out again.
        let mut record = files.remove(i);
        if let JValue::Obj(items) = &mut record {
            items.retain(|(k, _)| k != "textRef" && k != "chars" && k != "skillHeader");
            if let JValue::Obj(fresh) = &file {
                for (k, v) in fresh {
                    js_json::wtf16::set(items, k, v.clone());
                }
            }
            // The upload's attachment (kept above) says what the document says, not what it said when queued.
            let state = file
                .get("document")
                .get("state")
                .as_str()
                .map(str::to_owned);
            if let (Some(state), Some((_, JValue::Obj(a)))) =
                (state, items.iter_mut().find(|(k, _)| k == "attachment"))
            {
                js_json::wtf16::set(a, "state", JValue::Str(state));
            }
        } else {
            record = file.clone();
        }
        files.push(record);
        store::set(p, "files", JValue::Arr(files));
        if crate::lifecycle::externalize(dir, p).is_err() {
            return (Saved::Disk, false);
        }
        (Saved::Stored, true)
    });
    match saved {
        Ok(Saved::Stored) => {
            document_ingest::prune_now(dir, &job.project);
            let index = outbox::Job {
                kind: "index",
                project: &job.project,
                file: Some(&job.file),
                payload: None,
            };
            if let Err(e) = outbox::enqueue(dir, &[index], ctx.now_ms) {
                eprintln!(
                    "noevia-server: index outbox write failed for an extracted document: {e:?}"
                );
            }
            if open {
                let _ = again(conn, job, "completion pass open", ctx.now_ms);
                Outcome::Waiting
            } else {
                let _ = done(conn, job);
                Outcome::Stored
            }
        }
        Ok(Saved::Gone) => drop_it(),
        Ok(Saved::Disk) => {
            let _ = again(conn, job, "text save failed", ctx.now_ms);
            Outcome::Waiting
        }
        Err(_) => {
            let _ = again(conn, job, "projects save failed", ctx.now_ms);
            Outcome::Waiting
        }
    }
}

/// What a person reads about a document whose extraction is queued (#2237): the file is fine, the
/// reading service is not there yet. The service's own words ("fetch failed") stay in the job's
/// `last_error`, never on the record.
pub const WAITING: &str =
    "Waiting for document reading to come back. The file is saved and will be read automatically.";
/// The stable id of [`WAITING`], so the browser can word it in the interface language.
pub const WAITING_ID: &str = "extractionWaiting";

/// Why a queued document is given up on when its stored original is missing or changed.
pub const ORIGINAL_GONE: &str =
    "The stored original of this document is no longer available, so it cannot be read. Upload it again.";

/// Rewrites the queued record of `job` as `failed` with `reason` (and no queued marker). `true` when that is
/// saved or the record is no longer there to rewrite (the caller drops the job); `false` when the save itself
/// failed (the caller keeps the job).
fn fail_record(ctx: Ctx<'_>, dir: &Path, job: &Job, reason: &str) -> bool {
    store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, &job.project) else {
            return (false, false);
        };
        let JValue::Arr(mut files) = p.get("files").clone() else {
            return (false, false);
        };
        let Some(file) = files.iter_mut().find(|f| {
            f.get("name").as_str() == Some(job.file.as_str())
                && f.get("document").get("byteHash").as_str() == Some(job.byte_hash.as_str())
        }) else {
            return (false, false);
        };
        if let JValue::Obj(items) = file {
            if let Some((_, JValue::Obj(d))) = items.iter_mut().find(|(k, _)| k == "document") {
                d.retain(|(k, _)| k != "extraction" && k != "errorId");
                js_json::wtf16::set(d, "state", JValue::Str("failed".into()));
                js_json::wtf16::set(d, "error", JValue::Str(reason.into()));
            }
        }
        store::set(p, "files", JValue::Arr(files));
        (true, true)
    })
    .is_ok()
}

/// Marks a source record as queued for extraction (`document.extraction: "queued"`).
///
/// A record has one state (#2237): a document that failed to read only because the service was
/// unavailable is `state: "queued"` with the plain waiting reason, not `failed` and queued at once.
/// A record that already has text (`ready`, `partial`: a completion pass left open) keeps its state.
/// The worker finds the record by name and byte hash, never by state, and a successful run replaces it.
///
/// `retryable`: the read failed only because the service was unavailable. Only such a record is rewritten
/// from `failed`; one that failed for its own reasons (every page unreadable, say) with a completion pass
/// left open keeps its real state and error and is merely marked queued.
pub fn mark_queued(file: &mut JValue, retryable: bool) {
    if let JValue::Obj(items) = file {
        if let Some((_, JValue::Obj(d))) = items.iter_mut().find(|(k, _)| k == "document") {
            let failed = d
                .iter()
                .any(|(k, v)| k == "state" && v.as_str() == Some("failed"));
            if failed && retryable {
                js_json::wtf16::set(d, "state", JValue::Str(QUEUED.into()));
                js_json::wtf16::set(d, "error", JValue::Str(WAITING.into()));
                js_json::wtf16::set(d, "errorId", JValue::Str(WAITING_ID.into()));
            }
            js_json::wtf16::set(d, "extraction", JValue::Str(QUEUED.into()));
        }
    }
}

/// Users with a queue file under `data_dir/users`.
pub fn users_with_jobs(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("users")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|id| store::user_dir(data_dir, id).is_ok())
        .filter(|id| path(&data_dir.join("users").join(id)).exists())
        .collect()
}

/// Removes a source's job (a newer upload of the name was read).
pub fn clear(dir: &Path, project: &str, file: &str) -> rusqlite::Result<()> {
    let conn = open(dir)?;
    conn.execute(
        "DELETE FROM jobs WHERE key = ?1",
        params![key(project, file)],
    )?;
    Ok(())
}

/// Writes `job` back as it was (its state, attempts and due time), replacing the name's row.
pub fn put_back(dir: &Path, job: &Job) -> rusqlite::Result<()> {
    let conn = open(dir)?;
    conn.execute(
        "INSERT INTO jobs (key, project, file, byte_hash, version, state, attempts, next_at, last_error, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?8)
         ON CONFLICT(key) DO UPDATE SET byte_hash = excluded.byte_hash, version = excluded.version,
           state = excluded.state, attempts = excluded.attempts, next_at = excluded.next_at,
           last_error = excluded.last_error",
        params![
            job.key,
            job.project,
            job.file,
            job.byte_hash,
            job.version,
            job.state,
            job.attempts,
            job.next_at,
            job.last_error
        ],
    )?;
    Ok(())
}

/// The pending job of a source, if any (tests and the route's answer).
pub fn pending(dir: &Path, project: &str, file: &str) -> Option<Job> {
    if !path(dir).exists() {
        return None;
    }
    let conn = open(dir).ok()?;
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM jobs WHERE key = ?1"),
        params![key(project, file)],
        row,
    )
    .optional()
    .ok()
    .flatten()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(6), 960_000);
        assert_eq!(backoff_ms(7), CAP_MS);
        assert_eq!(backoff_ms(500), CAP_MS);
    }

    #[test]
    fn enqueue_is_idempotent_and_recover_reopens_running_jobs() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        enqueue(dir, "p", "a.pdf", "h1", "v1", Some("fetch failed"), 1000).unwrap();
        enqueue(dir, "p", "a.pdf", "h2", "v2", None, 2000).unwrap();
        let all = jobs(dir).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].byte_hash, "h2");
        assert_eq!(all[0].next_at, 2000 + BASE_MS);
        let conn = open(dir).unwrap();
        assert!(claim(&conn, 0).unwrap().is_none(), "not due yet");
        let job = claim(&conn, 1_000_000).unwrap().unwrap();
        assert_eq!(jobs(dir).unwrap()[0].state, "running");
        assert_eq!(recover(dir, 5).unwrap(), 1);
        let j = &jobs(dir).unwrap()[0];
        assert_eq!((j.state.as_str(), j.next_at), ("pending", 5));
        again(&conn, &job, "busy", 10).unwrap();
        let j = &jobs(dir).unwrap()[0];
        assert_eq!((j.attempts, j.next_at), (2, 10 + 60_000));
        done(&conn, &job).unwrap();
        assert!(jobs(dir).unwrap().is_empty());
    }
}
