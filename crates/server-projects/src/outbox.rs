//! The project index outbox (sbstndalton/noevia#2191 round 2): `users/<id>/index-outbox.db`, a
//! durable queue of the RAG and storage work a project write leaves to do, and the tombstones of
//! deleted projects' RAG indexes.
//!
//! The Rust front owns the project writes that cause this work (create, delete). The work itself
//! runs where its engine is: the vector store is sqlite-vec's `vec0` tables, which only noevia-core
//! loads (rag.cjs), and the empty-folder sweep in remote storage needs a WebDAV DELETE the front's
//! storage client does not make. So the front records *what* is to be done, in the same request
//! that changed projects.json, and core's executor (`server/rust-index-outbox.cjs`) drains it.
//!
//! - A job's key is its identity (`["index",<project>,<file>]`, `["sweep",<project>]`): enqueueing a key
//!   already pending replaces it with a new generation, so the same work is never queued twice.
//! - The executor claims a job at its generation, runs it, then deletes it only at that
//!   generation: work requested while it ran stays pending and runs again (nothing is lost), and a
//!   crash between claim and acknowledgement leaves a `running` row the executor takes back on its
//!   next start (at least once; every job is idempotent: an index run re-reads the file's current
//!   text, a sweep removes only empty folders).
//! - Every write transaction bumps `meta.generation`. A project delete writes a tombstone at that
//!   generation and drops the project's pending index jobs in the same transaction, then removes
//!   the RAG files. Core captures the generation when an index run starts and stops the run (and
//!   drops its database handle) on its next step once a tombstone newer than that exists, so an
//!   in-flight run never recreates a deleted project's index.
//!
//! Only the signed-in user's own directory is opened; nothing here creates that directory.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The outbox file in a user's directory.
pub const OUTBOX_FILE: &str = "index-outbox.db";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS jobs (
  key TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  project TEXT NOT NULL,
  file TEXT,
  payload TEXT,
  generation INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0,
  claimed_at INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_project ON jobs(project);
CREATE TABLE IF NOT EXISTS tombstones (project TEXT PRIMARY KEY, generation INTEGER NOT NULL, at INTEGER NOT NULL);
INSERT OR IGNORE INTO meta (key, value) VALUES ('generation', 0);
";

#[derive(Debug)]
pub enum OutboxError {
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for OutboxError {
    fn from(e: rusqlite::Error) -> Self {
        OutboxError::Sql(e)
    }
}

/// The outbox of the user whose directory is `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(OUTBOX_FILE)
}

fn open(dir: &Path) -> Result<Connection, OutboxError> {
    let conn = Connection::open(path(dir))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn bump(tx: &rusqlite::Transaction<'_>) -> Result<i64, OutboxError> {
    tx.execute(
        "UPDATE meta SET value = value + 1 WHERE key = 'generation'",
        [],
    )?;
    Ok(
        tx.query_row("SELECT value FROM meta WHERE key = 'generation'", [], |r| {
            r.get(0)
        })?,
    )
}

/// A job key: the JSON array of its parts (no NUL separators: core's node:sqlite reads a TEXT
/// value only up to its first NUL).
fn key(parts: &[&str]) -> String {
    let mut out = String::from("[");
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        js_json::quote(p, &mut out);
    }
    out.push(']');
    out
}

/// The key of a file's index job: `["index", project, file]`.
pub fn index_key(project: &str, file: &str) -> String {
    key(&["index", project, file])
}

/// The key of a deleted project's storage sweep: `["sweep", project]`.
pub fn sweep_key(project: &str) -> String {
    key(&["sweep", project])
}

/// One job: its kind, project, optional file and optional payload (JSON text).
pub struct Job<'a> {
    pub kind: &'a str,
    pub project: &'a str,
    pub file: Option<&'a str>,
    pub payload: Option<String>,
}

fn key_of(job: &Job<'_>) -> String {
    match (job.kind, job.file) {
        ("index", Some(f)) => index_key(job.project, f),
        ("sweep", _) => sweep_key(job.project),
        (kind, file) => key(&[kind, job.project, file.unwrap_or("")]),
    }
}

