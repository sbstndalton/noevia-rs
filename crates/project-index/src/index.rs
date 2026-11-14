//! Index writes of rag.cjs: `openIndex`, `dropFileRows`, `embedRowsInto`, `indexProjectFileNow`,
//! `appendFileTailNow` and `deleteProjectFile`, on one project index file.
//!
//! The account and lifecycle checks (`userActive`, the front's tombstones), the per-file queue and
//! the embedding request are the caller's: they come in as an opener, an `is_current` check and an
//! [`Embed`]. Node logs and swallows an embedding failure of a head index ("text stored, vectors
//! pending"); here it is returned in [`IndexOutcome::embed_error`] for the caller to log.

use crate::chunk::{hash, hash_units, to_text, units, DIRECT_INJECT_MAX};
use crate::embed::Vector;
use crate::slots::{self, Slot, Target, VectorRow};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

type Res<T> = Result<T, String>;
fn err(e: rusqlite::Error) -> String {
    format!("index: {e}")
}

/// A tail chunk (`meta` `{"tail":1,"page":N}`).
pub const TAIL_ROW: &str = "COALESCE(meta, '') LIKE '{\"tail\":1,%'";
/// A text chunk (every row written before the `kind` column is one).
pub const TEXT_ROW: &str = "COALESCE(kind, 'text') = 'text'";
/// Texts per embed request of a head index or re-index.
pub const EMBED_BATCH: usize = 8;
/// Texts per embed request of a tail append (`TAIL_EMBED_REQUEST`).
pub const TAIL_EMBED_REQUEST: usize = 4;

/// The embedding call for document chunks (framed for `target`'s profile by the implementation).
pub trait Embed {
    fn embed(&mut self, target: &Target, texts: &[String]) -> Res<Vec<Vector>>;
    /// Node's warning `[rag] <model> returned <got>-wide vectors but its slot holds <slot>; skipped`:
    /// a batch stored fewer vectors than it had rows and some vector did not fit the slot. Node logs it;
    /// the caller logs it here (`got` is the first vector's width, None when it had none).
    fn width_skipped(&mut self, _model: &str, _got: Option<usize>, _slot: usize) {}
}

impl<F: FnMut(&Target, &[String]) -> Res<Vec<Vector>>> Embed for F {
    fn embed(&mut self, target: &Target, texts: &[String]) -> Res<Vec<Vector>> {
        self(target, texts)
    }
}

/// An open project index: the connection, the active model and its slot (None until it has one).
pub struct Index {
    pub db: Connection,
    pub active: Target,
    pub slot: Option<Slot>,
}

const SCHEMA: &str = "
      CREATE TABLE IF NOT EXISTS chunks (
        id INTEGER PRIMARY KEY,
        file TEXT NOT NULL,
        chunk_no INTEGER NOT NULL,
        body TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_file_hash ON chunks(file, content_hash);
      CREATE TABLE IF NOT EXISTS vec_meta (key TEXT PRIMARY KEY, value TEXT);
    ";

/// `openIndex` after its account and tombstone checks: opens (creating the directory and file),
/// sets WAL, creates the schema, adds the later columns and adopts the legacy table. `deleted` is
/// asked once more after the open: a project deleted in between loses the file the open recreated.
/// The sqlite-vec extension must be registered (`sqlite-vec-loader`) before the first open.
pub fn open_index(path: &Path, active: &Target, deleted: &dyn Fn() -> bool) -> Res<Option<Index>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("index: {e}"))?;
    }
    let db = Connection::open(path).map_err(err)?;
    if deleted() {
        drop(db);
        let name = path.as_os_str().to_os_string();
        for suffix in ["", "-wal", "-shm"] {
            let mut f = name.clone();
            f.push(suffix);
            let _ = std::fs::remove_file(&f);
        }
        return Ok(None);
    }
    db.pragma_update(None, "journal_mode", "WAL").map_err(err)?;
    db.execute_batch(SCHEMA).map_err(err)?;
    let columns: Vec<String> = {
        let mut st = db.prepare("PRAGMA table_info(chunks)").map_err(err)?;
        let names = st.query_map([], |r| r.get::<_, String>(1)).map_err(err)?;
        names.collect::<Result<_, _>>().map_err(err)?
    };
    for (col, ty) in [
        ("version", "TEXT"),
        ("direct", "INTEGER"),
        ("kind", "TEXT"),
        ("meta", "TEXT"),
    ] {
        if !columns.iter().any(|c| c == col) {
            db.execute_batch(&format!("ALTER TABLE chunks ADD COLUMN {col} {ty}"))
                .map_err(err)?;
        }
    }
    slots::adopt_legacy(&db, active)?;
    let slot = slots::find_slot(&db, active)?;
    Ok(Some(Index {
        db,
        active: active.clone(),
        slot,
    }))
}

