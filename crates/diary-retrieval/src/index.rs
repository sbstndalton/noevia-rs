//! The chunk index and its search (Python `retrieval.Retriever`).
//!
//! Chunk unit: each `### <time/topic>` subsection, prefixed with its day header
//! (`diary_corpus_parse::parse_chunks`). Rows are keyed by (file, content hash), so a re-index
//! embeds only new or changed chunks and deletes rows whose text no longer appears (with their
//! vectors in every slot). Search uses the active model's slot only.
//!
//! Embedding can take minutes against a slow server, and the index file is shared with the
//! write-ahead journal, so a re-index embeds first ([`Retriever::plan`] reads, then
//! [`PendingPlan::embed`] runs without the connection) and writes in one short IMMEDIATE
//! transaction ([`Retriever::apply_reindex`]); the Python service does the same (#858).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::slots::{self, Slot};
use crate::{Error, Publication, Result, Target};

/// Python `retrieval._SCHEMA`, byte for byte.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS dirty_documents (document TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS chunks (
    id INTEGER PRIMARY KEY,
    xid TEXT,                    -- exchange xid if the chunk came from a logged exchange
    day TEXT NOT NULL,           -- ISO date of the owning day section
    file TEXT NOT NULL,          -- source month file
    header TEXT NOT NULL,        -- '### ' header text (time — topic)
    body TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_chunks_day ON chunks(day);
CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_hash ON chunks(file, content_hash);

CREATE TABLE IF NOT EXISTS vec_meta (
    key TEXT PRIMARY KEY,
    value TEXT
);
";

/// The objects `SCHEMA` creates, in its order (Python reads them with a regex).
const SCHEMA_OBJECTS: [&str; 5] = [
    "dirty_documents",
    "chunks",
    "idx_chunks_day",
    "idx_chunks_hash",
    "vec_meta",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Document,
    Query,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Query => "query",
        }
    }
}

/// The embedding seen by the index: the active target, and a call that embeds texts with a given
/// target. An error is a failed call (logged by the caller as a fixed message, retried later).
pub trait Embedder {
    fn active(&self) -> Target;
    fn embed(
        &self,
        target: &Target,
        texts: &[&str],
        kind: Kind,
    ) -> std::result::Result<Vec<Vec<f64>>, String>;
}

/// Python `_embed_one`: the first vector, or None when the call failed or returned nothing.
pub fn embed_one<E: Embedder + ?Sized>(
    embedder: &E,
    text: &str,
    target: &Target,
    kind: Kind,
) -> Option<Vec<f64>> {
    match embedder.embed(target, &[text], kind) {
        Ok(out) => out.into_iter().next(),
        Err(_) => None,
    }
}

/// One parsed chunk: (day, header, chunk text, content hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub day: String,
    pub header: String,
    pub text: String,
    pub content_hash: String,
}

/// Parsed chunks of one document plus the embeddings computed for them (Python `ReindexPlan`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReindexPlan {
    pub file_name: String,
    pub rows: Vec<Row>,
    /// content hash -> vector
    pub embeddings: HashMap<String, Vec<f64>>,
    /// Some chunk could not be embedded this pass.
    pub pending: bool,
    /// The embedding target the vectors were made with.
    pub target: Option<Target>,
}

/// A plan read from the index but not yet embedded: needs no connection to finish.
#[derive(Debug, Clone)]
pub struct PendingPlan {
    file_name: String,
    rows: Vec<Row>,
    target: Target,
    vec_available: bool,
    dim: Option<usize>,
    /// content hash -> has a current vector for `target`
    indexed: HashMap<String, bool>,
}

impl PendingPlan {
    /// Embeds the new and changed chunks (Python `prepare_reindex` after its reads).
    pub fn embed<E: Embedder + ?Sized>(self, embedder: &E) -> ReindexPlan {
        let mut embeddings: HashMap<String, Vec<f64>> = HashMap::new();
        let mut pending = false;
        let mut dim = self.dim;
        if self.vec_available {
            for row in &self.rows {
                let h = &row.content_hash;
                if embeddings.contains_key(h) || self.indexed.get(h).copied().unwrap_or(false) {
                    continue;
                }
                let emb = embed_one(embedder, &row.text, &self.target, Kind::Document);
                if let (Some(e), None) = (&emb, dim) {
                    dim = Some(e.len()); // the first vector fixes a new slot's width
                }
                match emb {
                    Some(e) if Some(e.len()) == dim => {
                        embeddings.insert(h.clone(), e);
                    }
                    _ => pending = true,
                }
            }
        }
        ReindexPlan {
            file_name: self.file_name,
            rows: self.rows,
            embeddings,
            pending,
            target: Some(self.target),
        }
    }
}

