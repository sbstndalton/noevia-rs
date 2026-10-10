//! Read access to noevia-core's `UI_DATA_DIR/cowork.db` for the Rust front (full-Rust migration
//! M2, docs/adr-0001-rust-and-repo-split.md "Amendment 2026-10-10" in sbstndalton/noevia).
//!
//! Node (core `server/auth.cjs` createAuth and the modules it wires) owns the schema and is its
//! only migrator. This crate:
//!
//! - opens the existing database **read-only** (`SQLITE_OPEN_READ_ONLY`, never `CREATE`), with
//!   `PRAGMA query_only` and an SQLite authorizer that refuses every write, DDL, `ATTACH` and
//!   pragma outside a short read list, so no raw SQL can write through it;
//! - uses Node's busy timeout (better-sqlite3's default `timeout`, 5000 ms; Node sets no other)
//!   and requires the WAL journal Node sets (`journal_mode = WAL`);
//! - refuses a database whose `schema_migrations` is newer than [`KNOWN_SCHEMA_VERSION`] (Node
//!   moved on without this build), older than it (Node has not migrated yet), or that lacks a
//!   column Rust reads ([`REQUIRED_COLUMNS`]: some of Node's columns arrive without a version
//!   bump, e.g. `users.credential_epoch`). The check reruns whenever SQLite's schema cookie
//!   changes, so a Node upgrade under a running front fails closed instead of misreading;
//! - writes only what the compiled [`OWNED_TABLES`] list grants, through [`Writer`], whose
//!   authorizer denies every other write and all DDL. M3 (Rust owns sign-in and the account):
//!   the list is the auth tables, each with the statements Rust may run on it ([`Access`]), and
//!   `settings` rows only for the keys in [`OWNED_SETTING_KEYS`], through
//!   [`WriteTx::set_setting`] and friends. A [`Writer`] opens only with the deployment switch
//!   ([`RustAuth`], `NOEVIA_RUST_AUTH=1`); without it Rust writes nothing in `cowork.db`, as in
//!   M2. Node refuses the same writes while the switch is on (core `server/rust-auth.cjs`), so
//!   each owned row has one writer.
//!
//! [`json`] has Node's `atomicJson` (core `server/workspace.cjs`) write-then-rename, gated the
//! same way by [`OWNED_JSON_FILES`] and the switch.

pub mod json;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, Row, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The database file inside UI_DATA_DIR (core auth.cjs).
pub const DB_FILE: &str = "cowork.db";

/// better-sqlite3's default `timeout` (Node passes none to `new Database`, so this applies);
/// core secrets-rotate.cjs sets the same 5000 ms explicitly.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// The highest `schema_migrations.version` core auth.cjs writes (v5: insights badge columns).
/// A newer database was migrated by a Node this build does not know: refused.
pub const KNOWN_SCHEMA_VERSION: i64 = 5;

/// The statements Rust may run on an owned table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// INSERT, UPDATE and DELETE.
    Full,
    /// INSERT and UPDATE (UPSERT included); rows are never deleted by Rust (an account is
    /// deleted by Node, whose cleanup spans its own stores, and the rows go with it by cascade).
    InsertUpdate,
    /// INSERT only. `audit_events` is shared: every Node module appends too, nobody rewrites.
    Append,
    /// DELETE only: an account recovery revokes the account's Diary connectors, a table Node
    /// otherwise owns.
    DeleteOnly,
}

impl Access {
    fn allows(self, action: &AuthAction<'_>) -> bool {
        matches!(
            (self, action),
            (Access::Full, _)
                | (
                    Access::InsertUpdate,
                    AuthAction::Insert { .. } | AuthAction::Update { .. }
                )
                | (Access::Append, AuthAction::Insert { .. })
                | (Access::DeleteOnly, AuthAction::Delete { .. })
        )
    }
}

