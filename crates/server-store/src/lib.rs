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
//!   [`WriteTx::set_setting`] and friends. A [`Writer`] opens with the ownership token
//!   ([`RustAuth`], permanent since sbstndalton/noevia#1071). Node always refuses the same writes
//!   (core `server/rust-auth.cjs`), so each owned row has one writer.
//!
//! [`json`] has Node's `atomicJson` (core `server/workspace.cjs`) write-then-rename, gated the
//! same way by [`OWNED_JSON_FILES`] and the ownership token.

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

/// What Rust writes once it owns sign-in and the account (M3, permanent since #1071): the tables
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
    // core dav-settings.cjs (Settings -> Sharing, #1071): created by core at boot; the DAV
    // listener only reads it.
    ("dav_settings", Access::InsertUpdate),
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
    "previous_origins",
    "passkey_decoy_key",
];

/// JSON files (by file name, in a user's workspace directory) Rust may write with
/// [`json::write_owned`]: core account-instructions.cjs, account-memory.cjs and
/// account-preferences.cjs and the two per-user chat-framing preferences.
pub const OWNED_JSON_FILES: &[&str] = &[
    "account-instructions.json",
    "account-memory.json",
    "account-preferences.json",
    "chat-framing.json",
    // core chat-vault-mirror.cjs `writePreferences` (sbstndalton/noevia#2190 round 3).
    "chat-vault-mirror.json",
];

/// Rust's ownership of sign-in and the account (M3): the tables and files Rust writes. It was the
/// deployment switch `NOEVIA_RUST_AUTH=1`; since sbstndalton/noevia#1071 it is permanent (Node's
/// copies of the routes are gone and Node always refuses these writes), so the token is simply
/// [`RustAuth::owned`]. It still marks every write path that needs the ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustAuth(());

impl RustAuth {
    /// The ownership token: Rust always owns sign-in and the account.
    pub const fn owned() -> Self {
        RustAuth(())
    }

    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        RustAuth(())
    }
}

/// The complete native feature-setting owner: typed catalog keys and one exact process fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustFeatureSettings(());
impl RustFeatureSettings {
    pub const fn owned() -> Self {
        Self(())
    }
}
/// Internal process-incarnation metadata, never a product flag or public description.
pub const FEATURE_RUNTIME_INCARNATION_KEY: &str = "native:feature-runtime:incarnation";

/// Rust's ownership of the project routes it serves (M4: a project's images, its chat list, the
/// free-chat list) and of projects.json and free-chats.json, which Rust alone writes (Node saves
/// through the project store, sbstndalton/noevia#1264). It was the deployment switch
/// `NOEVIA_RUST_PROJECTS=1`; since sbstndalton/noevia#1071 it is permanent (Node's copies of the
/// routes are gone and Node always refuses them), so the token is simply [`RustProjects::owned`].
/// It still marks every path that writes a shared project file ([`json::write_shared_projects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustProjects(());

impl RustProjects {
    /// The ownership token: Rust always owns the project routes it serves.
    pub const fn owned() -> Self {
        RustProjects(())
    }
}

/// M6 sampling-settings capability. Auth alone never grants this settings key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustSamplingSettings(());
impl RustSamplingSettings {
    pub const ENV: &'static str = "NOEVIA_RUST_SAMPLING_SETTINGS";
    pub fn from_env_value(value: Option<&str>, _auth: RustAuth) -> Option<Self> {
        (value == Some("1")).then_some(Self(()))
    }
}

/// Capability for the single reasoning default key; native auth alone cannot write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustReasoningSettings(());
impl RustReasoningSettings {
    pub const ENV: &'static str = "NOEVIA_RUST_REASONING_SETTINGS";
    pub fn from_env_value(value: Option<&str>, _auth: RustAuth) -> Option<Self> {
        (value == Some("1")).then_some(Self(()))
    }
}

/// Settings -> Models -> Decision service (sbstndalton/noevia#2190): the one `decision:configuration`
/// row, which the Rust front alone writes (core rust-decision-settings.cjs refuses the routes and
/// core rust-auth.cjs the key). Permanent, no switch: the web supervisor pairs the front's
/// `rust-decision-settings` capability with that Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustDecisionSettings(());
impl RustDecisionSettings {
    pub const KEY: &'static str = "decision:configuration";
    pub const fn owned() -> Self {
        Self(())
    }
}

