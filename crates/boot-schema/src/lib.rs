//! cowork.db's boot schema (track R12): what core's start does to `UI_DATA_DIR/cowork.db` before it
//! serves anything, for the front to do once Node no longer starts first (the removal release calls
//! `noevia-server --ensure-schema`; nothing calls it before).
//!
//! In core's order (index.cjs): auth.cjs `createAuth` (the data directory, WAL and foreign keys, the
//! account tables and the v1/v2 migrations with v2's backfill; v3 `onboarded`; v4 `region`, `bucket`; v5
//! `insights_badge`, `insights_seen_at`; device-auth.cjs's tables and `replaced_by`;
//! `users.credential_epoch`; `passkeys.rp_id` and its backfill; the `instance_id`; the first-run setup
//! code for a database with no account; app-passwords.cjs's table), then tool-policy.cjs, dav-settings.cjs,
//! directory-mcp.cjs (with its three guarded column adds), rust-mcp-credentials.cjs, chatgpt-oauth.cjs and
//! diary-connectors.cjs. Every statement is core's own text ([`sql`]): SQLite keeps it in sqlite_master.
//! A second run changes nothing. Replayed on recorded Node boots (`tests/node_schema.rs`).

pub mod sql;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;
use zeroize::Zeroizing;

/// The deployment values core's start reads.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// `PUBLIC_ORIGIN` ('' when unset).
    pub public_origin: String,
    /// `WEBAUTHN_RP_ID` ('' when unset).
    pub rp_id: String,
}

impl Context {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        Context {
            public_origin: get("PUBLIC_ORIGIN").unwrap_or_default(),
            rp_id: get("WEBAUTHN_RP_ID").unwrap_or_default(),
        }
    }
}

/// What a boot did that its caller says aloud.
#[derive(Debug, Default)]
pub struct Booted {
    /// A new first-run setup code (core logs `FIRST-RUN SETUP CODE: <code>` and the file's path).
    pub setup_code: Option<Zeroizing<String>>,
}

#[derive(Debug)]
pub enum Error {
    /// The data directory could not be made, or the setup-code file could not be written.
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    /// An address core's `new URL()` refuses (core's start throws then).
    Origin,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "cowork.db boot: {e}"),
            Error::Sqlite(e) => write!(f, "cowork.db boot: {e}"),
            Error::Origin => write!(f, "cowork.db boot: the public address is not a URL"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// `db.prepare('PRAGMA table_info(<table>)').all().some((c) => c.name === <column>)`.
fn has_column(db: &Connection, table: &str, column: &str) -> Result<bool, Error> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES(<v>, unixepoch() * 1000)`.
fn version(db: &Connection, v: i64) -> Result<(), Error> {
    db.execute(
        &format!(
            "INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES({v}, unixepoch() * 1000)"
        ),
        [],
    )?;
    Ok(())
}

/// auth.cjs `setting(key)`: the value, `None` for no row; `||` also skips ''.
fn setting(db: &Connection, key: &str) -> Result<Option<String>, Error> {
    Ok(db
        .query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .filter(|v| !v.is_empty()))
}

/// auth.cjs `rpFor(origin)`: the address's host name, or `rpId` while the host is it or below it.
fn rp_for(origin: &str, rp_id: &str) -> Result<String, Error> {
    let host = if origin.is_empty() {
        "localhost".to_string()
    } else {
        url::Url::parse(origin)
            .map_err(|_| Error::Origin)?
            .host_str()
            .unwrap_or("")
            .to_string()
    };
    if !rp_id.is_empty() && (host == rp_id || host.ends_with(&format!(".{rp_id}"))) {
        Ok(rp_id.to_string())
    } else {
        Ok(host)
    }
}

/// `crypto.randomUUID()` from 16 random bytes.
fn uuid_v4(bytes: [u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, b) in bytes.iter().enumerate() {
        let b = match i {
            6 => (b & 0x0f) | 0x40,
            8 => (b & 0x3f) | 0x80,
            _ => *b,
        };
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `Buffer#toString('base64url')`.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            let index = ((n >> (18 - 6 * i)) & 63) as usize;
            if let Some(c) = ALPHABET.get(index) {
                out.push(char::from(*c));
            }
        }
    }
    out
}