/// What Rust writes once it owns sign-in and the account (M3, `NOEVIA_RUST_AUTH=1`): the tables
/// core auth.cjs, device-auth.cjs and app-passwords.cjs write from the routes Rust now serves.
/// Node's guard (core `server/rust-auth.cjs`) refuses the same set; the few Node writers that
/// stay are listed there (account deletion, Settings → Web address, the DAV listener's
/// `app_passwords.last_used_at`, revoking every device grant when the feature is switched off).
pub const OWNED_TABLES: &[(&str, Access)] = &[
    ("sessions", Access::Full),
    ("users", Access::InsertUpdate),
    ("user_features", Access::InsertUpdate),
    ("user_appearance", Access::InsertUpdate),
    ("passkeys", Access::Full),
    ("challenges", Access::Full),
    ("invitations", Access::Full),
    ("recoveries", Access::Full),
    ("device_authorizations", Access::Full),
    ("device_grants", Access::Full),
    ("device_tokens", Access::Full),
    ("app_passwords", Access::Full),
    ("audit_events", Access::Append),
    ("diary_connectors", Access::DeleteOnly),
];

/// `settings` keys Rust writes (only through [`WriteTx::set_setting`],
/// [`WriteTx::insert_setting_if_absent`] and [`WriteTx::delete_setting_if`]): the first-run
/// setup code's hash (consumed by setup; Node still writes it at boot, before any account
/// exists), the address setup chose (`public_origin`, and `public_origin_admin` when it differs
/// from PUBLIC_ORIGIN; Settings → Web address in Node writes that one too), and the passkey
/// decoy key.
pub const OWNED_SETTING_KEYS: &[&str] = &[
    "setup_code_hash",
    "public_origin",
    "public_origin_admin",
    "passkey_decoy_key",
];

/// JSON files (by file name, in a user's workspace directory) Rust may write with
/// [`json::write_owned`]: core account-instructions.cjs, account-memory.cjs and
/// account-preferences.cjs.
pub const OWNED_JSON_FILES: &[&str] = &[
    "account-instructions.json",
    "account-memory.json",
    "account-preferences.json",
];

/// The deployment switch for M3: `NOEVIA_RUST_AUTH=1` (exactly). Without it no [`Writer`] opens
/// and no owned JSON file is written, so merging M3 changes nothing until the switch is set.
/// Removed once Rust-owned sign-in is proven, together with Node's copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustAuth(());

impl RustAuth {
    /// The environment variable.
    pub const ENV: &'static str = "NOEVIA_RUST_AUTH";

    /// `Some` only for the exact value `1`.
    pub fn from_env_value(value: Option<&str>) -> Option<Self> {
        (value == Some("1")).then_some(RustAuth(()))
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        RustAuth(())
    }
}

/// The deployment switch for M4 (Rust owns project routes, slice by slice): `NOEVIA_RUST_PROJECTS=1`
/// (exactly). Without it Rust writes no project file ([`json::write_shared_projects`] and the
/// project assets need it). The front honours it only together with [`RustAuth`] (it gates the
/// routes it owns itself). Removed once Rust owns every projects.json writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustProjects(());

impl RustProjects {
    /// The environment variable.
    pub const ENV: &'static str = "NOEVIA_RUST_PROJECTS";

    /// `Some` only for the exact value `1`.
    pub fn from_env_value(value: Option<&str>) -> Option<Self> {
        (value == Some("1")).then_some(RustProjects(()))
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        RustProjects(())
    }
}

/// The per-user file M4 shares with Node (core `server/rust-projects.cjs`): written by both, each
/// only under the shared lock (crates/server-projects `lock`) and never from a stale copy.
pub const SHARED_PROJECT_FILES: &[&str] = &["projects.json"];

