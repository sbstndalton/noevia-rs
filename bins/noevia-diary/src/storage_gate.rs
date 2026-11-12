//! Shared credential authority with Python storage_backoff. SQLite serializes the same
//! gate's check, record and explicit reset across incarnations. Only hashes are persisted.
use diary_backup_run::R;
use diary_storage_backoff::{GateKind, StorageGate};
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub struct Gate {
    path: PathBuf,
    key: String,
}
impl Gate {
    pub fn new(path: PathBuf, base: &str, username: &str, secret: &str) -> Self {
        let base = format!("{}/", base.trim_end_matches('/'));
        let key = format!(
            "{:x}",
            Sha256::digest(format!("webdav\0{base}\0{username}\0{secret}"))
        );
        Self { path, key }
    }
    fn update<T>(&self, action: impl FnOnce(&mut StorageGate, f64) -> R<T>) -> R<T> {
        let mut db = rusqlite::Connection::open(&self.path)?;
        db.busy_timeout(std::time::Duration::from_secs(1))?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS storage_gates (key TEXT PRIMARY KEY, until REAL NOT NULL, kind TEXT NOT NULL, last_reset REAL NOT NULL)")?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(f64, String, f64)> = tx
            .query_row(
                "SELECT until,kind,last_reset FROM storage_gates WHERE key=?",
                [&self.key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let mut gate = if let Some((until, kind, reset)) = row {
            let kind = match kind.as_str() {
                "" => None,
                "login" => Some(GateKind::Login),
                "throttle" => Some(GateKind::Throttle),
                _ => return Err("gate state".into()),
            };
            StorageGate::restore(until, kind, reset)?
        } else {
            StorageGate::new()
        };
        let result = action(&mut gate, crate::write::now_epoch())?;
        let (until, kind, reset) = gate.snapshot();
        tx.execute(
            "INSERT OR REPLACE INTO storage_gates VALUES (?,?,?,?)",
            params![self.key, until, kind.map_or("", GateKind::as_str), reset],
        )?;
        tx.commit()?;
        Ok(result)
    }
    pub fn check(&self) -> R<()> {
        self.update(|gate, now| {
            if gate.check(now)?.is_some() {
                Err("cooldown".into())
            } else {
                Ok(())
            }
        })
    }
    pub fn record(&self, code: u16, retry_after: Option<&str>) -> R<()> {
        self.update(|gate, now| {
            // A backup object's 403 is per-path, not an account-root auth probe.
            gate.record_with_local_date(now, now, code, retry_after, false, |date| {
                use chrono::TimeZone;
                chrono::Local
                    .from_local_datetime(&date)
                    .earliest()
                    .map(|d| d.timestamp() as f64)
            })?;
            Ok(())
        })
    }
}
