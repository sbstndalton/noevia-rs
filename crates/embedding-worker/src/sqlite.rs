//! Existing Node RAG schema only: never CREATE an index. Apply only Node's
//! four historical additive chunk columns before using an older index.
//! Account reads use server-store's checked cowork.db reader. Publication holds
//! its IMMEDIATE writer transaction (without changing auth rows), projects.json
//! flock, existing outbox IMMEDIATE transaction and RAG IMMEDIATE transaction.
use crate::{
    fill::{Row, Store as FillStore},
    Config, Control, Index, Target,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};
const EMBEDDABLE:&str="COALESCE(c.kind,'text')='text' AND NOT (COALESCE(c.direct,0)=1 OR (c.direct IS NULL AND LENGTH(c.body)<=2400 AND (SELECT COUNT(*) FROM chunks c2 WHERE c2.file=c.file AND COALESCE(c2.kind,'text')='text')=1))";
fn fault() -> String {
    "embedding index unavailable".into()
}
pub fn slot_id(target: &Target) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{}\0{}", target.profile, target.model).as_bytes())
    )
    .chars()
    .take(16)
    .collect()
}
#[derive(Clone)]
pub struct SqlStore {
    pub root: PathBuf,
    /// Shared index directory (RAG_DIR); tenant indexes remain users/<id>/rag.
    pub rag_dir: Option<PathBuf>,
    pub seed: Config,
    /// Existing-only opener. The caller supplies a reviewed sqlite-vec loader;
    /// absence must fail closed, never replace vec0 with an ordinary table.
    pub open_vec: fn(&Path) -> Result<Connection, String>,
}
pub fn open_existing(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| fault())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|_| fault())?;
    Ok(conn)
}
pub fn open_vec_required(path: &Path) -> Result<Connection, String> {
    crate::sqlite_vec_loader::register()?;
    let conn = open_existing(path)?;
    let version = conn
        .query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
        .map_err(|_| "sqlite-vec loader unavailable".to_owned())?;
    if version != "v0.1.9" {
        return Err("sqlite-vec version mismatch".into());
    }
    Ok(conn)
}
fn safe_id(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('.') && !value.contains(['/', '\\', '\0'])
}
fn table(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|s| {
        s.len() == 16
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
#[derive(Clone)]
struct Slot {
    table: String,
    map: Option<String>,
    dim: usize,
}
fn find(conn: &Connection, target: &Target) -> Result<Option<Slot>, String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM vec_meta WHERE key=?",
            [format!("slot:{}", slot_id(target))],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| fault())?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&raw).map_err(|_| fault())?;
    let name = value
        .get("table")
        .and_then(Value::as_str)
        .ok_or_else(fault)?;
    let map = value
        .get("map")
        .filter(|v| !v.is_null())
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(fault))
        .transpose()?;
    let dim = value
        .get("dim")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (1..=65536).contains(n))
        .ok_or_else(fault)?;
    if !(name == "vec_items" || table(name, "vec_"))
        || map.as_ref().is_some_and(|s| !table(s, "vecmap_"))
    {
        return Err(fault());
    }
    Ok(Some(Slot {
        table: name.into(),
        map,
        dim,
    }))
}
fn missing_sql(slot: Option<&Slot>) -> String {
    match slot {
        None=>format!("SELECT c.id FROM chunks c WHERE {EMBEDDABLE}"),
        Some(s)=>match &s.map {Some(map)=>format!("SELECT c.id FROM chunks c LEFT JOIN {map} m ON m.chunk_id=c.id AND m.content_hash=c.content_hash WHERE m.chunk_id IS NULL AND {EMBEDDABLE}"),None=>format!("SELECT c.id FROM chunks c WHERE c.id NOT IN (SELECT chunk_id FROM {}) AND {EMBEDDABLE}",s.table)}
    }
}
fn read_missing_sql(db: &Connection, slot: Option<&Slot>) -> Result<String, String> {
    let mut cols = BTreeSet::new();
    let mut statement = db
        .prepare("PRAGMA table_info(chunks)")
        .map_err(|_| fault())?;
    for c in statement
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|_| fault())?
    {
        cols.insert(c.map_err(|_| fault())?);
    }
    let mut sql = missing_sql(slot);
    if !cols.contains("kind") {
        sql = sql.replace("c2.kind", "NULL").replace("c.kind", "NULL");
    }
    if !cols.contains("direct") {
        sql = sql.replace("c.direct", "NULL");
    }
    Ok(sql)
}
fn exists(conn: &Connection, name: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?)",
        [name],
        |r| r.get(0),
    )
    .map_err(|_| fault())
}
fn legacy_claimed(conn: &Connection) -> Result<bool, String> {
    let mut stmt = conn
        .prepare("SELECT value FROM vec_meta WHERE key LIKE 'slot:%'")
        .map_err(|_| fault())?;
    for raw in stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|_| fault())?
    {
        let raw = raw.map_err(|_| fault())?;
        if serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("table").and_then(Value::as_str).map(str::to_owned))
            .as_deref()
            == Some("vec_items")
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn ensure(conn: &Connection, target: &Target, dim: usize) -> Result<Slot, String> {
    if let Some(slot) = find(conn, target)? {
        return Ok(slot);
    }
    if dim == 0 || dim > 65536 {
        return Err(fault());
    }
    let id = slot_id(target);
    let slot = if target.profile == "nomic-v1"
        && !legacy_claimed(conn)?
        && !exists(conn, "vec_items")?
    {
        conn.execute_batch(&format!("CREATE VIRTUAL TABLE IF NOT EXISTS vec_items USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}])")).map_err(|_|fault())?;
        conn.execute(
            "INSERT OR REPLACE INTO vec_meta(key,value) VALUES('dim',?)",
            [dim.to_string()],
        )
        .map_err(|_| fault())?;
        Slot {
            table: "vec_items".into(),
            map: None,
            dim,
        }
    } else {
        let name = format!("vec_{id}");
        let map = format!("vecmap_{id}");
        conn.execute_batch(&format!("CREATE VIRTUAL TABLE IF NOT EXISTS {name} USING vec0(chunk_id INTEGER PRIMARY KEY, embedding float[{dim}]);CREATE TABLE IF NOT EXISTS {map}(chunk_id INTEGER PRIMARY KEY,content_hash TEXT NOT NULL)")).map_err(|_|fault())?;
        Slot {
            table: name,
            map: Some(map),
            dim,
        }
    };
    conn.execute("INSERT OR REPLACE INTO vec_meta(key,value) VALUES(?,?)",params![format!("slot:{id}"),json!({"model":target.model,"profile":target.profile,"dim":dim,"table":slot.table,"map":slot.map}).to_string()]).map_err(|_|fault())?;
    Ok(slot)
}
impl SqlStore {
    pub fn config(&self) -> Result<Config, String> {
        crate::runtime::load_config(&self.root, &self.seed)
    }
    pub fn users(&self) -> Result<Vec<String>, String> {
        server_store::Store::open(&self.root)
            .map_err(|_| fault())?
            .read(|r| r.rows("SELECT id FROM users", [], |r| r.get(0)))
            .map_err(|_| fault())
    }
    fn dir(&self, index: &Index) -> Result<PathBuf, String> {
        if !safe_id(&index.project) {
            return Err(fault());
        }
        let dir = match &index.user {
            Some(user) => {
                server_projects::store::user_dir(&self.root, user).map_err(|_| fault())?
            }
            None => self.root.clone(),
        };
        for path in [self.root.join("users"), dir.clone(), self.index_dir(index)?] {
            if index.user.is_none() && path == self.root.join("users") {
                continue;
            }
            if std::fs::symlink_metadata(&path)
                .map_err(|_| fault())?
                .file_type()
                .is_symlink()
            {
                return Err(fault());
            }
        }
        Ok(dir)
    }
    pub fn index_dir(&self, index: &Index) -> Result<PathBuf, String> {
        match &index.user {
            Some(user) => Ok(server_projects::store::user_dir(&self.root, user)
                .map_err(|_| fault())?
                .join("rag")),
            None => Ok(self
                .rag_dir
                .clone()
                .unwrap_or_else(|| self.root.join("rag"))),
        }
    }
    pub fn index_path(&self, index: &Index) -> Result<PathBuf, String> {
        if !safe_id(&index.project) {
            return Err(fault());
        }
        Ok(self.index_dir(index)?.join(format!("{}.db", index.project)))
    }
    pub fn indexes(&self) -> Vec<Index> {
        let mut dirs = vec![(
            None,
            self.rag_dir
                .clone()
                .unwrap_or_else(|| self.root.join("rag")),
        )];
        for user in self.users().unwrap_or_default() {
            if let Ok(dir) = server_projects::store::user_dir(&self.root, &user) {
                dirs.push((Some(user), dir.join("rag")));
            }
        }
        let mut out = vec![];
        for (user, dir) in dirs {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|t| t.is_file()) {
                        continue;
                    }
                    if let Some(project) = entry
                        .file_name()
                        .to_str()
                        .and_then(|s| s.strip_suffix(".db"))
                        .filter(|s| safe_id(s))
                    {
                        let index = Index {
                            user: user.clone(),
                            project: project.into(),
                        };
                        if self.dir(&index).is_ok() {
                            out.push(index);
                        }
                    }
                }
            }
        }
        out
    }

    fn live(&self, index: &Index) -> Result<(), String> {
        if let Some(user) = &index.user {
            let live = server_store::Store::open(&self.root)
                .map_err(|_| fault())?
                .read(|r| {
                    r.row("SELECT id FROM users WHERE id=?", [user], |r| {
                        r.get::<_, String>(0)
                    })
                })
                .map_err(|_| fault())?;
            if live.is_none() {
                return Err(fault());
            }
            let outbox = self.dir(index)?.join("index-outbox.db");
            if outbox.exists() {
                let db = Connection::open_with_flags(
                    outbox,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|_| fault())?;
                let dead: bool = db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM tombstones WHERE project=?)",
                        [&index.project],
                        |r| r.get(0),
                    )
                    .map_err(|_| fault())?;
                if dead {
                    return Err(fault());
                }
            }
        }
        Ok(())
    }
    fn read_index<T>(
        &self,
        index: &Index,
        run: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        self.live(index)?;
        self.dir(index)?;
        let path = self.index_path(index)?;
        if std::fs::symlink_metadata(&path)
            .map_err(|_| fault())?
            .file_type()
            .is_symlink()
        {
            return Err(fault());
        }
        crate::sqlite_vec_loader::register()?;
        let mut db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| fault())?;
        db.busy_timeout(Duration::from_secs(5))
            .map_err(|_| fault())?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .map_err(|_| fault())?;
        let result = run(&tx)?;
        tx.commit().map_err(|_| fault())?;
        self.live(index)?;
        Ok(result)
    }
    fn read_slot(&self, db: &Connection, target: &Target) -> Result<Option<Slot>, String> {
        let slot = find(db, target)?;
        if slot.is_some() {
            return Ok(slot);
        }
        let active = self.config()?.active;
        if target.same_identity(&active)
            && active.profile == "nomic-v1"
            && exists(db, "vec_items")?
            && !legacy_claimed(db)?
        {
            let dim: Option<String> = db
                .query_row("SELECT value FROM vec_meta WHERE key='dim'", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|_| fault())?;
            return Ok(dim
                .and_then(|s| s.parse().ok())
                .filter(|n| (1..=65536).contains(n))
                .map(|dim| Slot {
                    table: "vec_items".into(),
                    map: None,
                    dim,
                }));
        }
        Ok(None)
    }
    fn prepare(&self, db: &Connection) -> Result<(), String> {
        let mut columns = BTreeSet::new();
        {
            let mut statement = db
                .prepare("PRAGMA table_info(chunks)")
                .map_err(|_| fault())?;
            for c in statement
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(|_| fault())?
            {
                columns.insert(c.map_err(|_| fault())?);
            }
        }
        if !["id", "file", "body", "content_hash"]
            .iter()
            .all(|c| columns.contains(*c))
        {
            return Err(fault());
        }
        for (column, kind) in [
            ("version", "TEXT"),
            ("direct", "INTEGER"),
            ("kind", "TEXT"),
            ("meta", "TEXT"),
        ] {
            if !columns.contains(column) {
                db.execute_batch(&format!("ALTER TABLE chunks ADD COLUMN {column} {kind}"))
                    .map_err(|_| fault())?;
            }
        }
        let active = self.config()?.active;
        if active.profile == "nomic-v1"
            && exists(db, "vec_items")?
            && !legacy_claimed(db)?
            && find(db, &active)?.is_none()
        {
            let dim: Option<String> = db
                .query_row("SELECT value FROM vec_meta WHERE key='dim'", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|_| fault())?;
            if let Some(dim) = dim
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| (1..=65536).contains(n))
            {
                db.execute("INSERT OR REPLACE INTO vec_meta(key,value) VALUES(?,?)",params![format!("slot:{}",slot_id(&active)),json!({"model":active.model,"profile":active.profile,"dim":dim,"table":"vec_items","map":null}).to_string()]).map_err(|_|fault())?;
            }
        }
        Ok(())
    }
    /// Only publication takes cowork.db's IMMEDIATE account-deletion fence.
    /// Selection/coverage use deferred, read-only connections and recheck liveness.
    fn fenced<T>(
        &self,
        index: &Index,
        publish: bool,
        run: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let _config = server_projects::lock::acquire(&self.root.join("embedding-config.json"))
            .map_err(|_| fault())?;
        let operation = || {
            let dir = self.dir(index)?;
            let _projects =
                server_projects::lock::acquire(&dir.join("projects.json")).map_err(|_| fault())?;
            let mut outbox = if index.user.is_some() && dir.join("index-outbox.db").exists() {
                Some(open_existing(&dir.join("index-outbox.db"))?)
            } else {
                None
            };
            let _outbox_tx = match outbox.as_mut() {
                Some(db) => {
                    let tx = db
                        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                        .map_err(|_| fault())?;
                    let dead: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM tombstones WHERE project=?)",
                            [&index.project],
                            |r| r.get(0),
                        )
                        .map_err(|_| fault())?;
                    if dead {
                        return Err(fault());
                    }
                    Some(tx)
                }
                None => None,
            };
            let path = self.index_path(index)?;
            if std::fs::symlink_metadata(&path)
                .map_err(|_| fault())?
                .file_type()
                .is_symlink()
            {
                return Err(fault());
            }
            let mut db = (self.open_vec)(&path)?;
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|_| fault())?;
            self.prepare(&tx)?;
            let result = run(&tx)?;
            tx.commit().map_err(|_| fault())?;
            Ok(result)
        };
        if publish && index.user.is_some() {
            let writer = server_store::Writer::open(&self.root, server_store::RustAuth::owned())
                .map_err(|_| fault())?;
            writer
                .write(|auth| {
                    let live = auth.reader().row(
                        "SELECT id FROM users WHERE id=?",
                        [index.user.as_deref().unwrap_or_default()],
                        |r| r.get::<_, String>(0),
                    )?;
                    if live.is_none() {
                        return Ok(Err(fault()));
                    }
                    Ok(operation())
                })
                .map_err(|_| fault())?
        } else {
            self.live(index)?;
            operation()
        }
    }
    pub fn drop_slot(&self, index: &Index, target: &Target) -> Result<(), String> {
        self.fenced(index, false, |db| {
            if let Some(slot) = find(db, target)? {
                let config = self.config()?;
                if target.same_identity(&config.active)
                    || config
                        .previous
                        .as_ref()
                        .is_some_and(|p| target.same_identity(p))
                {
                    return Ok(());
                }
                if slot.table == "vec_items" {
                    db.execute_batch("DELETE FROM vec_items")
                        .map_err(|_| fault())?;
                } else {
                    db.execute_batch(&format!("DROP TABLE IF EXISTS {}", slot.table))
                        .map_err(|_| fault())?;
                    if let Some(map) = slot.map {
                        db.execute_batch(&format!("DROP TABLE IF EXISTS {map}"))
                            .map_err(|_| fault())?;
                    }
                }
                db.execute(
                    "DELETE FROM vec_meta WHERE key=?",
                    [format!("slot:{}", slot_id(target))],
                )
                .map_err(|_| fault())?;
            }
            Ok(())
        })
    }
}
impl FillStore for SqlStore {
    fn missing(
        &self,
        index: &Index,
        target: &Target,
        limit: usize,
        exclude: &BTreeSet<i64>,
    ) -> Result<Option<Vec<Row>>, String> {
        self.read_index(index,|db| {
            let slot=self.read_slot(db,target)?;let skip=exclude.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            let sql=format!("SELECT c.id,c.content_hash,c.body FROM chunks c WHERE c.id IN ({}) {} ORDER BY c.id LIMIT ?",read_missing_sql(db,slot.as_ref())?,if skip.is_empty() {String::new()} else {format!("AND c.id NOT IN ({skip})")});
            let mut stmt=db.prepare(&sql).map_err(|_|fault())?;
            let rows=stmt.query_map([limit.min(16) as i64],|r|Ok(Row {id:r.get(0)?,hash:r.get(1)?,text:r.get::<_,String>(2)?.encode_utf16().collect()})).map_err(|_|fault())?.collect::<Result<Vec<_>,_>>().map_err(|_|fault())?;Ok(Some(rows))
        })
    }
    fn remaining(&self, index: &Index, target: &Target) -> Result<usize, String> {
        self.read_index(index, |db| {
            let slot = self.read_slot(db, target)?;
            db.query_row(
                &format!(
                    "SELECT COUNT(*) FROM ({})",
                    read_missing_sql(db, slot.as_ref())?
                ),
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|_| fault())
            .and_then(|n| usize::try_from(n).map_err(|_| fault()))
        })
    }
    fn current(&self, version: u64, control: &Control) -> bool {
        !control.stopped()
            && !control.suspended()
            && self.config().is_ok_and(|c| c.version == version)
    }
    fn publish(
        &self,
        index: &Index,
        target: &Target,
        rows: &[Row],
        vectors: &[Vec<f32>],
        version: u64,
        control: &Control,
    ) -> Result<usize, String> {
        self.fenced(index, true, |db| {
            if !control.ownership_fresh() {
                control.suspend();
                return Err("ownership proof expired while waiting for publication fence".into());
            }
            if !self.current(version, control) {
                return Ok(0);
            }
            let dim = vectors.first().map(Vec::len).ok_or_else(fault)?;
            let slot = ensure(db, target, dim)?;
            let mut written = 0;
            for (row, vector) in rows.iter().zip(vectors) {
                if vector.len() != slot.dim || vector.iter().any(|v| !v.is_finite()) {
                    continue;
                }
                let still: bool = db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM chunks WHERE id=? AND content_hash=?)",
                        params![row.id, row.hash],
                        |r| r.get(0),
                    )
                    .map_err(|_| fault())?;
                if !still {
                    continue;
                }
                db.execute(
                    &format!("DELETE FROM {} WHERE chunk_id=?", slot.table),
                    [row.id],
                )
                .map_err(|_| fault())?;
                let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
                db.execute(
                    &format!(
                        "INSERT INTO {}(chunk_id,embedding) VALUES({},?)",
                        slot.table, row.id
                    ),
                    [bytes],
                )
                .map_err(|_| fault())?;
                if let Some(map) = &slot.map {
                    db.execute(
                        &format!("INSERT OR REPLACE INTO {map}(chunk_id,content_hash) VALUES(?,?)"),
                        params![row.id, row.hash],
                    )
                    .map_err(|_| fault())?;
                }
                written += 1;
            }
            if !self.current(version, control) {
                return Err("embedding configuration changed".into());
            }
            if !control.ownership_fresh() {
                control.suspend();
                return Err("ownership proof expired during publication".into());
            }
            Ok(written)
        })
    }
}
