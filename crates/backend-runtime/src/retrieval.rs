//! Native index with model/profile-separated vector slots and hash-bound vectors.
//! A new database is explicit; this module does not silently migrate sqlite-vec tables.
use crate::{diary::Hit, Control, Fault, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
    time::Duration,
};
const SCHEMA:&str="CREATE TABLE native_index_meta(version INTEGER NOT NULL);INSERT INTO native_index_meta VALUES(1);
CREATE TABLE chunks(id INTEGER PRIMARY KEY,day TEXT NOT NULL,file TEXT NOT NULL,header TEXT NOT NULL,body TEXT NOT NULL,content_hash TEXT NOT NULL,UNIQUE(file,content_hash));
CREATE TABLE dirty_documents(document TEXT PRIMARY KEY);
CREATE TABLE native_embedding_slots(id TEXT PRIMARY KEY,model TEXT NOT NULL,profile TEXT NOT NULL,dim INTEGER NOT NULL);
CREATE TABLE native_embeddings(slot TEXT NOT NULL,chunk INTEGER NOT NULL,content_hash TEXT NOT NULL,embedding BLOB NOT NULL,PRIMARY KEY(slot,chunk));";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub model: String,
    pub profile: String,
}
impl Target {
    pub fn id(&self) -> Result<String> {
        if self.model.is_empty()
            || self.model.len() > 1024
            || self.profile.is_empty()
            || self.profile.len() > 128
            || self.model.contains('\0')
            || self.profile.contains('\0')
        {
            return Err(Fault::Invalid);
        }
        Ok(format!(
            "{:x}",
            Sha256::digest(format!("{}\0{}", self.profile, self.model).as_bytes())
        ))
    }
}
pub struct Index {
    db: Mutex<Connection>,
}
pub struct Plan {
    pub file: String,
    pub chunks: Vec<diary_corpus_parse::Chunk>,
    pub target: Target,
    pub vectors: BTreeMap<String, Vec<f32>>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Applied {
    pub embedded: usize,
    pub pending: bool,
}
fn encode(vector: &[f32]) -> Result<Vec<u8>> {
    if vector.is_empty() || vector.len() > 65536 || vector.iter().any(|v| !v.is_finite()) {
        return Err(Fault::Invalid);
    }
    Ok(vector.iter().flat_map(|v| v.to_le_bytes()).collect())
}
fn decode(bytes: &[u8], dim: usize) -> Result<Vec<f32>> {
    if dim == 0 || dim > 65536 || bytes.len() != dim * 4 {
        return Err(Fault::Database);
    }
    let mut values = Vec::with_capacity(dim);
    for chunk in bytes.as_chunks::<4>().0 {
        let value = f32::from_le_bytes(*chunk);
        if !value.is_finite() {
            return Err(Fault::Database);
        }
        values.push(value);
    }
    Ok(values)
}
impl Index {
    pub fn create(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| Fault::Conflict)?;
        drop(file);
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        let version: i64 =
            db.query_row("SELECT version FROM native_index_meta", [], |r| r.get(0))?;
        if version != 1 {
            return Err(Fault::Unavailable);
        }
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn plan(file: &str, text: &str, target: Target) -> Result<Plan> {
        target.id()?;
        if file.is_empty() || file.len() > 4096 || text.len() > 8 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let chunks = diary_corpus_parse::parse_chunks(Some(text));
        if chunks.len() > 10000 {
            return Err(Fault::Capacity);
        }
        Ok(Plan {
            file: file.to_owned(),
            chunks,
            target,
            vectors: BTreeMap::new(),
        })
    }
    pub fn mark_dirty(&self, file: &str) -> Result<()> {
        self.db.lock().map_err(|_| Fault::Database)?.execute(
            "INSERT OR IGNORE INTO dirty_documents(document) VALUES(?)",
            [file],
        )?;
        Ok(())
    }
    pub fn apply(&self, plan: &Plan, control: &Control) -> Result<Applied> {
        control.check()?;
        let slot = plan.target.id()?;
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        let mut dim: Option<u32> = tx
            .query_row(
                "SELECT dim FROM native_embedding_slots WHERE id=?",
                [&slot],
                |r| r.get(0),
            )
            .optional()?;
        let mut indexed = BTreeMap::<String, i64>::new();
        {
            let mut q = tx.prepare("SELECT content_hash,id FROM chunks WHERE file=?")?;
            for row in q.query_map([&plan.file], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (hash, id) = row?;
                indexed.insert(hash, id);
            }
        }
        let hashes: BTreeSet<&str> = plan
            .chunks
            .iter()
            .map(|c| c.content_hash.as_str())
            .collect();
        for (hash, id) in &indexed {
            if !hashes.contains(hash.as_str()) {
                tx.execute("DELETE FROM native_embeddings WHERE chunk=?", [id])?;
                tx.execute("DELETE FROM chunks WHERE id=?", [id])?;
            }
        }
        let mut result = Applied {
            embedded: 0,
            pending: false,
        };
        let mut done = BTreeSet::new();
        for chunk in &plan.chunks {
            control.check()?;
            if !done.insert(chunk.content_hash.as_str()) {
                continue;
            }
            if let Some(id) = indexed.get(&chunk.content_hash) {
                let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM native_embeddings WHERE slot=? AND chunk=? AND content_hash=?)",params![slot,id,chunk.content_hash],|r|r.get(0))?;
                if exists {
                    continue;
                }
            }
            let Some(vector) = plan.vectors.get(&chunk.content_hash) else {
                result.pending = true;
                continue;
            };
            let bytes = encode(vector)?;
            if dim.is_some_and(|d| d as usize != vector.len()) {
                result.pending = true;
                continue;
            }
            if dim.is_none() {
                tx.execute(
                    "INSERT INTO native_embedding_slots(id,model,profile,dim) VALUES(?,?,?,?)",
                    params![
                        slot,
                        plan.target.model,
                        plan.target.profile,
                        vector.len() as u32
                    ],
                )?;
                dim = Some(vector.len() as u32);
            }
            let id = if let Some(id) = indexed.get(&chunk.content_hash) {
                *id
            } else {
                tx.execute(
                    "INSERT INTO chunks(day,file,header,body,content_hash) VALUES(?,?,?,?,?)",
                    params![
                        chunk.day,
                        plan.file,
                        chunk.header,
                        chunk.text,
                        chunk.content_hash
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.execute("DELETE FROM native_embeddings WHERE chunk=?", [id])?;
                id
            };
            tx.execute("INSERT INTO native_embeddings(slot,chunk,content_hash,embedding) VALUES(?,?,?,?) ON CONFLICT(slot,chunk) DO UPDATE SET content_hash=excluded.content_hash,embedding=excluded.embedding",params![slot,id,chunk.content_hash,bytes])?;
            result.embedded += 1;
        }
        if !result.pending {
            tx.execute("DELETE FROM dirty_documents WHERE document=?", [&plan.file])?;
        }
        control.check()?;
        tx.commit()?;
        Ok(result)
    }
    pub fn search(
        &self,
        target: &Target,
        query: &[f32],
        top_k: usize,
        min_score: f64,
        control: &Control,
    ) -> Result<Vec<Hit>> {
        control.check()?;
        encode(query)?;
        if top_k > 1000 || !min_score.is_finite() {
            return Err(Fault::Invalid);
        }
        let slot = target.id()?;
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction()?;
        let dim: Option<u32> = tx
            .query_row(
                "SELECT dim FROM native_embedding_slots WHERE id=?",
                [&slot],
                |r| r.get(0),
            )
            .optional()?;
        let Some(dim) = dim else {
            return Ok(Vec::new());
        };
        let dim = dim as usize;
        if dim != query.len() {
            return Err(Fault::Invalid);
        }
        let mut hits = Vec::new();
        let mut total = 0usize;
        {
            let mut q=tx.prepare("SELECT c.id,c.day,c.header,c.body,v.embedding,length(c.body),length(v.embedding) FROM chunks c JOIN native_embeddings v ON v.chunk=c.id AND v.content_hash=c.content_hash WHERE v.slot=? AND c.file NOT IN(SELECT document FROM dirty_documents) ORDER BY c.id LIMIT 100001")?;
            let mut rows = q.query([&slot])?;
            let mut count = 0usize;
            while let Some(row) = rows.next()? {
                control.check()?;
                count += 1;
                if count > 100000 {
                    return Err(Fault::Capacity);
                }
                let body_bytes = row.get::<_, u32>(5)? as usize;
                let vector_bytes = row.get::<_, u32>(6)? as usize;
                total = total
                    .saturating_add(body_bytes)
                    .saturating_add(vector_bytes);
                if body_bytes > 8 * 1024 * 1024
                    || vector_bytes != dim * 4
                    || total > 128 * 1024 * 1024
                {
                    return Err(Fault::Capacity);
                }
                let data: Vec<u8> = row.get(4)?;
                let vector = decode(&data, dim)?;
                let candidates = crate::diary::search_vectors(
                    query,
                    [(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, vector)],
                    1,
                    min_score,
                )?;
                hits.extend(candidates);
                if hits.len() > top_k.saturating_mul(2).max(1) {
                    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
                    hits.truncate(top_k);
                }
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        hits.truncate(top_k);
        control.check()?;
        tx.finish()?;
        for hit in &mut hits {
            hit.score = (hit.score * 10000.0).round_ties_even() / 10000.0;
        }
        Ok(hits)
    }
}