fn ids(db: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Res<Vec<i64>> {
    let mut st = db.prepare(sql).map_err(err)?;
    let rows = st.query_map(args, |r| r.get::<_, i64>(0)).map_err(err)?;
    rows.collect::<Result<_, _>>().map_err(err)
}

/// `dropFileRows`: one file's chunks and their vectors in every slot. `text_only` keeps image rows;
/// `keep_tail_of` keeps the tail rows cut from that version. The caller holds a transaction.
pub fn drop_file_rows(
    index: &Index,
    file: &str,
    text_only: bool,
    keep_tail_of: Option<&str>,
) -> Res<()> {
    let mut which = if text_only {
        format!(" AND {TEXT_ROW}")
    } else {
        String::new()
    };
    if keep_tail_of.is_some() {
        which.push_str(&format!(" AND NOT ({TAIL_ROW} AND version IS ?)"));
    }
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![&file];
    if let Some(v) = &keep_tail_of {
        args.push(v);
    }
    let gone = ids(
        &index.db,
        &format!("SELECT id FROM chunks WHERE file = ?{which}"),
        &args,
    )?;
    slots::drop_chunks(&index.db, &gone)?;
    index
        .db
        .execute(
            &format!("DELETE FROM chunks WHERE file = ?{which}"),
            args.as_slice(),
        )
        .map_err(err)?;
    Ok(())
}

/// A chunk to embed.
#[derive(Clone, Debug)]
pub struct Row {
    pub id: i64,
    pub body: String,
    pub hash: String,
    pub fresh: bool,
}

/// `embedRowsInto`: embeds rows in batches with `target` and stores each vector while its chunk still
/// holds the embedded text. Returns how many vectors were stored; an embedding error ends the call.
pub fn embed_rows_into(
    index: &mut Index,
    target: &Target,
    rows: &[Row],
    is_current: &mut dyn FnMut() -> bool,
    batch_size: usize,
    embed: &mut dyn Embed,
) -> Res<usize> {
    let mut embedded = 0;
    for batch in rows.chunks(batch_size.max(1)) {
        if !is_current() {
            break;
        }
        let bodies: Vec<String> = batch.iter().map(|r| r.body.clone()).collect();
        let vectors = embed.embed(target, &bodies)?;
        // A newer run (or a delete) replaced these chunks while we were embedding.
        if !is_current() {
            break;
        }
        let dim = vectors
            .first()
            .and_then(|v| v.as_ref().map(Vec::len))
            .unwrap_or(0);
        let tx = index.db.unchecked_transaction().map_err(err)?;
        let slot = slots::ensure_slot(&tx, target, dim)?;
        let mut items = Vec::new();
        for (j, r) in batch.iter().enumerate() {
            let Some(Some(vector)) = vectors.get(j) else {
                continue;
            };
            if vector.len() != slot.dim {
                continue;
            }
            let still = tx
                .query_row(
                    "SELECT 1 FROM chunks WHERE id = ? AND content_hash = ?",
                    params![r.id, r.hash],
                    |_| Ok(()),
                )
                .optional()
                .map_err(err)?
                .is_some();
            if still {
                items.push(VectorRow {
                    id: r.id,
                    hash: r.hash.clone(),
                    vector: vector.clone(),
                });
            }
        }
        slots::write_vectors(&tx, &slot, &items)?;
        tx.commit().map_err(err)?;
        if items.len() < batch.len()
            && vectors
                .iter()
                .any(|v| v.as_ref().map(Vec::len) != Some(slot.dim))
        {
            let got = vectors.first().and_then(|v| v.as_ref().map(Vec::len));
            embed.width_skipped(&target.model, got, slot.dim);
        }
        if slots::slot_id(&index.active) == slots::slot_id(target) {
            index.slot = Some(slot);
        }
        embedded += items.len();
    }
    Ok(embedded)
}

/// What an index run returns (`{ ok, stored, embedded, direct?, superseded?, reason? }`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexOutcome {
    pub ok: bool,
    pub stored: usize,
    pub embedded: usize,
    pub direct: bool,
    pub superseded: bool,
    /// `rag-unavailable` when the index could not be opened.
    pub reason: Option<String>,
    /// The embedding failure Node logs ("text stored, vectors pending").
    pub embed_error: Option<String>,
}