/// Columns Rust reads or writes, per table, with the core module that creates them. Each must
/// exist; Node may add others. (`diary_connectors` is optional: diary-connectors.cjs creates it
/// when the server wires it up, and Rust only deletes from it when it is there.)
pub const REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    ("schema_migrations", &["version"]),
    ("settings", &["key", "value"]),
    (
        "users",
        &[
            "id",
            "username",
            "username_norm",
            "display_name",
            "role",
            "password_hash",
            "webauthn_user_id",
            "disabled_at",
            "created_at",
            "updated_at",
            "credential_epoch",
        ],
    ),
    (
        "sessions",
        &[
            "id_hash",
            "user_id",
            "csrf_hash",
            "created_at",
            "last_seen_at",
            "expires_at",
            "user_agent",
            "ip",
        ],
    ),
    (
        "passkeys",
        &[
            "id",
            "user_id",
            "name",
            "public_key",
            "webauthn_user_id",
            "counter",
            "device_type",
            "backed_up",
            "transports",
            "created_at",
            "last_used_at",
            "rp_id",
        ],
    ),
    (
        "challenges",
        &["id_hash", "user_id", "kind", "challenge", "expires_at"],
    ),
    (
        "invitations",
        &[
            "token_hash",
            "created_by",
            "role",
            "expires_at",
            "used_at",
            "created_at",
        ],
    ),
    (
        "recoveries",
        &[
            "token_hash",
            "user_id",
            "created_by",
            "expires_at",
            "used_at",
            "created_at",
        ],
    ),
    (
        "audit_events",
        &[
            "id",
            "actor_user_id",
            "target_user_id",
            "action",
            "detail",
            "created_at",
        ],
    ),
    (
        "user_features",
        &["user_id", "diary_enabled", "onboarded", "updated_at"],
    ),
    ("user_appearance", &["user_id", "value", "updated_at"]),
    // device-auth.cjs ensureDeviceSchema, called from createAuth.
    (
        "device_authorizations",
        &[
            "device_code_hash",
            "user_code_hash",
            "client_name",
            "ip",
            "user_agent",
            "created_at",
            "expires_at",
            "interval_ms",
            "last_poll_at",
            "status",
            "user_id",
            "decided_at",
        ],
    ),
    (
        "device_grants",
        &[
            "id",
            "user_id",
            "client_name",
            "created_at",
            "last_used_at",
            "expires_at",
            "ip",
            "user_agent",
        ],
    ),
    (
        "device_tokens",
        &[
            "token_hash",
            "grant_id",
            "kind",
            "created_at",
            "expires_at",
            "used_at",
            "replaced_by",
        ],
    ),
    // app-passwords.cjs createAppPasswords, called from createAuth.
    (
        "app_passwords",
        &[
            "id",
            "user_id",
            "name",
            "scope",
            "password_hash",
            "created_at",
            "last_used_at",
        ],
    ),
];

/// Pragmas a read-only connection may run (reads only: no value).
const READ_PRAGMAS: &[&str] = &["journal_mode", "schema_version", "query_only"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// No `cowork.db` yet (Node has not started). Retry later.
    Missing,
    /// The database is not one Node has finished migrating: not WAL, migrations below
    /// [`KNOWN_SCHEMA_VERSION`], or a required column is missing. Retry later.
    NotReady(String),
    /// Node migrated past what this build knows. Never read: an update of noevia-rs is needed.
    SchemaTooNew { found: i64, known: i64 },
    /// A write [`OWNED_TABLES`] does not grant, or any write without the switch.
    NotOwned(String),
    /// An SQLite error (the message never contains bound values).
    Sqlite(String),
    /// A previous holder of the connection panicked.
    Poisoned,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Missing => write!(f, "cowork.db does not exist yet"),
            StoreError::NotReady(why) => write!(f, "cowork.db is not ready: {why}"),
            StoreError::SchemaTooNew { found, known } => write!(
                f,
                "cowork.db schema version {found} is newer than this build knows ({known}); update noevia-rs"
            ),
            StoreError::NotOwned(t) => write!(f, "Rust does not own {t}"),
            StoreError::Sqlite(e) => write!(f, "sqlite: {e}"),
            StoreError::Poisoned => write!(f, "store lock poisoned"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        // rusqlite's Display carries SQLite's message, never parameter values.
        StoreError::Sqlite(e.to_string())
    }
}

