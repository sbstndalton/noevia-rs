//! Opt-in source-half of managed Diary import APPLY (#1071). No service selector is wired.
//! Owns one existing SQLite transaction, never creation, migration, networking or indexing.
#![forbid(unsafe_code)]
mod float;
mod generation;
pub use float_test_api::format_python_float;
mod float_test_api {
    /// Source differential hook: finite Python str(float) spelling, no database effects.
    pub fn format_python_float(value: f64) -> Option<String> {
        super::float::python_float(value)
    }
}
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use workspace_restore_verify::{verify_archive, VerifiedArchive, MAX_ARCHIVE};

pub const MAGIC: &[u8] = b"noevia-workspace-import-apply-v1\0";
pub const MAX_METADATA: usize = 1024 * 1024;
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub version: u8,
    pub db_path: String,
    pub expected_tenant: String,
    pub destination: String,
    pub reviewed_fingerprint: String,
    pub debounce: f64,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub version: u8,
    pub destination: String,
    pub file_count: usize,
    pub already_applied: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Frame,
    Destination,
    Fingerprint,
    Archive(workspace_restore_verify::Reject),
    Database,
    Schema,
    Tenant,
    Inactive,
    Conflict,
    Clock,
    Generation,
}
impl Error {
    pub fn code(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::Destination => "destination",
            Self::Fingerprint => "fingerprint",
            Self::Archive(reason) => reason.code(),
            Self::Database => "database",
            Self::Schema => "schema",
            Self::Tenant => "tenant",
            Self::Inactive => "inactive",
            Self::Conflict => "conflict",
            Self::Clock => "clock",
            Self::Generation => "generation",
        }
    }
    pub fn unavailable(self) -> bool {
        matches!(
            self,
            Self::Database
                | Self::Schema
                | Self::Tenant
                | Self::Inactive
                | Self::Clock
                | Self::Generation
        )
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

/// CLI frame parser; EOF is required before opening the managed database.
pub fn read_request(mut input: impl Read) -> Result<(Request, Vec<u8>), Error> {
    let mut magic = vec![0; MAGIC.len()];
    input.read_exact(&mut magic).map_err(|_| Error::Frame)?;
    if magic != MAGIC {
        return Err(Error::Frame);
    }
    let mut size = [0; 4];
    input.read_exact(&mut size).map_err(|_| Error::Frame)?;
    let size = u32::from_le_bytes(size) as usize;
    if size > MAX_METADATA {
        return Err(Error::Frame);
    }
    let mut json = vec![0; size];
    input.read_exact(&mut json).map_err(|_| Error::Frame)?;
    let request = serde_json::from_slice(&json).map_err(|_| Error::Frame)?;
    let mut size = [0; 8];
    input.read_exact(&mut size).map_err(|_| Error::Frame)?;
    let size = usize::try_from(u64::from_le_bytes(size)).map_err(|_| Error::Frame)?;
    if size > MAX_ARCHIVE {
        return Err(Error::Archive(
            workspace_restore_verify::Reject::ArchiveTooLarge,
        ));
    }
    let mut archive = vec![0; size];
    input.read_exact(&mut archive).map_err(|_| Error::Frame)?;
    let mut tail = [0; 1];
    if input.read(&mut tail).map_err(|_| Error::Frame)? != 0 {
        return Err(Error::Frame);
    }
    Ok((request, archive))
}
fn request_valid(request: &Request) -> Result<(), Error> {
    if request.version != 1
        || !request.debounce.is_finite()
        || !Path::new(&request.db_path).is_absolute()
    {
        return Err(Error::Frame);
    }
    let Some(name) = request.destination.strip_prefix("Imports/") else {
        return Err(Error::Destination);
    };
    if name.is_empty()
        || name.chars().count() > 100
        || name.starts_with('.')
        || name.contains('/')
        || name.trim() != name
        || name.chars().any(|c| (c as u32) < 32)
        || !workspace_restore_verify::safe_key(&request.destination)
    {
        return Err(Error::Destination);
    }
    if request.expected_tenant.len() != 36
        || !request.expected_tenant.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
            }
        })
    {
        return Err(Error::Tenant);
    }
    Ok(())
}
fn checked_archive(request: &Request, body: &[u8]) -> Result<VerifiedArchive, Error> {
    request_valid(request)?;
    let archive = verify_archive(body).map_err(Error::Archive)?;
    let mut digest = Sha256::new();
    digest.update(request.destination.as_bytes());
    digest.update([0]);
    digest.update(body);
    if request.reviewed_fingerprint != format!("{:x}", digest.finalize()) {
        return Err(Error::Fingerprint);
    }
    Ok(archive)
}
/// Apply with the wall clock sampled after receipt/conflict checks under the write transaction.
pub fn apply(request: &Request, body: &[u8]) -> Result<Applied, Error> {
    apply_with_clock(request, body, || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .map_err(|_| Error::Clock)
    })
}
/// Deterministic clock injection for synthetic differential/fault qualification.
/// The caller must retain its store/migration/journal exclusion until this returns.
pub fn apply_with_clock(
    request: &Request,
    body: &[u8],
    clock: impl FnOnce() -> Result<f64, Error>,
) -> Result<Applied, Error> {
    let archive = checked_archive(request, body)?;
    // READ_WRITE explicitly excludes CREATE, URI and load-extension flags.
    let mut db = Connection::open_with_flags(
        Path::new(&request.db_path),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(30))?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    required_schema(&tx)?;
    if meta(&tx, "tenant")?.as_deref() != Some(request.expected_tenant.as_str()) {
        return Err(Error::Tenant);
    }
    if meta(&tx, "active")?.as_deref() != Some("1") {
        return Err(Error::Inactive);
    }
    let applied = Applied {
        version: 1,
        destination: request.destination.clone(),
        file_count: archive.listing().files.len(),
        already_applied: false,
    };
    // Receipt precedes collision checks: a retry must not overwrite later edits.
    if tx
        .query_row(
            "SELECT 1 FROM workspace_imports WHERE fingerprint=?",
            [&request.reviewed_fingerprint],
            |_| Ok(()),
        )
        .optional()?
        .is_some()
    {
        tx.commit()?;
        return Ok(Applied {
            already_applied: true,
            ..applied
        });
    }
    if conflicts(&tx, &request.destination)? {
        return Err(Error::Conflict);
    }
    let now = clock()?;
    let now_text = float::python_float(now).ok_or(Error::Clock)?;
    let due_text = float::python_float(now + request.debounce).ok_or(Error::Clock)?;
    let generation = generation::increment(meta(&tx, "generation")?.as_deref().unwrap_or("0"))?;
    let mut dirs = BTreeSet::from(["Imports".to_owned(), request.destination.clone()]);
    for dir in &archive.listing().directories {
        dirs.insert(format!("{}/{dir}", request.destination));
    }
    for (file, data) in archive.files() {
        let target = format!("{}/{}", request.destination, file.path);
        tx.execute(
            "INSERT INTO files VALUES (?,?,?,?)",
            params![target, data, file.sha256, now],
        )?;
        if file.path.to_lowercase().ends_with(".md") {
            tx.execute(
                "INSERT OR IGNORE INTO workspace_index_outbox VALUES (?)",
                [&target],
            )?;
        }
        for (at, _) in target.match_indices('/') {
            if let Some(parent) = target.get(..at) {
                dirs.insert(parent.to_owned());
            }
        }
    }
    for dir in dirs {
        tx.execute("INSERT OR IGNORE INTO directories VALUES (?)", [dir])?;
    }
    for (key, value) in [
        ("generation", generation),
        ("due", due_text),
        ("snapshot_time", now_text),
    ] {
        tx.execute("INSERT OR REPLACE INTO meta VALUES (?,?)", [key, &value])?;
    }
    tx.execute(
        "INSERT INTO workspace_imports VALUES (?,?)",
        [&request.reviewed_fingerprint, &request.destination],
    )?;
    tx.commit()?;
    Ok(applied)
}
fn meta(db: &Connection, key: &str) -> Result<Option<String>, Error> {
    Ok(db
        .query_row("SELECT value FROM meta WHERE key=?", [key], |row| {
            row.get(0)
        })
        .optional()?)
}
fn conflicts(db: &Connection, destination: &str) -> Result<bool, Error> {
    // Rust comparisons avoid LIKE's ASCII case-folding and wildcard semantics.
    let mut statement = db.prepare("SELECT path FROM files UNION SELECT path FROM directories")?;
    for path in statement.query_map([], |row| row.get::<_, String>(0))? {
        let path = path?;
        if path == destination
            || path
                .strip_prefix(destination)
                .is_some_and(|tail| tail.starts_with('/'))
        {
            return Ok(true);
        }
    }
    for (at, _) in destination.match_indices('/') {
        if let Some(parent) = destination.get(..at) {
            if db
                .query_row("SELECT 1 FROM files WHERE path=?", [parent], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn required_schema(db: &Connection) -> Result<(), Error> {
    type Column = (&'static str, &'static str, bool, bool);
    const TABLES: &[(&str, &[Column])] = &[
        (
            "files",
            &[
                ("path", "TEXT", false, true),
                ("data", "BLOB", true, false),
                ("version", "TEXT", true, false),
                ("updated", "REAL", true, false),
            ],
        ),
        ("directories", &[("path", "TEXT", false, true)]),
        (
            "workspace_imports",
            &[
                ("fingerprint", "TEXT", false, true),
                ("destination", "TEXT", true, false),
            ],
        ),
        ("workspace_index_outbox", &[("path", "TEXT", false, true)]),
        (
            "meta",
            &[("key", "TEXT", false, true), ("value", "TEXT", true, false)],
        ),
    ];
    for (table, expected) in TABLES {
        let table_type: Option<String> = db
            .query_row(
                "SELECT type FROM sqlite_master WHERE name=?",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        if table_type.as_deref() != Some("table") {
            return Err(Error::Schema);
        }
        let mut query = db.prepare(&format!("PRAGMA table_xinfo({table})"))?;
        let columns = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, u8>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if columns.len() != expected.len()
            || !columns.iter().zip(*expected).all(|(actual, expected)| {
                actual.0 == expected.0
                    && actual.1 == expected.1
                    && actual.2 == expected.2
                    && actual.3 == expected.3
                    && actual.4 == 0
            })
        {
            return Err(Error::Schema);
        }
        // Require BINARY primary-key comparison; NOCASE/RTRIM changes tenant/path semantics.
        let mut indexes = db.prepare(&format!("PRAGMA index_list({table})"))?;
        for index in indexes.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(3)?))
        })? {
            let (name, origin) = index?;
            if origin == "pk" {
                let mut collation =
                    db.prepare("SELECT coll FROM pragma_index_xinfo(?) WHERE key=1")?;
                if collation
                    .query_map([name], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
                    .iter()
                    .any(|coll| coll != "BINARY")
                {
                    return Err(Error::Schema);
                }
            }
        }
    }
    Ok(())
}