impl IndexOutcome {
    /// The outcome as Node returns it (without the logged error).
    pub fn to_json(&self) -> Value {
        if !self.ok {
            return json!({ "ok": false, "reason": self.reason });
        }
        let mut v = serde_json::Map::new();
        v.insert("ok".into(), json!(true));
        v.insert("stored".into(), json!(self.stored));
        v.insert("embedded".into(), json!(self.embedded));
        if self.direct {
            v.insert("direct".into(), json!(true));
        }
        if self.superseded {
            v.insert("superseded".into(), json!(true));
        }
        Value::Object(v)
    }
}

/// `indexProjectFileNow`: index (or re-index) one file at `version` (sha256 of the full text).
/// `open` is `openIndex` with its checks (None: no index can be written).
pub fn index_file_now(
    open: &mut dyn FnMut() -> Res<Option<Index>>,
    file: &str,
    text: &[u16],
    version: &str,
    is_current: &mut dyn FnMut() -> bool,
    embed: &mut dyn Embed,
) -> IndexOutcome {
    let superseded = IndexOutcome {
        ok: true,
        superseded: true,
        ..Default::default()
    };
    if !is_current() {
        return superseded;
    }
    let Ok(Some(mut index)) = open() else {
        return IndexOutcome {
            reason: Some("rag-unavailable".into()),
            ..Default::default()
        };
    };
    if !is_current() {
        return superseded;
    }
    match index_open(&mut index, file, text, version, is_current, embed) {
        Ok(out) => out,
        // Node throws here; its queue records the run as failed.
        Err(e) => IndexOutcome {
            reason: Some("failed".into()),
            embed_error: Some(e),
            ..Default::default()
        },
    }
}

fn index_open(
    index: &mut Index,
    file: &str,
    text: &[u16],
    version: &str,
    is_current: &mut dyn FnMut() -> bool,
    embed: &mut dyn Embed,
) -> Res<IndexOutcome> {
    if text.len() <= DIRECT_INJECT_MAX {
        // Small file: stored for the record, injected directly by chat.
        let body = to_text(text);
        let tx = index.db.unchecked_transaction().map_err(err)?;
        drop_file_rows(index, file, true, None)?;
        tx.execute(
            "INSERT INTO chunks (file, chunk_no, body, content_hash, version, direct) VALUES (?, 0, ?, ?, ?, 1)",
            params![file, body, hash(&body), version],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        return Ok(IndexOutcome {
            ok: true,
            stored: 1,
            direct: true,
            ..Default::default()
        });
    }
    // Overlapping windows can yield identical chunks; the (file, content_hash) index keeps one.
    let mut seen = HashSet::new();
    let unique: Vec<(String, String)> = crate::chunk::chunk_text(text)
        .iter()
        .map(|c| (to_text(c), hash_units(c)))
        .filter(|(_, h)| seen.insert(h.clone()))
        .collect();
    let tx = index.db.unchecked_transaction().map_err(err)?;
    // The tail of this same text stays; a tail chunk that is now one of the head's goes.
    drop_file_rows(index, file, true, Some(version))?;
    let mut clashing = Vec::new();
    for (_, h) in &unique {
        let sql =
            format!("SELECT id FROM chunks WHERE file = ? AND content_hash = ? AND {TAIL_ROW}");
        if let Some(id) = tx
            .query_row(&sql, params![file, h], |r| r.get::<_, i64>(0))
            .optional()
            .map_err(err)?
        {
            clashing.push(id);
        }
    }
    if !clashing.is_empty() {
        slots::drop_chunks(&tx, &clashing)?;
        for id in &clashing {
            tx.execute("DELETE FROM chunks WHERE id = ?", [id])
                .map_err(err)?;
        }
    }
    let mut rows = Vec::with_capacity(unique.len());
    for (i, (body, h)) in unique.into_iter().enumerate() {
        tx.execute(
            "INSERT INTO chunks (file, chunk_no, body, content_hash, version, direct) VALUES (?, ?, ?, ?, ?, 0)",
            params![file, i as i64, body, h, version],
        )
        .map_err(err)?;
        rows.push(Row {
            id: tx.last_insert_rowid(),
            body,
            hash: h,
            fresh: true,
        });
    }
    tx.commit().map_err(err)?;
    let active = index.active.clone();
    let (embedded, embed_error) =
        match embed_rows_into(index, &active, &rows, is_current, EMBED_BATCH, embed) {
            Ok(n) => (n, None),
            Err(e) => (0, Some(e)),
        };
    Ok(IndexOutcome {
        ok: true,
        stored: rows.len(),
        embedded,
        embed_error,
        ..Default::default()
    })
}

/// One chunk of a long document's tail (`{ body, page }`).
#[derive(Clone, Debug)]
pub struct TailChunk {
    pub body: String,
    /// Used when a positive integer; otherwise the page comes from the body's marker.
    pub page: Option<i64>,
}

/// What `appendFileTail` is asked to do.
pub struct TailRequest<'a> {
    pub file: &'a str,
    pub head_text: &'a str,
    pub tail: &'a [TailChunk],
    pub limit: usize,
    pub from_page: Option<i64>,
    pub to_page: Option<i64>,
    pub final_window: bool,
}