/// Queues `jobs` in one transaction (a job whose key is pending gets this generation). A project
/// with a tombstone takes no index job. Returns the generation written.
pub fn enqueue(dir: &Path, jobs: &[Job<'_>], now_ms: i64) -> Result<i64, OutboxError> {
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let generation = bump(&tx)?;
    for job in jobs {
        if job.kind == "index" {
            let dead: Option<i64> = tx
                .query_row(
                    "SELECT generation FROM tombstones WHERE project = ?1",
                    [job.project],
                    |r| r.get(0),
                )
                .optional()?;
            if dead.is_some() {
                continue;
            }
        }
        tx.execute(
            "INSERT INTO jobs (key, kind, project, file, payload, generation, state, attempts, claimed_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, NULL, ?7)
             ON CONFLICT(key) DO UPDATE SET generation = excluded.generation, payload = excluded.payload,
               state = 'pending', attempts = 0, claimed_at = NULL",
            params![
                key_of(job),
                job.kind,
                job.project,
                job.file,
                job.payload,
                generation,
                now_ms
            ],
        )?;
    }
    tx.commit()?;
    Ok(generation)
}

/// A project's delete: its tombstone at a new generation, its pending index jobs dropped, and
/// `sweep` (the storage sweep, if any) queued, in one transaction. Returns the generation.
pub fn tombstone(
    dir: &Path,
    project: &str,
    sweep: Option<String>,
    now_ms: i64,
) -> Result<i64, OutboxError> {
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let generation = bump(&tx)?;
    tx.execute(
        "INSERT INTO tombstones (project, generation, at) VALUES (?1, ?2, ?3)
         ON CONFLICT(project) DO UPDATE SET generation = excluded.generation, at = excluded.at",
        params![project, generation, now_ms],
    )?;
    tx.execute(
        "DELETE FROM jobs WHERE project = ?1 AND kind = 'index'",
        [project],
    )?;
    if let Some(payload) = sweep {
        tx.execute(
            "INSERT INTO jobs (key, kind, project, file, payload, generation, state, attempts, claimed_at, created_at)
             VALUES (?1, 'sweep', ?2, NULL, ?3, ?4, 'pending', 0, NULL, ?5)
             ON CONFLICT(key) DO UPDATE SET generation = excluded.generation, payload = excluded.payload,
               state = 'pending', attempts = 0, claimed_at = NULL",
            params![sweep_key(project), project, payload, generation, now_ms],
        )?;
    }
    tx.commit()?;
    Ok(generation)
}

/// A project id created again (the Diary context is created with a fixed id): its tombstone goes,
/// so its new index is not refused.
pub fn revive(dir: &Path, project: &str) -> Result<(), OutboxError> {
    if !path(dir).exists() {
        return Ok(());
    }
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let gone = tx.execute("DELETE FROM tombstones WHERE project = ?1", [project])?;
    if gone > 0 {
        bump(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

/// A pending or running job, as the tests and the executor read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    pub kind: String,
    pub project: String,
    pub file: Option<String>,
    pub generation: i64,
    pub state: String,
}

/// Every job, oldest first (tests and diagnostics).
pub fn jobs(dir: &Path) -> Result<Vec<Row>, OutboxError> {
    if !path(dir).exists() {
        return Ok(Vec::new());
    }
    let conn = open(dir)?;
    let mut stmt = conn.prepare(
        "SELECT key, kind, project, file, generation, state FROM jobs ORDER BY generation, key",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Row {
                key: r.get(0)?,
                kind: r.get(1)?,
                project: r.get(2)?,
                file: r.get(3)?,
                generation: r.get(4)?,
                state: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The generation of the project's tombstone, if it has one.
pub fn tombstoned(dir: &Path, project: &str) -> Result<Option<i64>, OutboxError> {
    if !path(dir).exists() {
        return Ok(None);
    }
    let conn = open(dir)?;
    Ok(conn
        .query_row(
            "SELECT generation FROM tombstones WHERE project = ?1",
            [project],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn index<'a>(project: &'a str, file: &'a str) -> Job<'a> {
        Job {
            kind: "index",
            project,
            file: Some(file),
            payload: None,
        }
    }

    #[test]
    fn the_same_work_queued_twice_is_one_job_at_the_newer_generation() {
        let d = tempfile::tempdir().unwrap();
        let g1 = enqueue(d.path(), &[index("p", "a.md")], 1).unwrap();
        let g2 = enqueue(d.path(), &[index("p", "a.md"), index("p", "b.md")], 2).unwrap();
        assert!(g2 > g1);
        let rows = jobs(d.path()).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|r| r.generation == g2 && r.state == "pending"));
    }

    #[test]
    fn a_tombstone_drops_pending_index_work_and_refuses_more() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a.md"), index("q", "a.md")], 1).unwrap();
        let g = tombstone(d.path(), "p", Some("{\"folder\":\"f\"}".into()), 2).unwrap();
        assert_eq!(tombstoned(d.path(), "p").unwrap(), Some(g));
        enqueue(d.path(), &[index("p", "late.md")], 3).unwrap();
        let rows = jobs(d.path()).unwrap();
        let mut kinds: Vec<_> = rows
            .iter()
            .map(|r| (r.kind.as_str(), r.project.as_str()))
            .collect();
        kinds.sort_unstable();
        assert_eq!(kinds, vec![("index", "q"), ("sweep", "p")]);
        revive(d.path(), "p").unwrap();
        assert_eq!(tombstoned(d.path(), "p").unwrap(), None);
    }

    #[test]
    fn nothing_is_created_for_a_read_of_a_missing_outbox() {
        let d = tempfile::tempdir().unwrap();
        assert!(jobs(d.path()).unwrap().is_empty());
        assert_eq!(tombstoned(d.path(), "p").unwrap(), None);
        revive(d.path(), "p").unwrap();
        assert!(!path(d.path()).exists());
    }
}