/// Settings -> Diary & storage, the Nextcloud sign-in and the storage test/folder routes
/// (sbstndalton/noevia#2190): a completed sign-in saves the account's `storage_connections` row
/// through [`WriteTx::save_storage_connection`], Node's exact upsert. Since round 4 the Settings save
/// (`PUT /api/integrations/storage`) and the legacy WebDAV setup are saved here too, so this is the
/// row's only writer: core's guard (rust-auth.cjs `FRONT_TABLES`) refuses every Node write of the
/// table but the same-plaintext compare-and-swap reseal. The table stays out of [`OWNED_TABLES`]
/// (whose grants are per statement kind); the grant is this one statement, never a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustStorageIntegrations(());
impl RustStorageIntegrations {
    pub const fn owned() -> Self {
        Self(())
    }
}

/// The table [`RustStorageIntegrations`] may upsert one row of.
const STORAGE_CONNECTIONS: &str = "storage_connections";

/// Sign in with ChatGPT (sbstndalton/noevia#2190 round 5): the Rust front is the token broker and
/// the only writer of `chatgpt_oauth_tokens` (the device sign-in's save, the single-flight refresh's
/// compare-and-swap, "reconnect needed", disconnect, account removal and the start sweep of orphans). Core's guard (rust-auth.cjs
/// `FRONT_TABLES`) refuses every Node write of the table but the rotation's same-plaintext reseal.
/// Like [`RustStorageIntegrations`] the grant is per statement: only the five [`WriteTx`] methods
/// named `*_chatgpt_tokens` open it, each for its one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustChatgptOAuth(());
impl RustChatgptOAuth {
    pub const fn owned() -> Self {
        Self(())
    }
}

/// The table [`RustChatgptOAuth`] writes.
const CHATGPT_OAUTH_TOKENS: &str = "chatgpt_oauth_tokens";

/// MCP server credentials (sbstndalton/noevia#2202): the Rust front is the broker of the OAuth
/// sign-ins (`mcp_oauth_tokens`, `mcp_oauth_clients`) and of each account's own API keys for a
/// directory server (`directory_mcp_user_keys`), and the only writer of those three tables. Core's
/// guard (rust-auth.cjs `FRONT_TABLES`) refuses every Node write of them, even inside `exempt()`.
/// Like [`RustChatgptOAuth`] the grant is per statement: only the [`WriteTx`] methods named
/// `mcp_*` open it, each for its one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustMcpCredentials(());
impl RustMcpCredentials {
    pub const fn owned() -> Self {
        Self(())
    }
}

/// Settings -> Connectors (sbstndalton/noevia#2190 round 7): the Rust front is the only writer of
/// `tool_policies`, the per-account allow/ask/block of each tool. Core's guard (rust-auth.cjs
/// `FRONT_TABLES`) refuses every Node write of the table, and core re-reads it per call. Like
/// [`RustChatgptOAuth`] the grant is per statement: only [`WriteTx::upsert_tool_policy`] opens it,
/// for core tool-policy.cjs's one upsert, never a delete (a deleted account's rows go by cascade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustToolPolicies(());
impl RustToolPolicies {
    pub const fn owned() -> Self {
        Self(())
    }
}

/// The tables [`RustMcpCredentials`] writes.
const MCP_TABLES: [&str; 3] = [
    "mcp_oauth_tokens",
    "mcp_oauth_clients",
    "directory_mcp_user_keys",
];
/// The table [`RustToolPolicies`] writes.
const TOOL_POLICIES: &str = "tool_policies";

/// Dedicated routing configuration capability; native auth alone grants neither its file nor key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustRoutingMode(());
impl RustRoutingMode {
    pub const ENV: &'static str = "NOEVIA_RUST_ROUTING_MODE";
    pub const FILE: &'static str = "routing-mode.json";
    pub fn from_env_value(value: Option<&str>, _auth: RustAuth) -> Option<Self> {
        (value == Some("1")).then_some(Self(()))
    }
}