/// The process state a tail append consults.
pub struct TailGates<'a> {
    pub is_current: &'a mut dyn FnMut() -> bool,
    /// `foregroundBusy()`: a chat or search is in flight.
    pub busy: &'a dyn Fn() -> bool,
    /// `indexExists`: a tail append never creates an index.
    pub index_exists: bool,
    /// `fileVersions` entry of the file (see [`crate::state::FileState::wanted`]).
    pub wanted: Option<Option<&'a str>>,
    /// `embeddingOff(index.active)`.
    pub embedding_off: bool,
}

/// What `appendFileTailNow` returns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailOutcome {
    pub ok: bool,
    pub reason: Option<String>,
    pub error: Option<String>,
    pub yielded: bool,
    pub embedded: usize,
    pub inserted: usize,
    pub attempted: usize,
    pub pending: Option<usize>,
    pub searchable_to: Option<i64>,
    pub window_done: Option<bool>,
    pub complete: bool,
}

impl TailOutcome {
    fn none(reason: &str) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Default::default()
        }
    }

    /// The outcome as Node returns it.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::Map::new();
        v.insert("ok".into(), json!(self.ok));
        if let Some(r) = &self.reason {
            v.insert("reason".into(), json!(r));
        }
        if let Some(e) = &self.error {
            v.insert("error".into(), json!(e));
        }
        if self.yielded {
            v.insert("yielded".into(), json!(true));
        }
        v.insert("embedded".into(), json!(self.embedded));
        v.insert("inserted".into(), json!(self.inserted));
        v.insert("attempted".into(), json!(self.attempted));
        v.insert("pending".into(), json!(self.pending));
        v.insert("searchableTo".into(), json!(self.searchable_to));
        if let Some(w) = self.window_done {
            v.insert("windowDone".into(), json!(w));
        }
        v.insert("complete".into(), json!(self.complete));
        Value::Object(v)
    }
}

/// `pageOf`: the page of a `[Page N]` chunk, else 0.
fn page_of(body: &str) -> i64 {
    let u = units(body);
    match crate::chunk::page_marker(&u) {
        Some(end) => body
            .get(6..end.saturating_sub(1))
            .and_then(|d| d.parse::<i64>().ok())
            .unwrap_or(0),
        None => 0,
    }
}

/// `metaPage`: `Number(JSON.parse(meta).page) || 0`.
fn meta_page(meta: Option<&str>) -> f64 {
    let Some(v) = meta.and_then(|m| serde_json::from_str::<Value>(m).ok()) else {
        return 0.0;
    };
    let n = match v.get("page") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        _ => f64::NAN,
    };
    if n.is_nan() {
        0.0
    } else {
        n
    }
}