/// The read authorizer: reads, selects, functions, transactions, CTEs and [`READ_PRAGMAS`]
/// without a value. Everything else (every write, DDL, ATTACH, savepoints) is denied.
fn read_only_authorizer(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Read { .. }
        | AuthAction::Select
        | AuthAction::Function { .. }
        | AuthAction::Recursive
        | AuthAction::Transaction { .. } => Authorization::Allow,
        AuthAction::Pragma {
            pragma_name,
            pragma_value: None,
        } if READ_PRAGMAS.contains(&pragma_name) => Authorization::Allow,
        // table_info's value is the table it describes, not a setting.
        AuthAction::Pragma {
            pragma_name: "table_info",
            ..
        } => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

/// The writer's authorizer: the reader's set plus what `owned` grants, on the main database only;
/// `settings` only while `settings_open` is set (inside the key-checked setting helpers). DDL
/// stays denied: Rust never writes schema while Node is the migrator.
///
/// SQLite consults the authorizer when a statement is prepared (and re-prepared after a schema
/// change), including the programs it compiles for foreign-key actions, so a cascade can only
/// reach tables this grants too.
fn owned_authorizer(
    owned: &'static [(&'static str, Access)],
    settings_open: Arc<AtomicBool>,
) -> impl FnMut(AuthContext<'_>) -> Authorization {
    move |ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. } => {
            if ctx.database_name != Some("main") {
                return Authorization::Deny;
            }
            if table_name == "settings" {
                let open = settings_open.load(Ordering::SeqCst)
                    && matches!(
                        ctx.action,
                        AuthAction::Insert { .. } | AuthAction::Delete { .. }
                    );
                return if open {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                };
            }
            match owned.iter().find(|(t, _)| *t == table_name) {
                Some((_, access)) if access.allows(&ctx.action) => Authorization::Allow,
                _ => Authorization::Deny,
            }
        }
        _ => read_only_authorizer(ctx),
    }
}

fn schema_cookie(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("PRAGMA schema_version", [], |r| r.get(0))?)
}

/// The startup/upgrade check, see the crate docs.
fn verify(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::NotReady(format!(
            "journal mode is {mode}, Node sets WAL"
        )));
    }
    let has_migrations: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations')",
        [],
        |r| r.get(0),
    )?;
    if !has_migrations {
        return Err(StoreError::NotReady("no schema_migrations table".into()));
    }
    let found: Option<i64> =
        conn.query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })?;
    let found = found.unwrap_or(0);
    if found > KNOWN_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found,
            known: KNOWN_SCHEMA_VERSION,
        });
    }
    if found < KNOWN_SCHEMA_VERSION {
        return Err(StoreError::NotReady(format!(
            "schema version {found}, expected {KNOWN_SCHEMA_VERSION}"
        )));
    }
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    for (table, columns) in REQUIRED_COLUMNS {
        let have: Vec<String> = stmt
            .query_map([table], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        if have.is_empty() {
            return Err(StoreError::NotReady(format!("no {table} table")));
        }
        if let Some(missing) = columns.iter().find(|c| !have.iter().any(|h| h == *c)) {
            return Err(StoreError::NotReady(format!(
                "{table}.{missing} is missing"
            )));
        }
    }
    Ok(())
}

struct Inner {
    conn: Connection,
    /// The schema cookie [`verify`] last passed at.
    verified: i64,
}