/// The whole boot, in core's order. `random` fills buffers from the caller's CSPRNG.
pub fn ensure(
    data_dir: &Path,
    ctx: &Context,
    random: &mut dyn FnMut(&mut [u8]),
) -> Result<Booted, Error> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(data_dir)?;
    let db = Connection::open(data_dir.join("cowork.db"))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.execute_batch(sql::AUTH)?;
    // v3
    if !has_column(&db, "user_features", "onboarded")? {
        db.execute_batch(sql::V3)?;
    }
    version(&db, 3)?;
    // v4
    if !has_column(&db, "storage_connections", "region")? {
        db.execute_batch(
            "ALTER TABLE storage_connections ADD COLUMN region TEXT NOT NULL DEFAULT 'us-east-1'",
        )?;
    }
    if !has_column(&db, "storage_connections", "bucket")? {
        db.execute_batch(
            "ALTER TABLE storage_connections ADD COLUMN bucket TEXT NOT NULL DEFAULT ''",
        )?;
    }
    version(&db, 4)?;
    // v5
    if !has_column(&db, "user_features", "insights_badge")? {
        db.execute_batch(
            "ALTER TABLE user_features ADD COLUMN insights_badge INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !has_column(&db, "user_features", "insights_seen_at")? {
        db.execute_batch("ALTER TABLE user_features ADD COLUMN insights_seen_at INTEGER")?;
    }
    version(&db, 5)?;
    // device-auth.cjs ensureDeviceSchema
    db.execute_batch(sql::DEVICE)?;
    if !has_column(&db, "device_tokens", "replaced_by")? {
        db.execute_batch("ALTER TABLE device_tokens ADD COLUMN replaced_by TEXT")?;
    }
    if !has_column(&db, "users", "credential_epoch")? {
        db.execute_batch(
            "ALTER TABLE users ADD COLUMN credential_epoch INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    // The relying party a passkey made before rp_id was recorded belongs to.
    let origin = setting(&db, "public_origin_admin")?
        .or_else(|| Some(ctx.public_origin.clone()).filter(|o| !o.is_empty()))
        .map_or_else(|| setting(&db, "public_origin"), |o| Ok(Some(o)))?
        .unwrap_or_default();
    let relying_party = rp_for(&origin, &ctx.rp_id)?;
    if !has_column(&db, "passkeys", "rp_id")? {
        db.execute_batch("ALTER TABLE passkeys ADD COLUMN rp_id TEXT")?;
    }
    let backfill = setting(&db, "passkey_rp_id")?.unwrap_or(relying_party);
    db.execute(
        "UPDATE passkeys SET rp_id=? WHERE rp_id IS NULL",
        [backfill],
    )?;
    db.execute("DELETE FROM settings WHERE key='passkey_rp_id'", [])?;
    if setting(&db, "instance_id")?.is_none() {
        let mut b = [0u8; 16];
        random(&mut b);
        db.execute(
            "INSERT OR IGNORE INTO settings(key,value) VALUES('instance_id',?)",
            [uuid_v4(b)],
        )?;
    }
    // The first-run setup code, for a database with no account yet.
    let mut booted = Booted::default();
    let setup_file = data_dir.join("first-run-setup-code");
    let users: i64 = db.query_row("SELECT count(*) AS n FROM users", [], |r| r.get(0))?;
    if users == 0 && !setup_file.exists() {
        let mut b = Zeroizing::new([0u8; 16]);
        random(b.as_mut_slice());
        let code = Zeroizing::new(base64url(b.as_slice()));
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            // create_new: never over an existing path, a link included (core's flag 'wx').
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&setup_file)?;
            file.write_all(format!("{}\n", code.as_str()).as_bytes())?;
            file.sync_all()?;
        }
        db.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('setup_code_hash',?)",
            [format!("{:x}", Sha256::digest(code.as_bytes()))],
        )?;
        booted.setup_code = Some(code);
    }
    db.execute_batch(sql::APP_PASSWORDS)?;
    db.execute_batch(sql::TOOL_POLICIES)?;
    db.execute_batch(sql::DAV_SETTINGS)?;
    db.execute_batch(sql::DIRECTORY_MCP)?;
    let directory_column = |name: &str| -> Result<bool, Error> {
        Ok(db
            .query_row(
                "SELECT 1 FROM pragma_table_info('directory_mcp_servers') WHERE name=?",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };
    if !directory_column("headers_enc")? {
        db.execute_batch("ALTER TABLE directory_mcp_servers ADD COLUMN headers_enc TEXT")?;
    }
    if !directory_column("oauth")? {
        db.execute_batch(
            "ALTER TABLE directory_mcp_servers ADD COLUMN oauth INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !directory_column("personal")? {
        db.execute_batch("ALTER TABLE directory_mcp_servers ADD COLUMN personal INTEGER NOT NULL DEFAULT 0; ALTER TABLE directory_mcp_servers ADD COLUMN declared_json TEXT NOT NULL DEFAULT '[]'")?;
    }
    db.execute_batch(sql::MCP_CREDENTIALS)?;
    db.execute_batch(sql::CHATGPT_OAUTH)?;
    db.execute_batch(sql::DIARY_CONNECTORS)?;
    Ok(booted)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_have_node_shapes() {
        assert_eq!(base64url(&[0xfb; 16]), "-_v7-_v7-_v7-_v7-_v7-w");
        assert_eq!(base64url(&[0u8; 16]).len(), 22);
        let id = uuid_v4([0xff; 16]);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(rp_for("", "").unwrap(), "localhost");
        assert_eq!(rp_for("https://app.qa.test", "qa.test").unwrap(), "qa.test");
        assert_eq!(
            rp_for("https://app.qa.test", "other.test").unwrap(),
            "app.qa.test"
        );
        assert_eq!(rp_for("http://[::1]:3000", "").unwrap(), "[::1]");
        assert!(rp_for("not a url", "").is_err());
    }
}
