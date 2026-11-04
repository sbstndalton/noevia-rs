//! Read-only access to an app-managed Diary (`users/<uid>/managed-diary.db`), byte-compatible with
//! managed_storage.py. This process never writes a tenant's files (D4 is read routes only; the
//! single-writer rule keeps every write in Python until D5).

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug)]
pub struct Error(pub String);

impl<E: std::fmt::Display> From<E> for Error {
    fn from(e: E) -> Self {
        Error(e.to_string())
    }
}

pub struct Managed {
    db_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// managed_storage.safe_key.
pub fn safe_key(path: &str) -> bool {
    !(path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".."))
}

fn open_ro(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(30))?;
    Ok(conn)
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>, Error> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .optional()?)
}

fn count(conn: &Connection, sql: &str) -> Result<i64, Error> {
    Ok(conn.query_row(sql, [], |r| r.get(0))?)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
            [name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The tenant settings stored at activation (`meta.settings`), as raw JSON values.
pub type Settings = serde_json::Map<String, Value>;

/// Is this tenant an active, consistent managed Diary whose reply Python would not change by
/// recovering first (no pending journal entries)? Some(settings) when it is; None
/// means "let Python serve it" (Python then builds, recovers or refuses exactly as today).
/// Nothing here writes, and nothing depends on the request's credentials.
pub fn ready(tenant_root: &Path, uid: &str) -> Result<Option<(Managed, Settings)>, Error> {
    let marker = tenant_root.join("managed-diary.active");
    let db_path = tenant_root.join("managed-diary.db");
    let Ok(identity) = std::fs::read(&marker) else {
        return Ok(None);
    };
    if identity != uid.as_bytes() || !db_path.is_file() {
        return Ok(None);
    }
    let conn = open_ro(&db_path)?;
    for table in [
        "files",
        "directories",
        "meta",
        "workspace_index_outbox",
        "backups",
    ] {
        if !table_exists(&conn, table)? {
            return Ok(None);
        }
    }
    if meta(&conn, "tenant")?.as_deref() != Some(uid)
        || meta(&conn, "active")?.as_deref() != Some("1")
    {
        return Ok(None);
    }
    let settings = match meta(&conn, "settings")? {
        None => Settings::new(),
        Some(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            _ => return Ok(None),
        },
    };
    drop(conn);
    // Python applies pending journal entries to the corpus on a state's first request, which
    // changes what a read returns: leave those tenants to it. Dirty documents and the index outbox
    // only feed the index (never these replies); Python reindexes them on its next request for
    // the tenant (every write and chat drains them).
    let index = tenant_root.join("managed-index.db");
    if index.is_file() {
        let idx = open_ro(&index)?;
        if table_exists(&idx, "journal")?
            && count(&idx, "SELECT COUNT(*) FROM journal WHERE applied = 0")? != 0
        {
            return Ok(None);
        }
    }
    Ok(Some((Managed { db_path }, settings)))
}

impl Managed {
    fn conn(&self) -> Result<Connection, Error> {
        open_ro(&self.db_path)
    }

    /// `get_text`: the file decoded as UTF-8 with errors="replace", or None.
    pub fn get_text(&self, path: &str) -> Result<Option<String>, Error> {
        if !safe_key(path) {
            return Err(Error("Invalid corpus path".into()));
        }
        let conn = self.conn()?;
        let data: Option<Vec<u8>> = conn
            .query_row("SELECT data FROM files WHERE path=?", [path], |r| r.get(0))
            .optional()?;
        Ok(data.map(|d| String::from_utf8_lossy(&d).into_owned()))
    }

    /// `list_dir`: direct children of `path` ("" is the root), sorted by lower-cased name, with
    /// Python's dict semantics (a later row for the same name replaces the earlier one but keeps
    /// the first position).
    pub fn list_dir(&self, path: &str) -> Result<Vec<Entry>, Error> {
        if !path.is_empty() && !safe_key(path) {
            return Err(Error("Invalid corpus path".into()));
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT path,version FROM files UNION ALL SELECT path,NULL FROM directories",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut entries: Vec<Entry> = Vec::new();
        for row in rows {
            let (p, version) = row?;
            let Some(rest) = p.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            let name = rest.split('/').next().unwrap_or("").to_string();
            let entry = Entry {
                path: format!("{prefix}{name}"),
                is_dir: rest.contains('/') || version.is_none(),
                name,
            };
            match entries.iter_mut().find(|e| e.name == entry.name) {
                Some(slot) => *slot = entry,
                None => entries.push(entry),
            }
        }
        entries.sort_by_key(|e| e.name.to_lowercase());
        Ok(entries)
    }
}