/// One search hit. `id` is set only when asked for (retrieval tracing).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: Option<i64>,
    pub day: String,
    pub header: String,
    pub text: String,
    pub score: f64,
}

/// Why a search came back empty (Python `diag["status"]`); None when it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchStatus {
    IndexUnavailable,
    EmbedFailed,
    EmbedDimMismatch,
    SearchError,
}

impl SearchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IndexUnavailable => "index_unavailable",
            Self::EmbedFailed => "embed_failed",
            Self::EmbedDimMismatch => "embed_dim_mismatch",
            Self::SearchError => "search_error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    pub chunks: i64,
    pub vec_available: bool,
    pub dim: Option<usize>,
    pub model: String,
    pub profile: String,
}

/// Options the caller decides from the tenant's writer state.
pub struct Options {
    /// Whether a read may adopt the legacy table (Python: not a guarded writer tenant without its
    /// writer lock). A re-index always adopts, under its publication guard.
    pub adopt_on_read: bool,
    /// SQLite busy timeout (Python `timeout=30`).
    pub busy_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            adopt_on_read: true,
            busy_timeout: Duration::from_secs(30),
        }
    }
}

pub struct Retriever<E: Embedder, P: Publication> {
    conn: Connection,
    embedder: E,
    publication: P,
    options: Options,
    vec_available: bool,
    dim: Option<usize>,
    pending_embeddings: bool,
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Python `round(x, 4)`: correctly rounded, ties to even on the exact binary value. Rust's
/// fixed-precision formatting rounds the exact value the same way.
pub fn round4(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.4}").parse().unwrap_or(x)
}

/// Whether every object `SCHEMA` creates is present (a read; Python checks `sqlite_master` the
/// same way before it takes any guard).
pub fn schema_complete(conn: &Connection) -> Result<bool> {
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master")?;
    let have: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, Option<String>>(0))?
        .filter_map(|r| r.ok().flatten())
        .collect();
    Ok(SCHEMA_OBJECTS.iter().all(|n| have.contains(*n)))
}

/// Creates the retrieval schema objects that are missing, writing nothing when all exist.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    if schema_complete(conn)? {
        return Ok(());
    }
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// Runs `body` in a `BEGIN IMMEDIATE` transaction: committed when it succeeds, rolled back when
/// it or the commit fails.
fn immediate<T>(conn: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match body() {
        Ok(value) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(value),
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e.into())
            }
        },
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn vec_works(conn: &Connection) -> bool {
    conn.query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
        .is_ok()
}