/// The per-user files of M4 (core `server/rust-projects.cjs`): Rust is their only writer, under
/// each file's lock (crates/server-projects `lock`) and never from a stale copy; Node saves its
/// changes through the project store (sbstndalton/noevia#1264).
/// free-chats.json is the free-chat slice's (crates/server-projects `free_chats`).
///
/// `providers.json` (sbstndalton/noevia#2190 round 6) is the same kind of file: the account's private
/// provider rows, whose only writer is the Rust front (its provider routes, the ChatGPT broker's
/// row and Node's seal-on-load migration through the project store), under the file's lock.
pub const SHARED_PROJECT_FILES: &[&str] = &["projects.json", "free-chats.json", "providers.json"];

/// The shared provider rows (`UI_DATA_DIR/shared-providers.json`, administrators'), which the Rust
/// front alone writes since sbstndalton/noevia#2190 round 6.
pub const ROOT_PROVIDER_FILE: &str = "shared-providers.json";

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
        // table_info's and foreign_key_list's value is the table they describe, not a setting.
        AuthAction::Pragma {
            pragma_name: "table_info" | "foreign_key_list",
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
    storage_open: Arc<AtomicBool>,
    chatgpt_open: Arc<AtomicBool>,
    mcp_open: Arc<AtomicBool>,
    policy_open: Arc<AtomicBool>,
) -> impl FnMut(AuthContext<'_>) -> Authorization {
    move |ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. } => {
            if ctx.database_name != Some("main") {
                return Authorization::Deny;
            }
            // noevia#1378: inside a trigger body SQLite names the trigger as the accessor. Rust's
            // statements never rely on a trigger, so a trigger that would write (delete another
            // user's sessions, say) is denied, and the whole statement with it. A trigger that
            // only RAISEs still aborts as before. Foreign-key actions carry no accessor; the
            // known set of those is checked when the writer opens (verify_write_topology).
            if ctx.accessor.is_some() {
                return Authorization::Deny;
            }
            if table_name == "settings" {
                // Fixed-key grants authorize the caller statement, never persisted trigger/view SQL.
                if ctx.accessor.is_some() {
                    return Authorization::Deny;
                }
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
            if table_name == STORAGE_CONNECTIONS {
                // sbstndalton/noevia#2190: only Node's saveStorage upsert, inside
                // [`WriteTx::save_storage_connection`]; never a delete.
                let open = storage_open.load(Ordering::SeqCst)
                    && matches!(
                        ctx.action,
                        AuthAction::Insert { .. } | AuthAction::Update { .. }
                    );
                return if open {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                };
            }
            if table_name == CHATGPT_OAUTH_TOKENS {
                // sbstndalton/noevia#2190 round 5: only inside the `*_chatgpt_tokens` methods.
                return if chatgpt_open.load(Ordering::SeqCst) {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                };
            }
            if MCP_TABLES.contains(&table_name) {
                // sbstndalton/noevia#2202: only inside the `mcp_*` methods.
                return if mcp_open.load(Ordering::SeqCst) {
                    Authorization::Allow
                } else {
                    Authorization::Deny
                };
            }
            if table_name == TOOL_POLICIES {
                // sbstndalton/noevia#2190 round 7: only the upsert inside
                // [`WriteTx::upsert_tool_policy`]; never a delete.
                let open = policy_open.load(Ordering::SeqCst)
                    && matches!(
                        ctx.action,
                        AuthAction::Insert { .. } | AuthAction::Update { .. }
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

/// Every foreign key Node's schema declares (core `auth.cjs`, `app-passwords.cjs`,
/// `device-auth.cjs`, `dav-settings.cjs`, `diary-connectors.cjs`, `tool-policy.cjs`), as
/// (child table, column, parent table, parent column, ON UPDATE, ON DELETE). Checked against the
/// live database on 2026-10-14: exactly these fifteen, and no triggers.
pub const KNOWN_FOREIGN_KEYS: &[(&str, &str, &str, &str, &str, &str)] = &[
    (
        "app_passwords",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "dav_settings",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "device_authorizations",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "device_grants",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "device_tokens",
        "grant_id",
        "device_grants",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "diary_connectors",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "invitations",
        "created_by",
        "users",
        "id",
        "NO ACTION",
        "NO ACTION",
    ),
    ("passkeys", "user_id", "users", "id", "NO ACTION", "CASCADE"),
    (
        "recoveries",
        "created_by",
        "users",
        "id",
        "NO ACTION",
        "NO ACTION",
    ),
    (
        "recoveries",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    ("sessions", "user_id", "users", "id", "NO ACTION", "CASCADE"),
    (
        "storage_connections",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "tool_policies",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "user_appearance",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
    (
        "user_features",
        "user_id",
        "users",
        "id",
        "NO ACTION",
        "CASCADE",
    ),
];

/// At most this many tables are inspected; a database with more is refused for writing.
const TOPOLOGY_TABLE_CAP: usize = 512;

/// noevia#1378: what a [`Writer`]'s statement can set off beyond the row it names. The authorizer
/// already refuses a foreign-key action that reaches a table the writer does not own
/// (`a_cascade_reaches_only_tables_the_writer_owns`), but it grants a whole owned table, so an
/// action between owned tables (a self-referencing `sessions.parent_session ... ON DELETE
/// CASCADE`) or a trigger on one (deleting other sessions) would let one user's sign-out delete
/// another user's rows. Refused unless every foreign key with an action whose child is a table
/// this writer owns is one of [`KNOWN_FOREIGN_KEYS`]. Triggers are handled by
/// [`owned_authorizer`]: a write inside a trigger body is denied, so a trigger can abort a
/// statement (tests inject audit failures that way) but never write a row.
fn verify_write_topology(
    conn: &Connection,
    owned: &[(&'static str, Access)],
) -> Result<(), StoreError> {
    let written = |t: &str| {
        t == "settings"
            || t == STORAGE_CONNECTIONS
            || t == TOOL_POLICIES
            || owned.iter().any(|(o, _)| *o == t)
    };
    let mut tables = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY name LIMIT ?1",
    )?;
    let names: Vec<String> = tables
        .query_map([TOPOLOGY_TABLE_CAP as i64 + 1], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if names.len() > TOPOLOGY_TABLE_CAP {
        return Err(StoreError::NotReady(format!(
            "more than {TOPOLOGY_TABLE_CAP} tables; not writing"
        )));
    }
    let mut fks = conn.prepare(
        "SELECT \"from\", \"table\", coalesce(\"to\", ''), on_update, on_delete FROM pragma_foreign_key_list(?1)",
    )?;
    for child in &names {
        let rows: Vec<(String, String, String, String, String)> = fks
            .query_map([child], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<_, _>>()?;
        for (from, parent, to, on_update, on_delete) in rows {
            let inert =
                |a: &str| a.eq_ignore_ascii_case("NO ACTION") || a.eq_ignore_ascii_case("RESTRICT");
            if (inert(&on_update) && inert(&on_delete)) || !written(child) {
                continue;
            }
            let known = KNOWN_FOREIGN_KEYS.iter().any(|(c, f, p, t, u, d)| {
                c == child
                    && *f == from
                    && *p == parent
                    && *t == to
                    && *u == on_update
                    && *d == on_delete
            });
            if !known {
                return Err(StoreError::NotReady(format!(
                    "unexpected foreign key {child}.{from} -> {parent}({to}) ON UPDATE {on_update} ON DELETE {on_delete}; not writing"
                )));
            }
        }
    }
    Ok(())
}

/// The startup/upgrade check, see the crate docs. A writer's connection also gets
/// [`verify_write_topology`].
fn verify(
    conn: &Connection,
    owned: Option<&'static [(&'static str, Access)]>,
) -> Result<(), StoreError> {
    verify_schema(conn)?;
    if let Some(owned) = owned {
        verify_write_topology(conn, owned)?;
    }
    Ok(())
}

fn verify_schema(conn: &Connection) -> Result<(), StoreError> {
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
    /// A [`Writer`]'s grants: [`verify_write_topology`] is part of every check. None for a [`Store`].
    owned: Option<&'static [(&'static str, Access)]>,
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
            verify(&self.conn, self.owned)?;
            self.verified = cookie;
        }
        Ok(())
    }
}

fn open_inner(
    data_dir: &Path,
    flags: OpenFlags,
    owned: Option<&'static [(&'static str, Access)]>,
    authorizer: impl FnMut(AuthContext<'_>) -> Authorization + Send + 'static,
) -> Result<Inner, StoreError> {
    let path = db_path(data_dir);
    if !path.is_file() {
        return Err(StoreError::Missing);
    }
    let conn = Connection::open_with_flags(&path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if owned.is_none() {
        // A second line of defence after the read-only open; set before the authorizer, which
        // refuses every pragma with a value.
        conn.pragma_update(None, "query_only", true)?;
    } else {
        // core auth.cjs: `db.pragma('foreign_keys = ON')` on every connection that writes, so
        // deleting a grant deletes its tokens and the REFERENCES checks hold for Rust's rows too.
        conn.pragma_update(None, "foreign_keys", true)?;
    }
    conn.authorizer(Some(authorizer))?;
    verify(&conn, owned)?;
    let verified = schema_cookie(&conn)?;
    Ok(Inner {
        conn,
        verified,
        owned,
    })
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
            None,
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
    storage_open: &'a AtomicBool,
    chatgpt_open: &'a AtomicBool,
    mcp_open: &'a AtomicBool,
    policy_open: &'a AtomicBool,
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

    /// Only a canonical catalog setting, never a caller-selected settings key.
    pub fn set_feature_enabled(
        &self,
        _features: RustFeatureSettings,
        feature: server_features::Feature,
        enabled: bool,
    ) -> Result<usize, StoreError> {
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        Ok(self.conn.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
            [
                server_features::spec(feature).setting,
                if enabled { "true" } else { "false" },
            ],
        )?)
    }

    /// One native-only epoch key; no generic key, prefix or arbitrary value grant.
    pub fn set_feature_runtime_incarnation(
        &self,
        _features: RustFeatureSettings,
        incarnation: &str,
    ) -> Result<usize, StoreError> {
        if incarnation.len() != 32
            || !incarnation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !incarnation.bytes().any(|b| b != b'0')
        {
            return Err(StoreError::NotOwned(
                "feature runtime incarnation format".into(),
            ));
        }
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        Ok(self.conn.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('native:feature-runtime:incarnation',?1)",
            [incarnation],
        )?)
    }

    /// A distinct capability grants exactly one fixed statement/key; arbitrary SQL stays denied.
    pub fn set_sampling_enabled(
        &self,
        _sampling: RustSamplingSettings,
        enabled: bool,
    ) -> Result<usize, StoreError> {
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        let mut stmt = self.conn.prepare(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('auto_sampling_presets_enabled',?1)",
        )?;
        Ok(stmt.execute([if enabled { "true" } else { "false" }])?)
    }

    /// Sets only the reasoning default key under its dedicated native capability.
    pub fn set_reasoning_default(
        &self,
        _reasoning: RustReasoningSettings,
        value: &str,
    ) -> Result<usize, StoreError> {
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        Ok(self.conn.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('reasoning_effort_default',?1)",
            [value],
        )?)
    }

    /// Exactly Node's decision-settings save statement (core feature-setting-source.cjs WRITE_SQL
    /// without RETURNING), under its own capability.
    pub fn set_decision_configuration(
        &self,
        _decision: RustDecisionSettings,
        value: &str,
    ) -> Result<usize, StoreError> {
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        Ok(self.conn.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('decision:configuration',?1)",
            [value],
        )?)
    }

    /// Exactly Node's decision-settings reset statement, under the same capability.
    pub fn delete_decision_configuration(
        &self,
        _decision: RustDecisionSettings,
    ) -> Result<usize, StoreError> {
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        Ok(self.conn.execute(
            "DELETE FROM settings WHERE key='decision:configuration'",
            [],
        )?)
    }

    /// Exactly core auth.cjs `saveStorage`'s upsert (sbstndalton/noevia#2190: a Nextcloud sign-in
    /// that completed, saved by the Rust front), under its own capability; the values are already
    /// normalized and the secret sealed by the caller. The audit row is the caller's.
    #[allow(clippy::too_many_arguments)]
    pub fn save_storage_connection(
        &self,
        _storage: RustStorageIntegrations,
        user_id: &str,
        kind: &str,
        base_url: &str,
        bucket: &str,
        region: &str,
        username: &str,
        sealed_secret: &str,
        corpus_root: &str,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        self.storage_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.storage_open);
        let mut stmt = self.conn.prepare(
            "INSERT INTO storage_connections(user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at)
        VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(user_id) DO UPDATE SET kind=excluded.kind,base_url=excluded.base_url,
        bucket=excluded.bucket,region=excluded.region,username=excluded.username,secret=excluded.secret,corpus_root=excluded.corpus_root,updated_at=excluded.updated_at",
        )?;
        Ok(stmt.execute(rusqlite::params![
            user_id,
            kind,
            base_url,
            bucket,
            region,
            username,
            sealed_secret,
            corpus_root,
            updated_at
        ])?)
    }

    /// Exactly core tool-policy.cjs `set()`'s statement for one tool: the account's mode upsert.
    pub fn upsert_tool_policy(
        &self,
        _cap: RustToolPolicies,
        user_id: &str,
        tool: &str,
        mode: &str,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        self.policy_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.policy_open);
        let mut stmt = self.conn.prepare(
            "INSERT INTO tool_policies VALUES(?,?,?,?) ON CONFLICT(user_id, tool) DO UPDATE SET mode=excluded.mode, updated_at=excluded.updated_at",
        )?;
        Ok(stmt.execute(rusqlite::params![user_id, tool, mode, updated_at])?)
    }

    /// Opens the `chatgpt_oauth_tokens` grant for one uncached statement.
    fn chatgpt_statement<P: Params>(&self, sql: &str, params: P) -> Result<usize, StoreError> {
        self.chatgpt_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.chatgpt_open);
        let mut stmt = self.conn.prepare(sql)?;
        Ok(stmt.execute(params)?)
    }

    /// Exactly core chatgpt-oauth.cjs `write()`: the sign-in's upsert (the row's sealed token JSON).
    pub fn save_chatgpt_tokens(
        &self,
        _cap: RustChatgptOAuth,
        user_id: &str,
        data_enc: &str,
        state: &str,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        self.chatgpt_statement(
            "INSERT INTO chatgpt_oauth_tokens(user_id, data_enc, state, updated_at) VALUES(?,?,?,?)
      ON CONFLICT(user_id) DO UPDATE SET data_enc=excluded.data_enc, state=excluded.state, updated_at=excluded.updated_at",
            rusqlite::params![user_id, data_enc, state, updated_at],
        )
    }

    /// Exactly core's refresh compare-and-set: the new tokens replace the ciphertext the refresh
    /// started from, only while that row is still connected.
    pub fn swap_chatgpt_tokens(
        &self,
        _cap: RustChatgptOAuth,
        user_id: &str,
        next: &str,
        updated_at: i64,
        previous: &str,
    ) -> Result<usize, StoreError> {
        self.chatgpt_statement(
            "UPDATE chatgpt_oauth_tokens SET data_enc=?, updated_at=? WHERE user_id=? AND state='connected' AND data_enc=?",
            rusqlite::params![next, updated_at, user_id, previous],
        )
    }

    /// Exactly core's `markReconnect`: this stored sign-in (by its ciphertext) or, without one, the row.
    pub fn mark_chatgpt_reconnect(
        &self,
        _cap: RustChatgptOAuth,
        user_id: &str,
        raw: Option<&str>,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        match raw {
            Some(raw) => self.chatgpt_statement(
                "UPDATE chatgpt_oauth_tokens SET state='reconnect', updated_at=? WHERE user_id=? AND data_enc=?",
                rusqlite::params![updated_at, user_id, raw],
            ),
            None => self.chatgpt_statement(
                "UPDATE chatgpt_oauth_tokens SET state='reconnect', updated_at=? WHERE user_id=?",
                rusqlite::params![updated_at, user_id],
            ),
        }
    }

    /// Exactly core's `disconnect`/`forgetUser` delete.
    pub fn delete_chatgpt_tokens(
        &self,
        _cap: RustChatgptOAuth,
        user_id: &str,
    ) -> Result<usize, StoreError> {
        self.chatgpt_statement(
            "DELETE FROM chatgpt_oauth_tokens WHERE user_id=?",
            rusqlite::params![user_id],
        )
    }

    /// The broker-start sweep: the rows of accounts that no longer exist (core `forgetUser` failed
    /// after `deleteUser` removed the account, so its tokens were left behind).
    pub fn delete_orphan_chatgpt_tokens(
        &self,
        _cap: RustChatgptOAuth,
    ) -> Result<usize, StoreError> {
        self.chatgpt_statement(
            "DELETE FROM chatgpt_oauth_tokens WHERE user_id NOT IN (SELECT id FROM users)",
            [],
        )
    }

    /// Opens the MCP credential tables' grant for one uncached statement.
    fn mcp_statement<P: Params>(&self, sql: &str, params: P) -> Result<usize, StoreError> {
        self.mcp_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.mcp_open);
        let mut stmt = self.conn.prepare(sql)?;
        Ok(stmt.execute(params)?)
    }

    /// Exactly core mcp-oauth.cjs's token upsert (`finish`).
    pub fn mcp_save_token(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
        data_enc: &str,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "INSERT INTO mcp_oauth_tokens VALUES(?,?,?,?) ON CONFLICT(user_id, server_id) DO UPDATE SET data_enc=excluded.data_enc, updated_at=excluded.updated_at",
            rusqlite::params![user_id, server_id, data_enc, updated_at],
        )
    }

    /// The refresh's and the re-seal's compare-and-swap: `next` replaces `previous` only while
    /// the row still holds it.
    pub fn mcp_swap_token(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
        next: &str,
        updated_at: i64,
        previous: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "UPDATE mcp_oauth_tokens SET data_enc=?, updated_at=? WHERE user_id=? AND server_id=? AND data_enc=?",
            rusqlite::params![next, updated_at, user_id, server_id, previous],
        )
    }

    /// One account's sign-in to one server (core `disconnect`).
    pub fn mcp_delete_token(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM mcp_oauth_tokens WHERE user_id=? AND server_id=?",
            rusqlite::params![user_id, server_id],
        )
    }

    /// The disconnect's compare-and-delete: the sign-in goes only while it is still `previous`, so
    /// what the service is asked to revoke is what was removed.
    pub fn mcp_delete_token_if(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
        previous: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM mcp_oauth_tokens WHERE user_id=? AND server_id=? AND data_enc=?",
            rusqlite::params![user_id, server_id, previous],
        )
    }

    /// Every account's sign-in to one server (core `forget`, `setClient`).
    pub fn mcp_delete_tokens_of_server(
        &self,
        _cap: RustMcpCredentials,
        server_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM mcp_oauth_tokens WHERE server_id=?",
            rusqlite::params![server_id],
        )
    }

    /// Every sign-in of one account (core `forgetUser`).
    pub fn mcp_delete_tokens_of_user(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM mcp_oauth_tokens WHERE user_id=?",
            rusqlite::params![user_id],
        )
    }

    /// The registered app of one server (core `clientFor`, `setClient`).
    pub fn mcp_save_client(
        &self,
        _cap: RustMcpCredentials,
        server_id: &str,
        data_enc: &str,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "INSERT INTO mcp_oauth_clients VALUES(?,?,?) ON CONFLICT(server_id) DO UPDATE SET data_enc=excluded.data_enc, updated_at=excluded.updated_at",
            rusqlite::params![server_id, data_enc, updated_at],
        )
    }

    pub fn mcp_swap_client(
        &self,
        _cap: RustMcpCredentials,
        server_id: &str,
        next: &str,
        updated_at: i64,
        previous: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "UPDATE mcp_oauth_clients SET data_enc=?, updated_at=? WHERE server_id=? AND data_enc=?",
            rusqlite::params![next, updated_at, server_id, previous],
        )
    }

    /// The registration a service no longer knows (`invalid_client` on a refresh) goes only while it
    /// is still `previous`: a registration made since is not removed.
    pub fn mcp_delete_client_if(
        &self,
        _cap: RustMcpCredentials,
        server_id: &str,
        previous: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM mcp_oauth_clients WHERE server_id=? AND data_enc=?",
            rusqlite::params![server_id, previous],
        )
    }

    pub fn mcp_delete_client(
        &self,
        _cap: RustMcpCredentials,
        server_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM mcp_oauth_clients WHERE server_id=?",
            rusqlite::params![server_id],
        )
    }

    /// Exactly core directory-mcp.cjs `setUserKey`'s upsert.
    pub fn mcp_save_key(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
        headers_enc: &str,
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "INSERT INTO directory_mcp_user_keys VALUES(?,?,?,?) ON CONFLICT(user_id, server_id) DO UPDATE SET headers_enc=excluded.headers_enc, updated_at=excluded.updated_at",
            rusqlite::params![user_id, server_id, headers_enc, updated_at],
        )
    }

    pub fn mcp_swap_key(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
        next: &str,
        previous: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "UPDATE directory_mcp_user_keys SET headers_enc=? WHERE user_id=? AND server_id=? AND headers_enc=?",
            rusqlite::params![next, user_id, server_id, previous],
        )
    }

    pub fn mcp_delete_key(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
        server_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM directory_mcp_user_keys WHERE user_id=? AND server_id=?",
            rusqlite::params![user_id, server_id],
        )
    }

    pub fn mcp_delete_keys_of_server(
        &self,
        _cap: RustMcpCredentials,
        server_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM directory_mcp_user_keys WHERE server_id=?",
            rusqlite::params![server_id],
        )
    }

    pub fn mcp_delete_keys_of_user(
        &self,
        _cap: RustMcpCredentials,
        user_id: &str,
    ) -> Result<usize, StoreError> {
        self.mcp_statement(
            "DELETE FROM directory_mcp_user_keys WHERE user_id=?",
            rusqlite::params![user_id],
        )
    }

    /// The start sweep: sign-ins and keys of accounts that no longer exist (core's account delete
    /// removes the account first and asks the broker to forget after; a failed ask leaves them).
    pub fn mcp_delete_orphans_of_users(
        &self,
        _cap: RustMcpCredentials,
    ) -> Result<usize, StoreError> {
        let mut n = self.mcp_statement(
            "DELETE FROM mcp_oauth_tokens WHERE user_id NOT IN (SELECT id FROM users)",
            [],
        )?;
        n += self.mcp_statement(
            "DELETE FROM directory_mcp_user_keys WHERE user_id NOT IN (SELECT id FROM users)",
            [],
        )?;
        Ok(n)
    }

    /// The same for servers that were removed (only where `directory_mcp_servers` exists).
    pub fn mcp_delete_orphans_of_servers(
        &self,
        _cap: RustMcpCredentials,
    ) -> Result<usize, StoreError> {
        let mut n = self.mcp_statement(
            "DELETE FROM mcp_oauth_tokens WHERE server_id NOT IN (SELECT id FROM directory_mcp_servers)",
            [],
        )?;
        n += self.mcp_statement(
            "DELETE FROM mcp_oauth_clients WHERE server_id NOT IN (SELECT id FROM directory_mcp_servers)",
            [],
        )?;
        n += self.mcp_statement(
            "DELETE FROM directory_mcp_user_keys WHERE server_id NOT IN (SELECT id FROM directory_mcp_servers)",
            [],
        )?;
        Ok(n)
    }

    /// Exactly the routing allowed-list statement, under its own capability.
    pub fn set_routing_allowed(
        &self,
        _routing: RustRoutingMode,
        value: &str,
    ) -> Result<usize, StoreError> {
        self.settings_open.store(true, Ordering::SeqCst);
        let _close = SettingsWindow(self.settings_open);
        Ok(self.conn.execute(
            "INSERT OR REPLACE INTO settings(key,value) VALUES('routing_modes_allowed',?1)",
            [value],
        )?)
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
    storage_open: Arc<AtomicBool>,
    chatgpt_open: Arc<AtomicBool>,
    mcp_open: Arc<AtomicBool>,
    policy_open: Arc<AtomicBool>,
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
        let storage_open = Arc::new(AtomicBool::new(false));
        let chatgpt_open = Arc::new(AtomicBool::new(false));
        let mcp_open = Arc::new(AtomicBool::new(false));
        let policy_open = Arc::new(AtomicBool::new(false));
        // READ_WRITE without CREATE: the file must already exist (Node made it).
        let inner = open_inner(
            data_dir,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            Some(owned),
            owned_authorizer(
                owned,
                Arc::clone(&settings_open),
                Arc::clone(&storage_open),
                Arc::clone(&chatgpt_open),
                Arc::clone(&mcp_open),
                Arc::clone(&policy_open),
            ),
        )?;
        Ok(Writer {
            inner: Mutex::new(inner),
            settings_open,
            storage_open,
            chatgpt_open,
            mcp_open,
            policy_open,
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
            storage_open: &self.storage_open,
            chatgpt_open: &self.chatgpt_open,
            mcp_open: &self.mcp_open,
            policy_open: &self.policy_open,
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
