//! Native managed-corpus access and vector retrieval. File mutations keep the legacy
//! managed-store generation/debounce/outbox semantics; workspace operations invoke the
//! existing receipt-preserving transaction engine directly, never a Python subprocess.
use crate::{Control, Fault, Result};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
pub struct WriteCondition<'a> {
    pub if_match: Option<&'a str>,
    pub if_none_match: bool,
}

#[derive(Clone)]
pub struct Corpus {
    path: PathBuf,
    tenant: String,
    debounce: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct File {
    pub content: String,
    pub version: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub id: i64,
    pub day: String,
    pub header: String,
    pub text: String,
    pub score: f64,
}
impl Corpus {
    pub fn open(path: &Path, tenant: &str, debounce: f64) -> Result<Self> {
        if !path.is_absolute()
            || uuid::Uuid::parse_str(tenant).is_err()
            || !debounce.is_finite()
            || debounce < 0.0
        {
            return Err(Fault::Invalid);
        }
        let result = Self {
            path: path.to_owned(),
            tenant: tenant.to_owned(),
            debounce,
        };
        let mut db = result.db()?;
        let tx = db.transaction()?;
        result.identity(&tx)?;
        tx.finish()?;
        Ok(result)
    }
    pub fn tenant_id(&self) -> &str {
        &self.tenant
    }
    fn db(&self) -> Result<Connection> {
        let db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.pragma_update(None, "synchronous", "FULL")?;
        Ok(db)
    }
    fn identity(&self, tx: &Transaction<'_>) -> Result<()> {
        let identity: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='tenant'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let active: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='active'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if identity.as_deref() != Some(&self.tenant) || active.as_deref() != Some("1") {
            return Err(Fault::Forbidden);
        }
        Ok(())
    }
    fn key(raw: &str) -> Result<String> {
        diary_workspace_rules::rel_path(raw, false).map_err(|_| Fault::Invalid)
    }
    pub fn read(&self, path: &str, control: &Control) -> Result<Option<File>> {
        let path = Self::key(path)?;
        let mut db = self.db()?;
        let tx = db.transaction()?;
        control.check()?;
        self.identity(&tx)?;
        let size: Option<i64> = tx
            .query_row(
                "SELECT length(data) FROM files WHERE path=?",
                [&path],
                |r| r.get(0),
            )
            .optional()?;
        if size.is_some_and(|n| !(0..=8 * 1024 * 1024).contains(&n)) {
            return Err(Fault::Capacity);
        }
        let row: Option<(Vec<u8>, String)> = tx
            .query_row(
                "SELECT data,version FROM files WHERE path=?",
                [&path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let result = row
            .map(|(bytes, version)| {
                if format!("{:x}", Sha256::digest(&bytes)) != version {
                    return Err(Fault::Database);
                }
                Ok(File {
                    content: String::from_utf8(bytes).map_err(|_| Fault::Invalid)?,
                    version,
                })
            })
            .transpose()?;
        control.check()?;
        tx.finish()?;
        Ok(result)
    }
    /// Native index publication consumes the managed corpus outbox only after the
    /// index commit. A crash before this commit leaves the item pending for replay.
    pub fn publish_index(
        &self,
        path: &str,
        version: Option<&str>,
        control: &Control,
        publish: impl FnOnce() -> Result<crate::retrieval::Applied>,
    ) -> Result<crate::retrieval::Applied> {
        let path = Self::key(path)?;
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        self.identity(&tx)?;
        let current: Option<String> = tx
            .query_row("SELECT version FROM files WHERE path=?", [&path], |r| {
                r.get(0)
            })
            .optional()?;
        if current.as_deref() != version {
            return Err(Fault::Conflict);
        }
        let result = publish()?;
        if !result.pending {
            tx.execute("DELETE FROM workspace_index_outbox WHERE path=?", [&path])?;
        }
        tx.commit()?;
        Ok(result)
    }
    /// Search holds the corpus writer while excluding every pending source change.
    /// Thus a changed/deleted file cannot leak stale indexed text before reconciliation.
    pub fn with_index_fence<T>(
        &self,
        control: &Control,
        search: impl FnOnce(&[String]) -> Result<T>,
    ) -> Result<T> {
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        self.identity(&tx)?;
        let pending = {
            let mut q =
                tx.prepare("SELECT path FROM workspace_index_outbox ORDER BY path LIMIT 100001")?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if pending.len() > 100000 {
            return Err(Fault::Capacity);
        }
        let result = search(&pending)?;
        tx.commit()?;
        Ok(result)
    }

    /// The caller's live authorization fence runs AFTER acquisition of the SQLite writer.
    /// HTTP/device/DAV callers must supply their actual authority, not a cached bool.
    pub fn put(
        &self,
        path: &str,
        content: &str,
        condition: WriteCondition<'_>,
        now: f64,
        control: &Control,
        authorize: impl FnOnce() -> Result<()>,
    ) -> Result<String> {
        let path = Self::key(path)?;
        if content.len() > 8 * 1024 * 1024 || !now.is_finite() {
            return Err(Fault::Capacity);
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        self.identity(&tx)?;
        authorize()?;
        let existing: Option<String> = tx
            .query_row("SELECT version FROM files WHERE path=?", [&path], |r| {
                r.get(0)
            })
            .optional()?;
        if condition.if_none_match && existing.is_some()
            || condition
                .if_match
                .is_some_and(|v| existing.as_deref() != Some(v))
        {
            return Err(Fault::Conflict);
        }
        let directory: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM directories WHERE path=?)",
            [&path],
            |r| r.get(0),
        )?;
        let children:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE substr(path,1,?)=?) OR EXISTS(SELECT 1 FROM directories WHERE substr(path,1,?)=?)",params![path.chars().count() as i64+1,format!("{path}/"),path.chars().count() as i64+1,format!("{path}/")],|r|r.get(0))?;
        if directory || children {
            return Err(Fault::Conflict);
        }
        let mut prefix = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let file: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE path=?)",
                [&prefix],
                |r| r.get(0),
            )?;
            if file {
                return Err(Fault::Conflict);
            }
            tx.execute(
                "INSERT OR IGNORE INTO directories(path) VALUES(?)",
                [&prefix],
            )?;
        }
        let version = format!("{:x}", Sha256::digest(content.as_bytes()));
        tx.execute("INSERT INTO files(path,data,version,updated) VALUES(?,?,?,?) ON CONFLICT(path) DO UPDATE SET data=excluded.data,version=excluded.version,updated=excluded.updated",params![path,content.as_bytes(),version,now])?;
        self.changed(&tx, &path, now)?;
        control.check()?;
        tx.commit()?;
        Ok(version)
    }
    fn changed(&self, tx: &Transaction<'_>, path: &str, now: f64) -> Result<()> {
        let generation: String =
            tx.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                r.get(0)
            })?;
        let generation = generation
            .parse::<u64>()
            .map_err(|_| Fault::Database)?
            .checked_add(1)
            .ok_or(Fault::Capacity)?;
        for (key, value) in [
            ("generation", generation.to_string()),
            ("due", (now + self.debounce).to_string()),
            ("snapshot_time", now.to_string()),
        ] {
            tx.execute("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO workspace_index_outbox(path) VALUES(?)",
            [path],
        )?;
        Ok(())
    }
    pub fn list(&self, parent: &str, control: &Control) -> Result<Vec<server_dav::Entry>> {
        let parent = diary_workspace_rules::rel_path(parent, true).map_err(|_| Fault::Invalid)?;
        let prefix = if parent.is_empty() {
            String::new()
        } else {
            format!("{parent}/")
        };
        let mut db = self.db()?;
        let tx = db.transaction()?;
        self.identity(&tx)?;
        control.check()?;
        let mut entries = std::collections::BTreeMap::new();
        {
            let mut query=tx.prepare("SELECT path,0 AS directory,updated FROM files WHERE substr(path,1,?1)=?2 UNION ALL SELECT path,1 AS directory,NULL FROM directories WHERE substr(path,1,?1)=?2 LIMIT 100001")?;
            let mut rows = query.query(params![prefix.chars().count() as i64, prefix])?;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                control.check()?;
                count += 1;
                if count > 100000 {
                    return Err(Fault::Capacity);
                }
                let path: String = row.get(0)?;
                if path.len() > 4096 {
                    return Err(Fault::Capacity);
                }
                let rest = path.strip_prefix(&prefix).ok_or(Fault::Database)?;
                let Some(name) = rest
                    .split('/')
                    .next()
                    .filter(|v| !v.is_empty() && !v.starts_with('.'))
                else {
                    continue;
                };
                let direct = !rest.contains('/');
                let directory = !direct || row.get::<_, bool>(1)?;
                if !directory && !name.to_ascii_lowercase().ends_with(".md") {
                    continue;
                }
                let modified: Option<f64> = row.get(2)?;
                let entry = server_dav::Entry {
                    path: format!("{prefix}{name}"),
                    name: name.to_owned(),
                    is_dir: directory,
                    modified_unix: modified
                        .filter(|n| n.is_finite() && *n > 0.0)
                        .map(|n| n as u64),
                };
                if let Some(old) = entries.get(&entry.name) {
                    let old: &server_dav::Entry = old;
                    if old.is_dir != entry.is_dir {
                        return Err(Fault::Database);
                    }
                } else {
                    entries.insert(entry.name.clone(), entry);
                }
                if entries.len() > 1024 {
                    return Err(Fault::Capacity);
                }
            }
        }
        control.check()?;
        tx.finish()?;
        Ok(entries.into_values().collect())
    }
    pub fn mkdir(&self, path: &str, now: f64, control: &Control) -> Result<()> {
        let path = Self::key(path)?;
        if !now.is_finite() {
            return Err(Fault::Invalid);
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        self.identity(&tx)?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path=?) OR EXISTS(SELECT 1 FROM directories WHERE path=?)",params![path,path],|r|r.get(0))?;
        if exists {
            return Err(Fault::Conflict);
        }
        if let Some((parent, _)) = path.rsplit_once('/') {
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM directories WHERE path=?) OR EXISTS(SELECT 1 FROM files WHERE substr(path,1,?)=?)",params![parent,parent.chars().count() as i64+1,format!("{parent}/")],|r|r.get(0))?;
            if !exists {
                return Err(Fault::Missing);
            }
        }
        tx.execute("INSERT INTO directories(path) VALUES(?)", [&path])?;
        self.changed(&tx, &path, now)?;
        control.check()?;
        tx.commit()?;
        Ok(())
    }
    pub fn workspace(
        &self,
        mut request: diary_workspace_transaction::Request,
        control: &Control,
    ) -> Result<diary_workspace_transaction::Reply> {
        // An authenticated tenant capability supplies these fields. A public request cannot
        // select another database or tenant by putting a path in its JSON body.
        request.db_path = self.path.to_str().ok_or(Fault::Invalid)?.to_owned();
        request.expected_tenant = self.tenant.clone();
        control.check()?;
        diary_workspace_transaction::execute(&request).map_err(|e| match e {
            diary_workspace_transaction::Fault::Conflict => Fault::Conflict,
            diary_workspace_transaction::Fault::Missing => Fault::Missing,
            diary_workspace_transaction::Fault::Protected => Fault::Forbidden,
            _ => Fault::Database,
        })
    }
}
/// Cosine search over already prepared vectors. A vector is usable only when its model
/// slot AND content hash match; dirty documents never enter the candidate set.
/// This function does not substitute lexical matches when embeddings are unavailable.
pub fn search_vectors(
    query: &[f32],
    rows: impl IntoIterator<Item = (i64, String, String, String, Vec<f32>)>,
    top_k: usize,
    min_score: f64,
) -> Result<Vec<Hit>> {
    if query.is_empty()
        || query.len() > 65536
        || query.iter().any(|v| !v.is_finite())
        || top_k > 1000
        || !min_score.is_finite()
    {
        return Err(Fault::Invalid);
    }
    let qnorm = query
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    if qnorm == 0.0 {
        return Err(Fault::Invalid);
    }
    let mut hits = Vec::new();
    for (count, (id, day, header, text, vector)) in rows.into_iter().enumerate() {
        if count >= 100_000 {
            return Err(Fault::Capacity);
        }
        if vector.len() != query.len() || vector.iter().any(|v| !v.is_finite()) {
            return Err(Fault::Invalid);
        }
        let norm = vector
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum::<f64>()
            .sqrt();
        if norm == 0.0 {
            continue;
        }
        let score = query
            .iter()
            .zip(&vector)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum::<f64>()
            / (qnorm * norm);
        if score >= min_score {
            hits.push(Hit {
                id,
                day,
                header,
                text,
                score,
            });
        }
        if hits.len() > top_k.saturating_mul(2).max(1) {
            hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
            hits.truncate(top_k);
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    hits.truncate(top_k);
    Ok(hits)
}
