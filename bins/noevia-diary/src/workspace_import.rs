//! Native import orchestration; verification/decoding and atomic apply use the existing leaves.
use crate::{managed, py, workspace::Reply, write};
use diary_workspace_import_apply as apply;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;

pub const UNCERTAIN: &str = "Workspace import outcome is uncertain. Retry the same archive and destination with the same reviewed fingerprint.";
fn detail(status: u16, msg: &str) -> Reply {
    Reply::Json(
        status,
        serde_json::to_vec(&serde_json::json!({"detail": msg})).unwrap_or_default(),
    )
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Preview {
    fingerprint: String,
    destination: String,
    file_count: usize,
    bytes: u64,
    files: Vec<String>,
    directories: Vec<String>,
    conflicts: BTreeSet<String>,
    duplicates: Vec<String>,
    mode: &'static str,
    already_applied: bool,
    #[serde(skip)]
    generation_supported: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Applied {
    destination: String,
    file_count: usize,
    already_applied: bool,
    index_pending: bool,
}

pub struct Options<'a> {
    pub action: &'a str,
    pub name: &'a str,
    pub reviewed: &'a str,
    /// Preserve the configured Python adapter's public failure classification.
    pub native_errors: bool,
}

/// Called only with the tenant's Writers exclusion held through all synchronous effects.
pub fn perform(
    db_path: &Path,
    index: &Path,
    uid: &str,
    body: &[u8],
    options: Options<'_>,
) -> Reply {
    let Options {
        action,
        name,
        reviewed,
        native_errors,
    } = options;
    let status = if action == "apply" { 409 } else { 400 };
    let archive = match workspace_restore_verify::verify_archive(body) {
        Ok(a) => a, // The leaf deliberately rejects some containers accepted by zipfile.
        // Decline before effects so the configured Python verifier owns every rejection.
        Err(_) => return Reply::Decline,
    };
    let name = py::strip(name);
    if name.is_empty()
        || name.chars().count() > 100
        || name.contains('/')
        || name.starts_with('.')
        || name.chars().any(|c| (c as u32) < 32)
    {
        return detail(status, "Choose a new folder name of 1–100 characters");
    }
    let destination = format!("Imports/{name}");
    if !managed::safe_key(&destination) {
        return detail(status, "Invalid corpus path");
    }
    let mut hash = Sha256::new();
    hash.update(destination.as_bytes());
    hash.update([0]);
    hash.update(body);
    let fingerprint = format!("{:x}", hash.finalize());
    let listing = archive.listing();
    let preview = (|| -> Result<Preview, rusqlite::Error> {
        let mut db = managed::open_ro(db_path).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = db.transaction()?;
        let already_applied = tx
            .query_row(
                "SELECT 1 FROM workspace_imports WHERE fingerprint=?",
                [&fingerprint],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let mut conflicts = BTreeSet::new();
        let mut stmt = tx.prepare("SELECT path FROM files UNION SELECT path FROM directories")?;
        for path in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let path = path?;
            if path == destination || path.starts_with(&format!("{destination}/")) {
                conflicts.insert(path);
            }
        }
        if tx
            .query_row("SELECT 1 FROM files WHERE path='Imports'", [], |_| Ok(()))
            .optional()?
            .is_some()
        {
            conflicts.insert("Imports".into());
        }
        let mut stmt = tx.prepare("SELECT version FROM files")?;
        let hashes = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        // Read-only, under the caller's tenant writer exclusion. A default Python apply may
        // raise ValueError with runtime-specific text for either int() or str(int + 1).
        // Let that actual handler own every unsupported conversion before applying anything.
        // Receipts and conflicts precede generation conversion in both implementations.
        let generation_supported = action != "apply"
            || native_errors
            || already_applied
            || !conflicts.is_empty()
            || tx
                .query_row("SELECT value FROM meta WHERE key='generation'", [], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
                .is_ok_and(|value| {
                    apply::increment_generation(value.as_deref().unwrap_or("0")).is_ok()
                });
        Ok(Preview {
            fingerprint: fingerprint.clone(),
            destination: destination.clone(),
            file_count: listing.files.len(),
            bytes: listing.bytes,
            files: listing.files.iter().map(|f| f.path.clone()).collect(),
            directories: listing.directories.clone(),
            conflicts,
            duplicates: listing
                .files
                .iter()
                .filter(|f| hashes.contains(&f.sha256))
                .map(|f| f.path.clone())
                .collect(),
            mode: "new-folder-only",
            already_applied,
            generation_supported,
        })
    })();
    let preview = match preview {
        Ok(p) => p,
        Err(_) => return crate::writes::internal(),
    };
    if action == "preview" {
        return Reply::Json(200, serde_json::to_vec(&preview).unwrap_or_default());
    }
    if reviewed != fingerprint {
        return detail(
            409,
            "Archive or destination changed. Review a fresh preview.",
        );
    }
    if !preview.generation_supported {
        return Reply::Decline;
    }
    let request = apply::Request {
        version: 1,
        db_path: db_path.to_string_lossy().into_owned(),
        expected_tenant: uid.into(),
        destination,
        reviewed_fingerprint: reviewed.into(),
        debounce: 3.0,
    };
    let result = match apply::apply(&request, body) {
        Ok(r) => r,
        Err(apply::Error::Conflict) => {
            return detail(
                409,
                "Destination already exists or conflicts. Choose a new folder.",
            )
        }
        Err(apply::Error::Database) if native_errors => return detail(503, UNCERTAIN),
        Err(_) if native_errors => {
            return detail(503, "Workspace import is unavailable. Try again later.")
        }
        // The in-process Python implementation lets unexpected transaction exceptions reach
        // Starlette's plain 500. The native adapter classifies them as above instead.
        Err(_) => return crate::writes::internal(),
    };
    // Mark dirty durably before removing an outbox row. Failure leaves it for ordinary recovery.
    let _ = drain(db_path, index);
    Reply::Json(
        200,
        serde_json::to_vec(&Applied {
            destination: result.destination,
            file_count: result.file_count,
            already_applied: result.already_applied,
            index_pending: true,
        })
        .unwrap_or_default(),
    )
}
fn drain(db_path: &Path, index: &Path) -> Result<(), write::Error> {
    let mut db = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_secs(30))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let journal = write::Journal::open(index)?;
    let paths = tx
        .prepare("SELECT path FROM workspace_index_outbox")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for path in paths {
        journal.mark_dirty(&path)?;
        tx.execute("DELETE FROM workspace_index_outbox WHERE path=?", [path])?;
    }
    tx.commit()?;
    Ok(())
}
