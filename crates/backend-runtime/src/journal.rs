//! Native owner of the existing Diary index.db journal layout. Replay is insertion ordered.
use crate::{Fault, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::Mutex, time::Duration};
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS dirty_documents(document TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS journal(id TEXT PRIMARY KEY,created_at TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL,applied INTEGER NOT NULL DEFAULT 0,applied_at TEXT,attempts INTEGER NOT NULL DEFAULT 0,last_error TEXT);
CREATE INDEX IF NOT EXISTS idx_journal_unapplied ON journal(applied,created_at);";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: String,
    pub created_at: String,
    pub kind: String,
    pub payload: Value,
    pub applied: bool,
    pub attempts: i64,
    pub last_error: Option<String>,
}
pub struct Journal {
    db: Mutex<Connection>,
}
fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}
fn entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let raw: String = row.get("payload")?;
    let payload = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Entry {
        id: row.get("id")?,
        created_at: row.get("created_at")?,
        kind: row.get("kind")?,
        payload,
        applied: row.get("applied")?,
        attempts: row.get("attempts")?,
        last_error: row.get("last_error")?,
    })
}
impl Journal {
    /// Explicit construction; no connection is ever selected from request-supplied paths.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(30))?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn enqueue(&self, kind: &str, payload: &Value) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.enqueue_id(&id, kind, payload, &stamp())?;
        Ok(id)
    }
    /// Exact caller identity supports a lost reply without inserting a second intent.
    /// The legacy enqueue API generates fresh UUIDs; retry identity is opt-in.
    pub fn enqueue_id(&self, id: &str, kind: &str, payload: &Value, created: &str) -> Result<()> {
        if uuid::Uuid::parse_str(id).is_err()
            || kind.is_empty()
            || kind.len() > 128
            || !payload.is_object()
            || created.len() > 64
        {
            return Err(Fault::Invalid);
        }
        let raw = serde_json::to_string(payload)?;
        if raw.len() > 8 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row("SELECT kind,payload FROM journal WHERE id=?", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        if let Some((old_kind, old_payload)) = existing {
            if old_kind != kind || serde_json::from_str::<Value>(&old_payload)? != *payload {
                return Err(Fault::Conflict);
            }
        } else {
            tx.execute(
                "INSERT INTO journal(id,created_at,kind,payload) VALUES(?,?,?,?)",
                params![id, created, kind, raw],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn mark_applied(&self, id: &str) -> Result<()> {
        self.update(
            id,
            "UPDATE journal SET applied=1,applied_at=?2,last_error=NULL WHERE id=?1",
            &stamp(),
        )
    }
    pub fn mark_failed(&self, id: &str, error: &str) -> Result<()> {
        self.update(
            id,
            "UPDATE journal SET attempts=attempts+1,last_error=?2 WHERE id=?1",
            &error.chars().take(500).collect::<String>(),
        )
    }
    pub fn quarantine(&self, id: &str, error: &str) -> Result<()> {
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        db.execute(
            "UPDATE journal SET applied=1,applied_at=?,attempts=attempts+1,last_error=? WHERE id=?",
            params![stamp(), error.chars().take(2000).collect::<String>(), id],
        )?;
        Ok(())
    }
    fn update(&self, id: &str, sql: &str, value: &str) -> Result<()> {
        self.db
            .lock()
            .map_err(|_| Fault::Database)?
            .execute(sql, params![id, value])?;
        Ok(())
    }
    pub fn unapplied(&self, limit: u32) -> Result<Vec<Entry>> {
        if limit > 10000 {
            return Err(Fault::Capacity);
        }
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        let mut q = db.prepare("SELECT * FROM journal WHERE applied=0 ORDER BY rowid LIMIT ?")?;
        let result = q
            .query_map([limit], entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
    pub fn unapplied_exchange_edit(&self, xid: &str) -> Result<Option<Entry>> {
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        let mut q = db.prepare(
            "SELECT * FROM journal WHERE applied=0 AND kind='exchange_edit' ORDER BY rowid DESC",
        )?;
        for item in q.query_map([], entry)? {
            let item = item?;
            if item.payload.get("xid").and_then(Value::as_str) == Some(xid) {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }
    pub fn unapplied_after_with_xid(&self, after: i64, xid: &str) -> Result<Vec<Entry>> {
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        let mut q =
            db.prepare("SELECT * FROM journal WHERE applied=0 AND rowid>? ORDER BY rowid")?;
        let mut result = Vec::new();
        for item in q.query_map([after], entry)? {
            let item = item?;
            if item.payload.get("xid").and_then(Value::as_str) == Some(xid) {
                if result.len() >= 10000 {
                    return Err(Fault::Capacity);
                }
                result.push(item);
            }
        }
        Ok(result)
    }
    pub fn rowid_of(&self, id: &str) -> Result<i64> {
        Ok(self
            .db
            .lock()
            .map_err(|_| Fault::Database)?
            .query_row("SELECT rowid FROM journal WHERE id=?", [id], |r| r.get(0))
            .optional()?
            .unwrap_or(-1))
    }
    pub fn is_applied(&self, id: &str) -> Result<bool> {
        Ok(self
            .db
            .lock()
            .map_err(|_| Fault::Database)?
            .query_row("SELECT applied FROM journal WHERE id=?", [id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }
    pub fn has_exchange(&self) -> Result<bool> {
        Ok(self.db.lock().map_err(|_| Fault::Database)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM journal WHERE kind='exchange')",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn pending_count(&self) -> Result<i64> {
        self.count("SELECT COUNT(*) FROM journal WHERE applied=0")
    }
    pub fn quarantined_count(&self) -> Result<i64> {
        self.count(
            "SELECT COUNT(*) FROM journal WHERE applied=1 AND last_error LIKE 'quarantined:%'",
        )
    }
    fn count(&self, sql: &str) -> Result<i64> {
        Ok(self
            .db
            .lock()
            .map_err(|_| Fault::Database)?
            .query_row(sql, [], |r| r.get(0))?)
    }
    pub fn mark_dirty(&self, document: &str) -> Result<()> {
        self.db.lock().map_err(|_| Fault::Database)?.execute(
            "INSERT OR IGNORE INTO dirty_documents(document) VALUES(?)",
            [document],
        )?;
        Ok(())
    }
    pub fn clear_dirty(&self, document: &str) -> Result<()> {
        self.db
            .lock()
            .map_err(|_| Fault::Database)?
            .execute("DELETE FROM dirty_documents WHERE document=?", [document])?;
        Ok(())
    }
    pub fn dirty_documents(&self) -> Result<Vec<String>> {
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        let mut q = db.prepare("SELECT document FROM dirty_documents")?;
        let result = q
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
}
