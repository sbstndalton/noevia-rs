//! Media and document-tail mutations use the embedding owner's existing lifecycle
//! fence. All snapshots are rechecked inside publication; no database handle crosses HTTP.
// serde_json's read-only string-key indexing returns Null. All mutable document
// parents are checked as objects by the candidate/version guards before mutation.
#![allow(clippy::indexing_slicing)]

use crate::{
    fill::Row,
    sqlite::{ensure, find, SqlStore},
    Control, Index, Target,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub fn digest(text: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            text.chars()
                .map(|c| if js_json::wtf16::lone_unit(c).is_some() {
                    '\u{fffd}'
                } else {
                    c
                })
                .collect::<String>()
                .as_bytes()
        )
    )
}
fn fault() -> String {
    "media index unavailable".into()
}
#[derive(Clone)]
pub struct Snapshot {
    pub index: Index,
    pub project: Value,
}
impl SqlStore {
    pub fn media_snapshot(&self, user: &str, project: &str) -> Result<Snapshot, String> {
        let index = Index {
            user: Some(user.into()),
            project: project.into(),
        };
        let dir = self.dir(&index)?;
        let projects = server_projects::store::read(&dir).map_err(|_| fault())?;
        let p = server_projects::store::find(&projects, project).ok_or_else(fault)?;
        let text = js_json::wtf16::stringify(p).ok_or_else(fault)?;
        Ok(Snapshot {
            index,
            project: server_projects::source_jobs::parse_js(text.as_bytes())
                .map_err(|_| fault())?,
        })
    }
    pub(crate) fn snapshot_current(&self, snapshot: &Snapshot) -> Result<bool, String> {
        self.media_snapshot(
            snapshot.index.user.as_deref().ok_or_else(fault)?,
            &snapshot.index.project,
        )
        .map(|s| s.project == snapshot.project)
    }
    /// The same schema as the native head-index owner, created only for an actual image.
    pub fn create_media_index(&self, snapshot: &Snapshot, control: &Control) -> Result<(), String> {
        if self.index_path(&snapshot.index)?.exists() {
            return Ok(());
        }
        let _config = server_projects::lock::acquire(&self.root.join("embedding-config.json"))
            .map_err(|_| fault())?;
        let writer = server_store::Writer::open(&self.root, server_store::RustAuth::owned())
            .map_err(|_| fault())?;
        writer.write(|auth| {
            let user = snapshot.index.user.as_deref().unwrap_or_default();
            if auth.reader().row("SELECT id FROM users WHERE id=?", [user], |r| r.get::<_, String>(0))?.is_none() { return Ok(Err(fault())); }
            Ok((|| {
                let dir = self.dir(&snapshot.index)?;
                let _projects = server_projects::lock::acquire(&dir.join("projects.json")).map_err(|_| fault())?;
                if !self.snapshot_current(snapshot)? { return Err(fault()); }
                let outbox_path=dir.join("index-outbox.db");
                let mut outbox=if outbox_path.exists(){Some(crate::sqlite::open_existing(&outbox_path)?)}else{None};
                let _fence=match outbox.as_mut(){
                    Some(db)=>{
                        let fence=db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_|fault())?;
                        let dead:bool=fence.query_row("SELECT EXISTS(SELECT 1 FROM tombstones WHERE project=?)",[&snapshot.index.project],|r|r.get(0)).map_err(|_|fault())?;
                        if dead{return Err(fault());}Some(fence)
                    },None=>None,
                };
                control.renew_publication_proof()?;
                if control.stopped() || control.suspended() { return Err(fault()); }
                let path = self.index_path(&snapshot.index)?;
                server_projects::store::create_dirs_below(&dir, path.parent().ok_or_else(fault)?).map_err(|_| fault())?;
                let mut db = Connection::open(path).map_err(|_| fault())?;
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|_| fault())?;
                tx.execute_batch("CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY,file TEXT NOT NULL,chunk_no INTEGER NOT NULL,body TEXT NOT NULL,content_hash TEXT NOT NULL,updated_at TEXT NOT NULL DEFAULT(datetime('now')),version TEXT,direct INTEGER,kind TEXT,meta TEXT); CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_file_hash ON chunks(file,content_hash); CREATE TABLE IF NOT EXISTS vec_meta(key TEXT PRIMARY KEY,value TEXT);").map_err(|_| fault())?;
                tx.commit().map_err(|_| fault())?;
                Ok(())
            })())
        }).map_err(|_| fault())?
    }
    pub fn reconcile_media(
        &self,
        snapshot: &Snapshot,
        items: &[MediaItem],
        target: &Target,
        control: &Control,
    ) -> Result<Vec<Row>, String> {
        self.fenced(&snapshot.index, true, |db| {
            control.renew_publication_proof()?;
            if control.stopped() || control.suspended() || !self.snapshot_current(snapshot)? { return Err(fault()); }
            let wanted: BTreeSet<_> = items.iter().map(|i| (i.file.as_str(), i.hash.as_str())).collect();
            let existing = {
                let mut q = db.prepare("SELECT id,file,content_hash FROM chunks WHERE kind='image'").map_err(|_| fault())?;
                let selected = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).map_err(|_| fault())?.collect::<Result<Vec<_>, _>>().map_err(|_| fault())?;
                selected
            };
            let stale: Vec<_> = existing.iter().filter(|(_, f, h)| !wanted.contains(&(f.as_str(), h.as_str()))).map(|(id, _, _)| *id).collect();
            self.drop_chunks(db, &stale)?;
            for (n, item) in items.iter().enumerate() {
                db.execute("INSERT OR IGNORE INTO chunks(file,chunk_no,body,content_hash,version,direct,kind,meta) VALUES(?,?,?,?,NULL,0,'image',?)", params![item.file, n as i64, js_json::wtf16::slice(&item.label,500), item.hash, item.meta.to_string()]).map_err(|_| fault())?;
            }
            let captions: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='media_captions')", [], |r| r.get(0)).map_err(|_| fault())?;
            if captions { db.execute("DELETE FROM media_captions WHERE content_hash NOT IN (SELECT content_hash FROM chunks WHERE kind='image')", []).map_err(|_| fault())?; }
            let slot = find(db, target)?;
            let sql = match slot.as_ref().and_then(|s| s.map.as_ref()) {
                Some(map) => format!("SELECT c.id,c.content_hash,c.body FROM chunks c LEFT JOIN {map} m ON m.chunk_id=c.id AND m.content_hash=c.content_hash WHERE c.kind='image' AND m.chunk_id IS NULL ORDER BY c.id"),
                None => "SELECT id,content_hash,body FROM chunks WHERE kind='image' ORDER BY id".into(),
            };
            let mut q = db.prepare(&sql).map_err(|_| fault())?;
            let selected = q.query_map([], |r| Ok(Row { id: r.get(0)?, hash: r.get(1)?, text: r.get::<_, String>(2)?.encode_utf16().collect() })).map_err(|_| fault())?.collect::<Result<Vec<_>, _>>().map_err(|_| fault());
            selected
        })
    }
    pub fn publish_media_rows(
        &self,
        snapshot: &Snapshot,
        target: &Target,
        rows: &[Row],
        vectors: &[Vec<f32>],
        version: u64,
        control: &Control,
    ) -> Result<usize, String> {
        self.fenced(&snapshot.index, true, |db| {
            control.renew_publication_proof()?;
            if control.stopped()
                || control.suspended()
                || self.config()?.version != version
                || !self.snapshot_current(snapshot)?
            {
                return Err(fault());
            }
            let Some(first) = vectors.first() else {
                return Ok(0);
            };
            if first.is_empty() || first.iter().any(|v| !v.is_finite()) {
                return Ok(0);
            }
            let slot = ensure(db, target, first.len())?;
            let mut count = 0;
            for (row, vector) in rows.iter().zip(vectors) {
                if vector.len() != slot.dim || vector.iter().any(|v| !v.is_finite()) {
                    continue;
                }
                if db
                    .query_row(
                        "SELECT id FROM chunks WHERE id=? AND content_hash=?",
                        params![row.id, row.hash],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(|_| fault())?
                    .is_none()
                {
                    continue;
                }
                db.execute(
                    &format!("DELETE FROM {} WHERE chunk_id=?", slot.table),
                    [row.id],
                )
                .map_err(|_| fault())?;
                let bytes: Vec<_> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
                db.execute(
                    &format!("INSERT INTO {}(chunk_id,embedding) VALUES(?,?)", slot.table),
                    params![row.id, bytes],
                )
                .map_err(|_| fault())?;
                if let Some(map) = &slot.map {
                    db.execute(
                        &format!("INSERT OR REPLACE INTO {map}(chunk_id,content_hash) VALUES(?,?)"),
                        params![row.id, row.hash],
                    )
                    .map_err(|_| fault())?;
                }
                count += 1;
            }
            control.renew_publication_proof()?;
            Ok(count)
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_tail(
        &self,
        snapshot: &Snapshot,
        file: &str,
        head: &str,
        window: &Value,
        target: &Target,
        limit: usize,
        control: &Control,
    ) -> Result<(Vec<Row>, TailCoverage), String> {
        self.fenced(&snapshot.index, true, |db| {
            control.renew_publication_proof()?;
            if control.stopped() || control.suspended() || !self.snapshot_current(snapshot)? { return Err(fault()); }
            let version = digest(head);
            let heads = {
                let mut q = db.prepare("SELECT DISTINCT version FROM chunks WHERE file=? AND COALESCE(kind,'text')='text' AND NOT (COALESCE(meta,'') LIKE '{\"tail\":1,%')").map_err(|_| fault())?;
                let selected = q.query_map([file], |r| r.get::<_, Option<String>>(0)).map_err(|_| fault())?.collect::<Result<Vec<_>, _>>().map_err(|_| fault())?;
                selected
            };
            if heads.len() != 1 || heads.first().and_then(Option::as_deref) != Some(version.as_str()) { return Err("head-not-indexed".into()); }
            let head_hashes = {
                let mut q = db.prepare("SELECT content_hash FROM chunks WHERE file=? AND NOT (COALESCE(meta,'') LIKE '{\"tail\":1,%')").map_err(|_| fault())?;
                let selected = q.query_map([file], |r| r.get::<_, String>(0)).map_err(|_| fault())?.collect::<Result<BTreeSet<_>, _>>().map_err(|_| fault())?;
                selected
            };
            let mut seen = BTreeSet::new();
            let wanted: Vec<_> = window["chunks"].as_array().into_iter().flatten().filter_map(|c| {
                let body = c["body"].as_str()?;
                let hash = digest(body);
                if body.is_empty() || head_hashes.contains(&hash) || !seen.insert(hash.clone()) { None } else { Some((hash, body.to_owned(), c["page"].as_i64().unwrap_or(0))) }
            }).collect();
            let old = {
                let mut q = db.prepare("SELECT id,content_hash,version,meta FROM chunks WHERE file=? AND COALESCE(meta,'') LIKE '{\"tail\":1,%'").map_err(|_| fault())?;
                let selected = q.query_map([file], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?))).map_err(|_| fault())?.collect::<Result<Vec<_>, _>>().map_err(|_| fault())?;
                selected
            };
            let lo = window["fromPage"].as_i64().unwrap_or(i64::MIN);
            let hi = window["toPage"].as_i64().unwrap_or(i64::MAX);
            let stale: Vec<_> = old.iter().filter(|(_, hash, v, meta)| {
                let page = serde_json::from_str::<Value>(meta).ok().and_then(|m| m["page"].as_i64()).unwrap_or(0);
                v.as_deref() != Some(version.as_str()) || (page >= lo && page <= hi && !seen.contains(hash)) || (window["final"] == true && page > hi)
            }).map(|(id, _, _, _)| *id).collect();
            self.drop_chunks(db, &stale)?;
            let mut next = db.query_row("SELECT COALESCE(MAX(chunk_no),-1)+1 FROM chunks WHERE file=?", [file], |r| r.get::<_, i64>(0)).map_err(|_| fault())?;
            let mut rows = Vec::new();
            let mut fresh = Vec::new();
            for (hash, body, page) in &wanted {
                let id = db.query_row("SELECT id FROM chunks WHERE file=? AND content_hash=?", params![file, hash], |r| r.get::<_, i64>(0)).optional().map_err(|_| fault())?;
                if let Some(id) = id {
                    if !vectored(db, target, id, hash)? && rows.len() < limit { rows.push(Row { id, hash: hash.clone(), text: body.encode_utf16().collect() }); }
                } else { fresh.push((hash, body, page)); }
            }
            for (hash, body, page) in fresh {
                if rows.len() >= limit { break; }
                // Field order is part of Node's tail-row discriminator.
                let meta = format!("{{\"tail\":1,\"page\":{page}}}");
                db.execute("INSERT INTO chunks(file,chunk_no,body,content_hash,version,direct,meta) VALUES(?,?,?,?,?,0,?)", params![file, next, body, hash, version, meta]).map_err(|_| fault())?;
                next += 1;
                rows.push(Row { id: db.last_insert_rowid(), hash: hash.clone(), text: body.encode_utf16().collect() });
            }
            Ok((rows, TailCoverage { wanted: wanted.into_iter().map(|(hash, _, page)| (hash, page)).collect(), final_window: window["final"] == true, to_page: hi }))
        })
    }
    pub fn tail_coverage(
        &self,
        snapshot: &Snapshot,
        file: &str,
        target: &Target,
        coverage: &TailCoverage,
    ) -> Result<(usize, Option<i64>), String> {
        self.fenced(&snapshot.index, false, |db| {
            if !self.snapshot_current(snapshot)? {
                return Err(fault());
            }
            let mut pending = 0;
            let mut first = None;
            for (hash, page) in &coverage.wanted {
                let id = db
                    .query_row(
                        "SELECT id FROM chunks WHERE file=? AND content_hash=?",
                        params![file, hash],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(|_| fault())?;
                if !id
                    .map(|id| vectored(db, target, id, hash))
                    .transpose()?
                    .unwrap_or(false)
                {
                    pending += 1;
                    if first.is_none() {
                        first = Some((*page - 1).max(0));
                    }
                }
            }
            Ok((
                pending,
                first.or(if coverage.final_window {
                    None
                } else {
                    Some(coverage.to_page)
                }),
            ))
        })
    }
}
fn vectored(db: &Connection, target: &Target, id: i64, hash: &str) -> Result<bool, String> {
    let Some(slot) = find(db, target)? else {
        return Ok(false);
    };
    let (sql, values) = match slot.map {
        Some(map) => (
            format!("SELECT EXISTS(SELECT 1 FROM {map} WHERE chunk_id=? AND content_hash=?)"),
            vec![
                rusqlite::types::Value::Integer(id),
                rusqlite::types::Value::Text(hash.into()),
            ],
        ),
        None => (
            format!(
                "SELECT EXISTS(SELECT 1 FROM {} WHERE chunk_id=?)",
                slot.table
            ),
            vec![rusqlite::types::Value::Integer(id)],
        ),
    };
    db.query_row(&sql, rusqlite::params_from_iter(values), |r| r.get(0))
        .map_err(|_| fault())
}
pub struct TailCoverage {
    wanted: Vec<(String, i64)>,
    final_window: bool,
    to_page: i64,
}
#[derive(Clone)]
pub struct MediaItem {
    pub file: String,
    pub hash: String,
    pub label: String,
    pub meta: Value,
    pub path: std::path::PathBuf,
    pub mime: String,
}