impl Inner {
    fn ensure_current(&mut self) -> Result<(), StoreError> {
        // A data-only migration (core v2 is one) inserts its version without touching the schema
        // cookie, so the version is read every time (an indexed max, cheap); columns are rechecked
        // when the cookie moves.
        let found: Option<i64> =
            self.conn
                .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
                    r.get(0)
                })?;
        let found = found.unwrap_or(0);
        if found > KNOWN_SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found,
                known: KNOWN_SCHEMA_VERSION,
            });
        }
        let cookie = schema_cookie(&self.conn)?;
        if cookie != self.verified || found != KNOWN_SCHEMA_VERSION {
            verify(&self.conn)?;
            self.verified = cookie;
        }
        Ok(())
    }
}

fn open_inner(
    data_dir: &Path,
    flags: OpenFlags,
    query_only: bool,
    authorizer: impl FnMut(AuthContext<'_>) -> Authorization + Send + 'static,
) -> Result<Inner, StoreError> {
    let path = db_path(data_dir);
    if !path.is_file() {
        return Err(StoreError::Missing);
    }
    let conn = Connection::open_with_flags(&path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if query_only {
        // A second line of defence after the read-only open; set before the authorizer, which
        // refuses every pragma with a value.
        conn.pragma_update(None, "query_only", true)?;
    } else {
        // core auth.cjs: `db.pragma('foreign_keys = ON')` on every connection that writes, so
        // deleting a grant deletes its tokens and the REFERENCES checks hold for Rust's rows too.
        conn.pragma_update(None, "foreign_keys", true)?;
    }
    conn.authorizer(Some(authorizer))?;
    verify(&conn)?;
    let verified = schema_cookie(&conn)?;
    Ok(Inner { conn, verified })
}

/// `UI_DATA_DIR/cowork.db`.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE)
}

/// A read-only connection to `cowork.db`. Every [`Store::read`] runs in one deferred read
/// transaction, so it sees one consistent snapshot even while Node writes (WAL).
pub struct Store {
    inner: Mutex<Inner>,
}

impl Store {
    /// Opens the existing database read-only and checks it (see the crate docs). Never creates
    /// the file, its directory, or anything in it.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        let inner = open_inner(
            data_dir,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            true,
            read_only_authorizer,
        )?;
        Ok(Store {
            inner: Mutex::new(inner),
        })
    }

    /// Runs `f` in one read transaction. Fails closed with [`StoreError::SchemaTooNew`] /
    /// [`StoreError::NotReady`] if Node changed the schema since the last check.
    pub fn read<T>(
        &self,
        f: impl FnOnce(&Reader<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut inner = self.inner.lock().map_err(|_| StoreError::Poisoned)?;
        inner.ensure_current()?;
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let out = f(&Reader { conn: &tx })?;
        tx.finish()?;
        Ok(out)
    }
}

/// Queries inside [`Store::read`] or [`Writer::write`]. Any SQL may be passed; the connection's
/// authorizer refuses every statement that would write outside [`OWNED_TABLES`].
pub struct Reader<'a> {
    conn: &'a Connection,
}

impl Reader<'_> {
    /// The first row, or `None`.
    pub fn row<T, P: Params>(
        &self,
        sql: &str,
        params: P,
        map: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Option<T>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        Ok(stmt.query_row(params, map).optional()?)
    }

    /// Every row.
    pub fn rows<T, P: Params>(
        &self,
        sql: &str,
        params: P,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let out = stmt
            .query_map(params, map)?
            .collect::<Result<Vec<T>, _>>()?;
        Ok(out)
    }
}

/// A write transaction's handle: [`Reader`] plus `execute`, limited by the authorizer to the
/// writer's owned tables.
pub struct WriteTx<'a> {
    conn: &'a Connection,
    settings_open: &'a AtomicBool,
}

/// Opens the `settings` grant for one statement (prepared uncached, so no authorized statement
/// outlives it) and closes it again whatever happens.
struct SettingsWindow<'a>(&'a AtomicBool);