impl<E: Embedder, P: Publication> Retriever<E, P> {
    /// Opens (creating if needed) an index file. The schema is written only when an object is
    /// missing, inside `publication`; vector support missing leaves search empty, never an error.
    pub fn open(path: &Path, embedder: E, publication: P, options: Options) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|_| Error::Sqlite("CannotOpen".into()))?;
            }
        }
        let registered = sqlite_vec_loader::register().is_ok();
        let conn = Connection::open(path)?;
        conn.busy_timeout(options.busy_timeout)?;
        Self::with_connection(conn, registered, embedder, publication, options)
    }

    /// As [`Retriever::open`] for an index file that must already exist: never creates the file
    /// or its directory (a missing file is an error).
    pub fn open_existing(
        path: &Path,
        embedder: E,
        publication: P,
        options: Options,
    ) -> Result<Self> {
        let registered = sqlite_vec_loader::register().is_ok();
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(options.busy_timeout)?;
        Self::with_connection(conn, registered, embedder, publication, options)
    }

    /// As [`Retriever::open`] on an already opened connection. `vec_registered` says whether the
    /// vector extension was registered before it was opened.
    pub fn with_connection(
        conn: Connection,
        vec_registered: bool,
        embedder: E,
        publication: P,
        options: Options,
    ) -> Result<Self> {
        // Opening an index whose schema is in place writes nothing and takes no guard.
        if !schema_complete(&conn)? {
            publication.run(&mut || ensure_schema(&conn))?;
        }
        let vec_available = vec_registered && vec_works(&conn);
        let mut me = Self {
            conn,
            embedder,
            publication,
            options,
            vec_available,
            dim: None,
            pending_embeddings: false,
        };
        if me.vec_available {
            me.active_slot()?;
        }
        Ok(me)
    }

    pub fn embedder(&self) -> &E {
        &self.embedder
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn vec_available(&self) -> bool {
        self.vec_available
    }

    /// Whether some chunk of the last re-index still needs a retry.
    pub fn pending_embeddings(&self) -> bool {
        self.pending_embeddings
    }

    /// The active slot's width after the last slot lookup.
    pub fn dim(&self) -> Option<usize> {
        self.dim
    }

    /// The active target and its slot (None until it has vectors here). May adopt the legacy
    /// table when reads may (see [`Options::adopt_on_read`]) and no transaction is open.
    pub fn active_slot(&mut self) -> Result<(Target, Option<Slot>)> {
        let target = self.embedder.active();
        if !self.vec_available {
            return Ok((target, None));
        }
        let needs = self.options.adopt_on_read
            && self.conn.is_autocommit()
            && slots::needs_legacy_adoption(&self.conn, Some(&target))?;
        if needs {
            let conn = &self.conn;
            self.publication.run(&mut || {
                // An open index transaction must not receive a mutation whose COMMIT we do not own.
                if !conn.is_autocommit() {
                    return Ok(());
                }
                // IMMEDIATE: the write lock is taken (with the busy timeout) before the reads, so a
                // journal write in between cannot turn the upgrade into an immediate BUSY.
                immediate(conn, || slots::adopt_legacy(conn, Some(&target)))
            })?;
        }
        let slot = slots::find_slot(&self.conn, Some(&target))?;
        self.dim = slot.as_ref().map(|s| s.dim);
        Ok((target, slot))
    }

    /// {content hash: (chunk id, has a current vector for `target`)} for one file. Read-only.
    fn indexed(
        &self,
        file_name: &str,
        target: Option<&Target>,
    ) -> Result<HashMap<String, (i64, bool)>> {
        let slot = if self.vec_available && target.is_some() {
            slots::find_slot(&self.conn, target)?
        } else {
            None
        };
        let mut stmt = self
            .conn
            .prepare("SELECT id, content_hash FROM chunks WHERE file = ?")?;
        let rows = stmt
            .query_map([file_name], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = HashMap::new();
        for (id, hash) in rows {
            let has = slot.is_some() && slots::has_vector(&self.conn, slot.as_ref(), id, &hash)?;
            out.insert(hash, (id, has));
        }
        Ok(out)
    }

    /// Parses one document and reads what is indexed for it; [`PendingPlan::embed`] finishes the
    /// plan without the connection.
    pub fn plan(&mut self, file_name: &str, month_text: Option<&str>) -> Result<PendingPlan> {
        let rows = diary_corpus_parse::parse_chunks(month_text)
            .into_iter()
            .map(|c| Row {
                day: c.day,
                header: c.header,
                text: c.text,
                content_hash: c.content_hash,
            })
            .collect();
        let (target, slot) = self.active_slot()?;
        let indexed = self
            .indexed(file_name, Some(&target))?
            .into_iter()
            .map(|(h, (_, has))| (h, has))
            .collect();
        Ok(PendingPlan {
            file_name: file_name.to_owned(),
            rows,
            target,
            vec_available: self.vec_available,
            dim: slot.map(|s| s.dim),
            indexed,
        })
    }

    /// Python `prepare_reindex`: [`Retriever::plan`] then [`PendingPlan::embed`] with this index's
    /// own embedder.
    pub fn prepare_reindex(
        &mut self,
        file_name: &str,
        month_text: Option<&str>,
    ) -> Result<ReindexPlan> {
        let pending = self.plan(file_name, month_text)?;
        Ok(pending.embed(&self.embedder))
    }

    /// Writes a prepared plan in one short transaction. Returns the chunks embedded.
    ///
    /// The index is re-read inside the transaction, so a plan whose state moved on (another
    /// re-index of the same file landed first) still applies correctly. The plan's vectors go to
    /// the slot of the target they were made with, never to another model's.
    pub fn apply_reindex(&mut self, plan: &ReindexPlan) -> Result<usize> {
        let target = plan
            .target
            .clone()
            .unwrap_or_else(|| self.embedder.active());
        let active = self.embedder.active();
        let mut outcome: Option<(usize, bool, Option<Slot>)> = None;
        {
            let this = &*self;
            this.publication.run(&mut || {
                // Python commits a caller's open transaction inside the guard, then begins its own.
                if !this.conn.is_autocommit() {
                    this.conn.execute_batch("COMMIT")?;
                }
                outcome = Some(immediate(&this.conn, || {
                    this.apply_body(plan, &target, &active)
                })?);
                Ok(())
            })?;
        }
        let Some((embedded, pending, slot)) = outcome else {
            return Err(Error::Busy("publication did not run".into()));
        };
        if self.vec_available && target == self.embedder.active() {
            if let Some(slot) = slot {
                self.dim = Some(slot.dim);
            }
        }
        self.pending_embeddings = pending;
        Ok(embedded)
    }

    fn apply_body(
        &self,
        plan: &ReindexPlan,
        target: &Target,
        active: &Target,
    ) -> Result<(usize, bool, Option<Slot>)> {
        let conn = &self.conn;
        let vec = self.vec_available;
        let keep: HashSet<&str> = plan.rows.iter().map(|r| r.content_hash.as_str()).collect();
        let mut pending = plan.pending;
        let mut embedded = 0usize;
        if vec {
            slots::adopt_legacy(conn, Some(active))?;
        }
        let mut slot = if vec {
            slots::find_slot(conn, Some(target))?
        } else {
            None
        };
        let indexed = self.indexed(&plan.file_name, Some(target))?;
        // Supersede cleanup runs even when nothing new is embedded: a stored row whose hash is no
        // longer in the parsed text is a stale version or removed content. Delete it, with its
        // vectors in every slot, so retrieval cannot serve the old text next to the corrected one.
        let mut stale: Vec<i64> = indexed
            .iter()
            .filter(|(h, _)| !keep.contains(h.as_str()))
            .map(|(_, (id, _))| *id)
            .collect();
        stale.sort_unstable();
        if vec {
            slots::drop_chunks(conn, &stale)?;
        }
        for id in &stale {
            conn.execute("DELETE FROM chunks WHERE id = ?", [id])?;
        }
        let mut done: HashSet<&str> = HashSet::new();
        for row in &plan.rows {
            let h = row.content_hash.as_str();
            if !done.insert(h) {
                continue;
            }
            let existing = indexed.get(h).copied();
            if let Some((_, has)) = existing {
                if !vec || has {
                    continue;
                }
            }
            let emb = if vec { plan.embeddings.get(h) } else { None };
            if vec {
                let width_ok = match (emb, &slot) {
                    (None, _) => false,
                    (Some(e), Some(s)) => e.len() == s.dim,
                    (Some(_), None) => true,
                };
                if !width_ok {
                    // A failed embed inserts no row: the hash-indexed row is the dedupe record, so
                    // inserting it would make this chunk skip every future re-index.
                    pending = true;
                    continue;
                }
                if slot.is_none() {
                    let width = emb.map_or(0, Vec::len);
                    slot = Some(slots::ensure_slot(conn, target, width)?);
                }
            }
            let chunk_id = match existing {
                // A row without a vector for this model: keep it (other models' vectors point at
                // it) and add the missing vector.
                Some((id, _)) => id,
                None => {
                    conn.execute(
                        "INSERT INTO chunks (day, file, header, body, content_hash) VALUES (?, ?, ?, ?, ?)",
                        params![row.day, plan.file_name, row.header, row.text, h],
                    )?;
                    conn.last_insert_rowid()
                }
            };
            if let (Some(e), Some(s)) = (emb, &slot) {
                // A new id may be one SQLite freed earlier: clear any vector another model left on it.
                if existing.is_none() {
                    slots::drop_chunks(conn, &[chunk_id])?;
                }
                slots::write_vectors(conn, s, &[(chunk_id, h, e.as_slice())])?;
            }
            embedded += 1;
        }
        Ok((embedded, pending, slot))
    }

    /// Python `reindex_file`: embed, then write in one short transaction.
    pub fn reindex_file(&mut self, file_name: &str, month_text: Option<&str>) -> Result<usize> {
        let plan = self.prepare_reindex(file_name, month_text)?;
        self.apply_reindex(&plan)
    }

    /// The active target and slot a search would use, or the empty-result status. The query can
    /// then be embedded outside any lock and passed to [`Retriever::search_vector`].
    pub fn search_slot(&mut self) -> Result<std::result::Result<(Target, Slot), SearchStatus>> {
        if !self.vec_available {
            return Ok(Err(SearchStatus::IndexUnavailable));
        }
        let (target, slot) = self.active_slot()?;
        Ok(match slot {
            Some(slot) => Ok((target, slot)),
            None => Err(SearchStatus::IndexUnavailable),
        })
    }

    /// Cosine-similarity search over past chunks with this index's own embedder.
    pub fn search(
        &mut self,
        query: &str,
        top_k: i64,
        min_score: f64,
        with_ids: bool,
    ) -> Result<(Vec<Hit>, Option<SearchStatus>)> {
        let (target, slot) = match self.search_slot()? {
            Ok(found) => found,
            Err(status) => return Ok((Vec::new(), Some(status))),
        };
        let q = embed_one(&self.embedder, query, &target, Kind::Query);
        self.search_vector(q.as_deref(), top_k, min_score, &slot, with_ids)
    }

    /// The vector half of a search. A SQLite failure is the `search_error` status.
    pub fn search_vector(
        &self,
        q: Option<&[f64]>,
        top_k: i64,
        min_score: f64,
        slot: &Slot,
        with_ids: bool,
    ) -> Result<(Vec<Hit>, Option<SearchStatus>)> {
        let Some(q) = q else {
            return Ok((Vec::new(), Some(SearchStatus::EmbedFailed)));
        };
        if q.len() != slot.dim {
            return Ok((Vec::new(), Some(SearchStatus::EmbedDimMismatch)));
        }
        let blob = slots::serialize_f32(q);
        let sql = format!(
            "
                SELECT c.id, c.day, c.header, c.body,
                       1.0 - vec_distance_cosine(v.embedding, ?) AS score
                FROM {}
                WHERE c.file NOT IN (SELECT document FROM dirty_documents)
                  AND 1.0 - vec_distance_cosine(v.embedding, ?) >= ?
                ORDER BY score DESC
                LIMIT ?
                ",
            slots::join_sql(slot)
        );
        let run = || -> rusqlite::Result<Vec<Hit>> {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![blob, blob, min_score, top_k], |r| {
                Ok(Hit {
                    id: if with_ids { Some(r.get(0)?) } else { None },
                    day: r.get(1)?,
                    header: r.get(2)?,
                    text: r.get(3)?,
                    score: round4(r.get(4)?),
                })
            })?;
            rows.collect()
        };
        match run() {
            Ok(hits) => Ok((hits, None)),
            Err(_) => Ok((Vec::new(), Some(SearchStatus::SearchError))),
        }
    }

    pub fn stats(&self) -> Result<Stats> {
        let chunks: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        let target = self.embedder.active();
        let slot = if self.vec_available {
            slots::find_slot(&self.conn, Some(&target))?
        } else {
            None
        };
        Ok(Stats {
            chunks,
            vec_available: self.vec_available,
            dim: slot.map(|s| s.dim),
            model: target.model,
            profile: target.profile,
        })
    }
}

/// Content hash of a chunk text (Python `Retriever._hash`).
pub fn content_hash(text: &str) -> String {
    sha256_hex(text)
}

#[cfg(test)]
mod tests {
    use super::round4;

    #[test]
    fn round4_matches_python_round() {
        // Values recorded with Python 3: round(x, 4).
        assert_eq!(round4(0.03125), 0.0312);
        assert_eq!(round4(0.722_649_99), 0.7226);
        assert_eq!(round4(0.722_650_01), 0.7227);
        assert_eq!(round4(-0.000_049), -0.0);
        assert_eq!(round4(1.0), 1.0);
    }
}
