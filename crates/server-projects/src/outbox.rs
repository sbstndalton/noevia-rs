//! The project index outbox (sbstndalton/noevia#2191 round 2): `users/<id>/index-outbox.db`, a
//! durable queue of the RAG and storage work a project write leaves to do, and the tombstones of
//! deleted projects' RAG indexes.
//!
//! The native front owns project writes and drains `index` / `unindex` through
//! embedding-worker's source executor. Storage `sweep` jobs retain their separate
//! native owner ([`drain_kind`], [`crate::project_storage`]). Core retains only
//! read-only generation/tombstone checks for its remaining request-time RAG calls.
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
  created_at INTEGER NOT NULL,
  last_error TEXT
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
    let mut conn = Connection::open(path(dir))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    ensure_error_column(&mut conn)?;
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
/// The projects a projects.json write lists exist again: their tombstones go (one transaction, and
/// nothing is created when the user has no outbox). Called by every projects.json writer after the
/// file is on disk, so a fixed id created again (a free chat's context project, the Diary context)
/// is indexed again while a deleted project, listed nowhere, stays refused (Node treats any
/// tombstone as deleted, core rust-index-outbox.cjs `deletedSince`).
pub fn revive_listed(dir: &Path, projects: &[js_json::JValue]) -> Result<usize, OutboxError> {
    if !path(dir).exists() {
        return Ok(0);
    }
    let ids: Vec<&str> = projects
        .iter()
        .filter_map(|p| p.get("id").as_str())
        .collect();
    if ids.is_empty() {
        return Ok(0);
    }
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut gone = 0;
    for id in ids {
        gone += tx.execute("DELETE FROM tombstones WHERE project = ?1", [id])?;
    }
    if gone > 0 {
        bump(&tx)?;
    }
    tx.commit()?;
    Ok(gone)
}