impl Drop for SettingsWindow<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl WriteTx<'_> {
    pub fn reader(&self) -> Reader<'_> {
        Reader { conn: self.conn }
    }

    fn settings_statement<P: Params>(
        &self,
        key: &str,
        sql: &str,
        params: P,
    ) -> Result<usize, StoreError> {
        if !OWNED_SETTING_KEYS.contains(&key) {
            return Err(StoreError::NotOwned(format!(
                "settings key {}",
                key.chars().take(64).collect::<String>()
            )));
        }
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        let mut stmt = self.conn.prepare(sql)?;
        Ok(stmt.execute(params)?)
    }

    /// `INSERT OR REPLACE INTO settings(key,value) VALUES(?,?)` for an owned key.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<usize, StoreError> {
        self.settings_statement(
            key,
            "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
            [key, value],
        )
    }

    /// `INSERT OR IGNORE INTO settings(key,value) VALUES(?,?)` for an owned key.
    pub fn insert_setting_if_absent(&self, key: &str, value: &str) -> Result<usize, StoreError> {
        self.settings_statement(
            key,
            "INSERT OR IGNORE INTO settings(key,value) VALUES(?1,?2)",
            [key, value],
        )
    }

    /// `DELETE FROM settings WHERE key=? AND value=?` for an owned key: the number deleted.
    pub fn delete_setting_if(&self, key: &str, value: &str) -> Result<usize, StoreError> {
        self.settings_statement(
            key,
            "DELETE FROM settings WHERE key=?1 AND value=?2",
            [key, value],
        )
    }

    /// Runs one statement; a write outside the owned tables fails with [`StoreError::NotOwned`].
    pub fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize, StoreError> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::AuthorizationForStatementDenied =>
            {
                StoreError::NotOwned(sql_target(sql))
            }
            other => StoreError::from(other),
        })?;
        Ok(stmt.execute(params)?)
    }
}

/// The table a refused statement names, for the error only (best effort, never values).
fn sql_target(sql: &str) -> String {
    let words: Vec<&str> = sql.split_whitespace().take(4).collect();
    words.join(" ")
}

/// A read-write connection limited to what [`OWNED_TABLES`] grants. Opens only with the
/// deployment switch.
pub struct Writer {
    inner: Mutex<Inner>,
    settings_open: Arc<AtomicBool>,
}

impl Writer {
    pub fn open(data_dir: &Path, _switch: RustAuth) -> Result<Self, StoreError> {
        Self::open_owning(data_dir, OWNED_TABLES)
    }

    fn open_owning(
        data_dir: &Path,
        owned: &'static [(&'static str, Access)],
    ) -> Result<Self, StoreError> {
        if owned.is_empty() {
            return Err(StoreError::NotOwned(
                "any table (OWNED_TABLES is empty)".into(),
            ));
        }
        let settings_open = Arc::new(AtomicBool::new(false));
        // READ_WRITE without CREATE: the file must already exist (Node made it).
        let inner = open_inner(
            data_dir,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            false,
            owned_authorizer(owned, Arc::clone(&settings_open)),
        )?;
        Ok(Writer {
            inner: Mutex::new(inner),
            settings_open,
        })
    }

    /// Runs `f` in one IMMEDIATE transaction (Node's writer waits up to [`BUSY_TIMEOUT`] too).
    pub fn write<T>(
        &self,
        f: impl FnOnce(&WriteTx<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut inner = self.inner.lock().map_err(|_| StoreError::Poisoned)?;
        inner.ensure_current()?;
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&WriteTx {
            conn: &tx,
            settings_open: &self.settings_open,
        })?;
        tx.commit()?;
        Ok(out)
    }

    /// Runs `f` in one deferred read transaction on the writer's connection (reads see the
    /// writer's own commits at once).
    pub fn read<T>(
        &self,
        f: impl FnOnce(&Reader<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut inner = self.inner.lock().map_err(|_| StoreError::Poisoned)?;
        inner.ensure_current()?;
        let tx = inner
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let out = f(&Reader { conn: &tx })?;
        tx.finish()?;
        Ok(out)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests;
pub use rusqlite;