fn tail_ids(db: &Connection, file: &str) -> Res<Vec<(i64, String)>> {
    let mut st = db
        .prepare(&format!(
            "SELECT id, content_hash FROM chunks WHERE file = ? AND {TAIL_ROW}"
        ))
        .map_err(err)?;
    let rows = st
        .query_map([file], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(err)?;
    rows.collect::<Result<_, _>>().map_err(err)
}

fn vectored(index: &Index, file: &str) -> Res<HashSet<i64>> {
    let Some(slot) = slots::find_slot(&index.db, &index.active)? else {
        return Ok(HashSet::new());
    };
    let sql = format!(
        "SELECT c.id FROM {} WHERE c.file = ? AND {TAIL_ROW}",
        slots::join_sql(&slot)
    );
    Ok(ids(&index.db, &sql, &[&file])?.into_iter().collect())
}

struct Wanted {
    body: String,
    hash: String,
    page: i64,
}

/// `appendFileTailNow`: appends the chunks of a long document past its head, embedding at most
/// `limit` of them per call (rows left without a vector first), and stops when chat is in flight.
pub fn append_tail_now(
    open: &mut dyn FnMut() -> Res<Option<Index>>,
    req: &TailRequest<'_>,
    gates: &mut TailGates<'_>,
    embed: &mut dyn Embed,
) -> Res<TailOutcome> {
    if !(gates.is_current)() {
        return Ok(TailOutcome::none("superseded"));
    }
    if (gates.busy)() {
        return Ok(TailOutcome::none("busy"));
    }
    if !gates.index_exists {
        return Ok(TailOutcome::none("no-index"));
    }
    let Ok(Some(mut index)) = open() else {
        return Ok(TailOutcome::none("rag-unavailable"));
    };
    let version = hash(req.head_text);
    if gates.wanted.is_some_and(|w| w != Some(version.as_str())) {
        return Ok(TailOutcome::none("reindex-pending"));
    }
    let file = req.file;
    let head: Vec<(Option<String>, i64)> = {
        let sql = format!("SELECT version, COUNT(*) AS n FROM chunks WHERE file = ? AND {TEXT_ROW} AND NOT {TAIL_ROW} GROUP BY version");
        let mut st = index.db.prepare(&sql).map_err(err)?;
        let rows = st
            .query_map([file], |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)?
    };
    let head_ok = matches!(head.as_slice(), [(Some(v), _)] if *v == version);
    if !head_ok || units(req.head_text).len() <= DIRECT_INJECT_MAX {
        return Ok(TailOutcome::none("head-not-indexed"));
    }
    if gates.embedding_off {
        return Ok(TailOutcome::none("embedding-off"));
    }
    let head_hashes: HashSet<String> = {
        let sql = format!(
            "SELECT content_hash FROM chunks WHERE file = ? AND {TEXT_ROW} AND NOT {TAIL_ROW}"
        );
        let mut st = index.db.prepare(&sql).map_err(err)?;
        let rows = st
            .query_map([file], |r| r.get::<_, String>(0))
            .map_err(err)?;
        rows.collect::<Result<_, _>>().map_err(err)?
    };
    let mut wanted = Vec::new();
    let mut seen = HashSet::new();
    for t in req.tail {
        if t.body.is_empty() {
            continue;
        }
        let h = hash(&t.body);
        if seen.contains(&h) || head_hashes.contains(&h) {
            continue;
        }
        seen.insert(h.clone());
        let page = t
            .page
            .filter(|p| *p > 0)
            .unwrap_or_else(|| page_of(&t.body));
        wanted.push(Wanted {
            body: t.body.clone(),
            hash: h,
            page,
        });
    }
    let lo = req.from_page.map_or(f64::NEG_INFINITY, |p| p as f64);
    let hi = req.to_page.map_or(f64::INFINITY, |p| p as f64);
    let rows = {
        let tx = index.db.unchecked_transaction().map_err(err)?;
        let stale: Vec<i64> = {
            let sql = format!(
                "SELECT id, content_hash, version, meta FROM chunks WHERE file = ? AND {TAIL_ROW}"
            );
            let mut st = tx.prepare(&sql).map_err(err)?;
            let all = st
                .query_map([file], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(err)?;
            let mut out = Vec::new();
            for row in all {
                let (id, h, v, meta) = row.map_err(err)?;
                let gone = if v.as_deref() != Some(version.as_str()) {
                    true
                } else {
                    let page = meta_page(meta.as_deref());
                    if page >= lo && page <= hi {
                        !seen.contains(&h)
                    } else {
                        req.final_window && page > hi
                    }
                };
                if gone {
                    out.push(id);
                }
            }
            out
        };
        if !stale.is_empty() {
            slots::drop_chunks(&tx, &stale)?;
            for id in &stale {
                tx.execute("DELETE FROM chunks WHERE id = ?", [id])
                    .map_err(err)?;
            }
        }
        let mut present: HashMap<String, i64> = tail_ids(&tx, file)?
            .into_iter()
            .map(|(id, h)| (h, id))
            .collect();
        let have = vectored(&index, file)?;
        let mut out = Vec::new();
        // First the rows already appended whose embedding failed or was cut short, then new rows.
        for w in &wanted {
            if out.len() >= req.limit {
                break;
            }
            if let Some(&id) = present.get(&w.hash) {
                if !have.contains(&id) {
                    out.push(Row {
                        id,
                        body: w.body.clone(),
                        hash: w.hash.clone(),
                        fresh: false,
                    });
                }
            }
        }
        let max: Option<i64> = tx
            .query_row(
                "SELECT MAX(chunk_no) AS n FROM chunks WHERE file = ?",
                [file],
                |r| r.get(0),
            )
            .map_err(err)?;
        let mut next = max.unwrap_or(-1) + 1;
        for w in &wanted {
            if out.len() >= req.limit {
                break;
            }
            if present.contains_key(&w.hash) {
                continue;
            }
            // JSON.stringify({ tail: 1, page }): key order matters, TAIL_ROW matches the text's start.
            let meta = format!("{{\"tail\":1,\"page\":{}}}", w.page);
            tx.execute(
                "INSERT INTO chunks (file, chunk_no, body, content_hash, version, direct, meta) VALUES (?, ?, ?, ?, ?, 0, ?)",
                params![file, next, w.body, w.hash, version, meta],
            )
            .map_err(err)?;
            next += 1;
            let id = tx.last_insert_rowid();
            present.insert(w.hash.clone(), id);
            out.push(Row {
                id,
                body: w.body.clone(),
                hash: w.hash.clone(),
                fresh: true,
            });
        }
        tx.commit().map_err(err)?;
        out
    };
    let inserted = rows.iter().filter(|r| r.fresh).count();
    let (mut embedded, mut error) = (0, None);
    if !rows.is_empty() {
        let active = index.active.clone();
        let busy = gates.busy;
        let current = &mut *gates.is_current;
        let mut check = || current() && !busy();
        match embed_rows_into(
            &mut index,
            &active,
            &rows,
            &mut check,
            TAIL_EMBED_REQUEST,
            embed,
        ) {
            Ok(n) => embedded = n,
            Err(e) => error = Some(to_text(units(&e).get(..200).unwrap_or(&units(&e)))),
        }
    }
    let yielded = error.is_none() && embedded < rows.len() && (gates.busy)();
    let have = vectored(&index, file)?;
    let ids: HashMap<String, i64> = tail_ids(&index.db, file)?
        .into_iter()
        .map(|(id, h)| (h, id))
        .collect();
    let missing = |w: &&Wanted| !ids.get(&w.hash).is_some_and(|id| have.contains(id));
    let first_missing = wanted.iter().find(missing);
    let pending = wanted.iter().filter(missing).count();
    let last_page = wanted.last().map(|w| w.page).or(req.to_page);
    let searchable_to = match first_missing {
        Some(w) => Some((w.page - 1).max(0)),
        None if req.final_window => None,
        None => last_page,
    };
    Ok(TailOutcome {
        ok: error.is_none(),
        reason: error.as_ref().map(|_| "embed-failed".to_string()),
        error,
        yielded,
        embedded,
        inserted,
        attempted: rows.len(),
        pending: Some(pending),
        searchable_to,
        window_done: Some(first_missing.is_none()),
        complete: req.final_window && first_missing.is_none(),
    })
}

/// `deleteProjectFile`'s write: drops the file's rows (its text only with `keep_media`). Returns
/// false when the index could not be opened (the caller's [`crate::state::FileState::after_delete`]).
pub fn delete_file(
    open: &mut dyn FnMut() -> Res<Option<Index>>,
    file: &str,
    keep_media: bool,
) -> Res<bool> {
    let Ok(Some(index)) = open() else {
        return Ok(false);
    };
    let tx = index.db.unchecked_transaction().map_err(err)?;
    drop_file_rows(&index, file, keep_media, None)?;
    tx.commit().map_err(err)?;
    Ok(true)
}