pub fn enqueue(dir: &Path, jobs: &[Job<'_>], now_ms: i64) -> Result<i64, OutboxError> {
    let mut conn = open(dir)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let generation = bump(&tx)?;
    for job in jobs {
        if job.kind == "index" || job.kind == "unindex" {
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
               state = 'pending', attempts = 0, claimed_at = NULL, last_error = NULL",
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

/// A project's delete: its tombstone at a new generation, its pending index and unindex jobs
/// (round 3: a removed file's RAG rows) dropped, and
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
        "DELETE FROM jobs WHERE project = ?1 AND kind IN ('index', 'unindex')",
        [project],
    )?;
    if let Some(payload) = sweep {
        tx.execute(
            "INSERT INTO jobs (key, kind, project, file, payload, generation, state, attempts, claimed_at, created_at)
             VALUES (?1, 'sweep', ?2, NULL, ?3, ?4, 'pending', 0, NULL, ?5)
             ON CONFLICT(key) DO UPDATE SET generation = excluded.generation, payload = excluded.payload,
               state = 'pending', attempts = 0, claimed_at = NULL, last_error = NULL",
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

/// The attempts after which a job is kept as `failed` (core rust-index-outbox.cjs MAX_ATTEMPTS).
pub const MAX_ATTEMPTS: i64 = 5;

/// A job this side claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub key: String,
    pub project: String,
    pub payload: Option<String>,
    pub generation: i64,
    pub attempts: i64,
}

/// Runs every pending job of `kind` once, oldest first, with the executor contract core keeps for
/// its kinds (sbstndalton/noevia#2191 round 3: the front executes `sweep` jobs itself, core only
/// `index`): claim at the row's generation (`running`), run, delete at that generation only (work
/// queued again meanwhile runs again); a failed run is pending again, `failed` after
/// [`MAX_ATTEMPTS`]. `recover` first takes back this kind's `running` rows (a run this process
/// never acknowledged: it restarted). Each side recovers and claims only its own kinds. Returns
/// (done, failed).
pub fn drain_kind(
    dir: &Path,
    kind: &str,
    recover: bool,
    now_ms: i64,
    mut run: impl FnMut(&Claimed) -> bool,
) -> Result<(usize, usize), OutboxError> {
    if !path(dir).exists() {
        return Ok((0, 0));
    }
    let conn = open(dir)?;
    if recover {
        conn.execute(
            "UPDATE jobs SET state = 'pending', claimed_at = NULL WHERE state = 'running' AND kind = ?1",
            [kind],
        )?;
    }
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT key, project, payload, generation, attempts FROM jobs
             WHERE state = 'pending' AND kind = ?1 ORDER BY generation, key",
        )?;
        let rows = stmt
            .query_map([kind], |r| {
                Ok(Claimed {
                    key: r.get(0)?,
                    project: r.get(1)?,
                    payload: r.get(2)?,
                    generation: r.get(3)?,
                    attempts: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let (mut done, mut failed) = (0, 0);
    for row in rows {
        let claimed = conn.execute(
            "UPDATE jobs SET state = 'running', claimed_at = ?1, attempts = attempts + 1
             WHERE key = ?2 AND generation = ?3 AND state = 'pending'",
            params![now_ms, row.key, row.generation],
        )?;
        if claimed == 0 {
            continue;
        }
        if run(&row) {
            conn.execute(
                "DELETE FROM jobs WHERE key = ?1 AND generation = ?2",
                params![row.key, row.generation],
            )?;
            done += 1;
        } else {
            let state = if row.attempts + 1 >= MAX_ATTEMPTS {
                "failed"
            } else {
                "pending"
            };
            conn.execute(
                "UPDATE jobs SET state = ?1, claimed_at = NULL WHERE key = ?2 AND generation = ?3 AND state = 'running'",
                params![state, row.key, row.generation],
            )?;
            failed += 1;
        }
    }
    Ok((done, failed))
}

// Native claims/acknowledgements never create or initialize a database: account
// removal may unlink it between the existence check and open.
fn open_claims(dir: &Path) -> Result<Connection, OutboxError> {
    let mut conn = Connection::open_with_flags(
        path(dir),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    ensure_error_column(&mut conn)?;
    Ok(conn)
}

/// A single RAG claim. The worker holds its lifetime owner lock before recovery.
/// Selecting just one row bounds synchronous work per tenant; network I/O happens
/// after this connection is dropped. Persistence is the original outbox schema.
#[derive(Debug, Clone)]
pub struct IndexClaim {
    pub kind: String,
    pub key: String,
    pub project: String,
    pub file: Option<String>,
    pub generation: i64,
    pub attempts: i64,
}

pub fn claim_index(
    dir: &Path,
    recover: bool,
    now_ms: i64,
) -> Result<Option<IndexClaim>, OutboxError> {
    for row in pending_rag(dir, recover)? {
        if claim_rag(dir, &row, now_ms)? {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

/// A drain snapshots pending work once, like Node: each generation is attempted
/// at most once in a pass. New work and failures wait until the next two-second
/// pass, without throttling a backlog to one file per tenant per tick.
pub fn pending_rag(dir: &Path, recover: bool) -> Result<Vec<IndexClaim>, OutboxError> {
    if !path(dir).exists() {
        return Ok(Vec::new());
    }
    let conn = open_claims(dir)?;
    if recover {
        // The older row-U contract said index-only recovery. Current Node owns
        // both index and unindex; recover both now-owned kinds to avoid stranding
        // a file removal interrupted by restart. Storage sweep recovery stays separate.
        conn.execute("UPDATE jobs SET state='pending',claimed_at=NULL WHERE kind IN ('index','unindex') AND state='running'",[])?;
    }
    let mut stmt=conn.prepare("SELECT key,project,file,generation,attempts,kind FROM jobs WHERE kind IN ('index','unindex') AND state='pending' ORDER BY generation,key")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(IndexClaim {
                key: r.get(0)?,
                project: r.get(1)?,
                file: r.get(2)?,
                generation: r.get(3)?,
                attempts: r.get(4)?,
                kind: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Compare-and-claim a snapshotted generation immediately before execution.
pub fn claim_rag(dir: &Path, row: &IndexClaim, now_ms: i64) -> Result<bool, OutboxError> {
    if !path(dir).exists() {
        return Ok(false);
    }
    let conn = open_claims(dir)?;
    Ok(conn.execute("UPDATE jobs SET state='running',claimed_at=?1,attempts=attempts+1 WHERE key=?2 AND generation=?3 AND state='pending' AND kind IN ('index','unindex')",params![now_ms,row.key,row.generation])?>0)
}

/// True only while this exact generation remains claimed and its project has
/// no tombstone. Publication must hold this transaction until its index commit.
pub fn index_current(conn: &Connection, row: &IndexClaim) -> Result<bool, OutboxError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE key=?1 AND generation=?2 AND state='running') AND NOT EXISTS(SELECT 1 FROM tombstones WHERE project=?3)", params![row.key,row.generation,row.project], |r|r.get(0))?)
}

/// Yield to foreground/ownership loss without charging a failed execution.
pub fn defer_index(dir: &Path, row: &IndexClaim) -> Result<(), OutboxError> {
    if !path(dir).exists() {
        return Ok(());
    }
    let conn = open_claims(dir)?;
    conn.execute("UPDATE jobs SET state='pending',claimed_at=NULL,attempts=MAX(0,attempts-1) WHERE key=?1 AND generation=?2 AND state='running'",params![row.key,row.generation])?;
    Ok(())
}

/// Only fixed operation/reason labels and a short opaque job hash enter errors.
#[derive(Clone, Copy, Debug)]
pub enum JobOperation {
    Notify,
    Prepare,
    Embed,
    Status,
    Ack,
    Defer,
    Claim,
}
impl JobOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Prepare => "prepare",
            Self::Embed => "embed",
            Self::Status => "status",
            Self::Ack => "ack",
            Self::Defer => "defer",
            Self::Claim => "claim",
        }
    }
}
#[derive(Clone, Debug)]
pub struct JobDiagnostic {
    pub job: String,
    pub operation: &'static str,
    pub reason: &'static str,
}
impl JobDiagnostic {
    pub fn new(row: &IndexClaim, operation: JobOperation, error: &str) -> Self {
        use sha2::{Digest, Sha256};
        let job = format!("{:x}", Sha256::digest(row.key.as_bytes()))
            .chars()
            .take(16)
            .collect();
        let lower = error
            .chars()
            .take(256)
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>();
        let reason = if lower.contains("superseded") {
            "source superseded"
        } else if lower.contains("ownership") {
            "ownership unavailable"
        } else if lower.contains("cancel") || lower.contains("stopped") {
            "cancelled"
        } else if lower.contains("incomplete") {
            "vectors incomplete"
        } else if lower.contains("databasebusy") || lower.contains("database is locked") {
            "database busy"
        } else {
            "operation unavailable"
        };
        Self {
            job,
            operation: operation.label(),
            reason,
        }
    }
    pub fn json(&self) -> String {
        serde_json::json!({"job":self.job,"operation":self.operation,"reason":self.reason})
            .to_string()
    }
    pub fn warning(&self) -> String {
        format!(
            "[index-outbox] job={} operation={} reason={}",
            self.job, self.operation, self.reason
        )
    }
}
fn ensure_error_column(conn: &mut Connection) -> Result<(), OutboxError> {
    fn present(conn: &Connection) -> Result<bool, rusqlite::Error> {
        let mut stmt = conn.prepare("PRAGMA table_info(jobs)")?;
        for name in stmt.query_map([], |r| r.get::<_, String>(1))? {
            if name? == "last_error" {
                return Ok(true);
            }
        }
        Ok(false)
    }
    if !present(conn)? {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !present(&tx)? {
            tx.execute_batch("ALTER TABLE jobs ADD COLUMN last_error TEXT")?;
        }
        tx.commit()?;
    }
    Ok(())
}
/// Recording an ack/status error is generation-bound and cannot expose arbitrary
/// upstream bodies, paths or credentials. Old named-column readers remain compatible.
pub fn record_error(
    dir: &Path,
    row: &IndexClaim,
    error: &JobDiagnostic,
) -> Result<(), OutboxError> {
    if !path(dir).exists() {
        return Ok(());
    }
    let conn = open_claims(dir)?;
    conn.execute(
        "UPDATE jobs SET last_error=?1 WHERE key=?2 AND generation=?3",
        params![error.json(), row.key, row.generation],
    )?;
    Ok(())
}
/// Acknowledge only the generation that actually ran; persist failure and its
/// sanitized diagnostic in the same transaction as the retry/give-up transition.
pub fn finish_index_with_error(
    dir: &Path,
    row: &IndexClaim,
    error: Option<&JobDiagnostic>,
) -> Result<(), OutboxError> {
    if !path(dir).exists() {
        return Ok(());
    }
    let conn = open_claims(dir)?;
    if let Some(error) = error {
        let state = if row.attempts.saturating_add(1) >= MAX_ATTEMPTS {
            "failed"
        } else {
            "pending"
        };
        conn.execute("UPDATE jobs SET state=?1,claimed_at=NULL,last_error=?2 WHERE key=?3 AND generation=?4 AND state='running'",params![state,error.json(),row.key,row.generation])?;
    } else {
        conn.execute(
            "DELETE FROM jobs WHERE key=?1 AND generation=?2 AND state='running'",
            params![row.key, row.generation],
        )?;
    }
    Ok(())
}
pub fn finish_index(dir: &Path, row: &IndexClaim, success: bool) -> Result<(), OutboxError> {
    let diagnostic =
        (!success).then(|| JobDiagnostic::new(row, JobOperation::Prepare, "unavailable"));
    finish_index_with_error(dir, row, diagnostic.as_ref())
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
    fn error_column_upgrades_legacy_jobs_without_rewriting_payload_and_is_generation_bound() {
        let dir = tempfile::tempdir().unwrap();
        let db = Connection::open(path(dir.path())).unwrap();
        db.execute_batch(&SCHEMA.replace(",\n  last_error TEXT", ""))
            .unwrap();
        enqueue(
            dir.path(),
            &[Job {
                kind: "index",
                project: "p",
                file: Some("private-name.md"),
                payload: Some("{\"synthetic\":true}".into()),
            }],
            1,
        )
        .unwrap();
        let row = claim_index(dir.path(), false, 2).unwrap().unwrap();
        let error = JobDiagnostic::new(
            &row,
            JobOperation::Embed,
            &"secret https://private.invalid/token\n".repeat(1000),
        );
        finish_index_with_error(dir.path(), &row, Some(&error)).unwrap();
        let (payload, stored): (String, String) = db
            .query_row("SELECT payload,last_error FROM jobs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(payload, "{\"synthetic\":true}");
        assert!(stored.len() < 256);
        assert!(!stored.contains("private"));
        assert!(!stored.contains("secret"));
        enqueue(dir.path(), &[index("p", "private-name.md")], 3).unwrap();
        record_error(dir.path(), &row, &error).unwrap();
        assert!(db
            .query_row("SELECT last_error FROM jobs", [], |r| r
                .get::<_, Option<String>>(0))
            .unwrap()
            .is_none());
    }

    #[test]
    fn native_rag_recovery_includes_running_unindex_and_is_tenant_isolated() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for d in [a.path(), b.path()] {
            enqueue(
                d,
                &[
                    index("p", "a.md"),
                    Job {
                        kind: "sweep",
                        project: "p",
                        file: None,
                        payload: None,
                    },
                    Job {
                        kind: "unindex",
                        project: "q",
                        file: Some("x"),
                        payload: None,
                    },
                ],
                1,
            )
            .unwrap();
            open(d)
                .unwrap()
                .execute(
                    "UPDATE jobs SET state='running',claimed_at=1,attempts=1",
                    [],
                )
                .unwrap();
        }
        let claim = claim_index(a.path(), true, 2).unwrap().unwrap();
        assert_eq!(claim.attempts, 1);
        let rows = jobs(a.path()).unwrap();
        assert!(rows
            .iter()
            .any(|r| r.kind == "sweep" && r.state == "running"));
        assert!(rows
            .iter()
            .any(|r| r.kind == "unindex" && r.state == "pending"));
        finish_index(a.path(), &claim, true).unwrap();
        assert_eq!(jobs(a.path()).unwrap().len(), 2);
        assert_eq!(jobs(b.path()).unwrap().len(), 3);
        let removal = claim_index(a.path(), false, 3).unwrap().unwrap();
        assert_eq!(removal.kind, "unindex");
        assert_eq!(removal.attempts, 1);
        finish_index(a.path(), &removal, true).unwrap();
        assert!(claim_index(a.path(), false, 4).unwrap().is_none());
        assert_eq!(jobs(a.path()).unwrap()[0].kind, "sweep");
    }
    #[test]
    fn each_pending_generation_is_attempted_once_per_drain_snapshot() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a"), index("p", "b")], 1).unwrap();
        let pass = pending_rag(d.path(), false).unwrap();
        assert_eq!(pass.len(), 2);
        for row in &pass {
            assert!(claim_rag(d.path(), row, 2).unwrap());
            finish_index(d.path(), row, false).unwrap();
        }
        let next = pending_rag(d.path(), false).unwrap();
        assert_eq!(next.len(), 2);
        assert!(next.iter().all(|r| r.attempts == 1));
        enqueue(d.path(), &[index("p", "a")], 3).unwrap();
        let stale = pass
            .iter()
            .find(|r| r.file.as_deref() == Some("a"))
            .unwrap();
        assert!(!claim_rag(d.path(), stale, 4).unwrap());
    }
    #[test]
    fn native_acknowledgement_never_recreates_an_unlinked_outbox() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a")], 1).unwrap();
        let row = claim_index(d.path(), false, 2).unwrap().unwrap();
        std::fs::remove_file(path(d.path())).unwrap();
        finish_index(d.path(), &row, false).unwrap();
        defer_index(d.path(), &row).unwrap();
        assert!(!path(d.path()).exists());
    }
    #[test]
    fn yielding_does_not_exhaust_the_retry_budget_or_ack_a_newer_generation() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a")], 1).unwrap();
        for _ in 0..MAX_ATTEMPTS + 2 {
            let row = claim_index(d.path(), false, 2).unwrap().unwrap();
            assert_eq!(row.attempts, 0);
            defer_index(d.path(), &row).unwrap();
        }
        assert_eq!(jobs(d.path()).unwrap()[0].state, "pending");
    }
    #[test]
    fn native_index_claim_retry_bound_and_new_generation_survives_ack() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a")], 1).unwrap();
        for n in 0..MAX_ATTEMPTS {
            let row = claim_index(d.path(), false, n).unwrap().unwrap();
            assert_eq!(row.attempts, n);
            finish_index(d.path(), &row, false).unwrap();
        }
        assert!(claim_index(d.path(), false, 10).unwrap().is_none());
        assert_eq!(jobs(d.path()).unwrap()[0].state, "failed");
        enqueue(d.path(), &[index("p", "a")], 11).unwrap();
        let old = claim_index(d.path(), false, 12).unwrap().unwrap();
        enqueue(d.path(), &[index("p", "a")], 13).unwrap();
        assert!(!index_current(&open(d.path()).unwrap(), &old).unwrap());
        finish_index(d.path(), &old, true).unwrap();
        let newer = claim_index(d.path(), false, 14).unwrap().unwrap();
        assert!(newer.generation > old.generation);
        tombstone(d.path(), "p", None, 15).unwrap();
        assert!(!index_current(&open(d.path()).unwrap(), &newer).unwrap());
        finish_index(d.path(), &newer, false).unwrap();
        assert!(jobs(d.path()).unwrap().is_empty());
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
    fn a_listed_project_is_revived_and_an_unlisted_one_stays_dead() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a.md")], 1).unwrap();
        tombstone(d.path(), "p", None, 2).unwrap();
        tombstone(d.path(), "q", None, 3).unwrap();
        let listed = js_json::parse(r#"[{"id":"p"},{"id":"r"},{"name":"no id"}]"#).unwrap();
        let listed = match listed {
            js_json::JValue::Arr(items) => items,
            _ => Vec::new(),
        };
        assert_eq!(listed.len(), 3);
        assert_eq!(revive_listed(d.path(), &listed).unwrap(), 1);
        assert_eq!(tombstoned(d.path(), "p").unwrap(), None);
        assert!(tombstoned(d.path(), "q").unwrap().is_some());
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(revive_listed(empty.path(), &listed).unwrap(), 0);
        assert!(!path(empty.path()).exists());
    }

    #[test]
    fn nothing_is_created_for_a_read_of_a_missing_outbox() {
        let d = tempfile::tempdir().unwrap();
        assert!(jobs(d.path()).unwrap().is_empty());
        assert_eq!(tombstoned(d.path(), "p").unwrap(), None);
        revive(d.path(), "p").unwrap();
        assert!(!path(d.path()).exists());
    }

    #[test]
    fn a_kind_drains_only_its_own_jobs_and_retries_then_fails() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a.md")], 1).unwrap();
        tombstone(d.path(), "q", Some("{\"folder\":\"r/q\"}".into()), 2).unwrap();
        let mut seen = Vec::new();
        let r = drain_kind(d.path(), "sweep", true, 3, |c| {
            seen.push((c.project.clone(), c.payload.clone()));
            false
        })
        .unwrap();
        assert_eq!(r, (0, 1));
        assert_eq!(
            seen,
            vec![("q".to_string(), Some("{\"folder\":\"r/q\"}".to_string()))]
        );
        for _ in 0..(MAX_ATTEMPTS - 1) {
            drain_kind(d.path(), "sweep", false, 4, |_| false).unwrap();
        }
        let rows = jobs(d.path()).unwrap();
        let sweep = rows.iter().find(|r| r.kind == "sweep").unwrap();
        assert_eq!(sweep.state, "failed");
        let index_row = rows.iter().find(|r| r.kind == "index").unwrap();
        assert_eq!(
            index_row.state, "pending",
            "never claimed by the sweep drain"
        );
        // Queued again: pending at a new generation, and an acknowledged run deletes it.
        tombstone(d.path(), "q", Some("{}".into()), 5).unwrap();
        assert_eq!(
            drain_kind(d.path(), "sweep", false, 6, |_| true).unwrap(),
            (1, 0)
        );
        assert!(jobs(d.path()).unwrap().iter().all(|r| r.kind == "index"));
    }

    #[test]
    fn recovery_takes_back_only_its_own_kind() {
        let d = tempfile::tempdir().unwrap();
        enqueue(d.path(), &[index("p", "a.md")], 1).unwrap();
        tombstone(d.path(), "q", Some("{}".into()), 2).unwrap();
        let conn = open(d.path()).unwrap();
        conn.execute("UPDATE jobs SET state = 'running'", [])
            .unwrap();
        drop(conn);
        assert_eq!(
            drain_kind(d.path(), "sweep", true, 3, |_| true).unwrap(),
            (1, 0)
        );
        let rows = jobs(d.path()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].kind.as_str(), rows[0].state.as_str()),
            ("index", "running")
        );
        assert_eq!(
            drain_kind(
                tempfile::tempdir().unwrap().path(),
                "sweep",
                true,
                1,
                |_| true
            )
            .unwrap(),
            (0, 0)
        );
    }
}
