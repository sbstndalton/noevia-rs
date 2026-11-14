//! core's boot DDL for cowork.db, extracted verbatim (SQLite keeps each CREATE statement's own text in
//! sqlite_master, so the text must be Node's byte for byte). Generated from noevia-core server/ at the
//! sha256s below; tests/node_schema.rs checks the result against a recording of Node's boot.
//! - auth.cjs: eb4db4aff4e83872c3ed3b2e75d2193db785da5420238ae03ef3df88326c48a0
//! - device-auth.cjs: ed7f06de2e4a3a239512bf2e8b8bec108dc2b92550f8867b4f97f1eca1395dbe
//! - app-passwords.cjs: 3cf8bbed29e87bce6748704ba94728697e596931ce179992e96259a77ca419db
//! - tool-policy.cjs: 6fa6cb43759a57808deaae8d262a3021a4b8475c545a0bf6762174afa6b8d1dd
//! - dav-settings.cjs: 38ad884b35f023cc5a00eb49b0753005b390d6424ad0c0832764d8ac7599486f
//! - directory-mcp.cjs: fa19572a914599a1768aade9fc8436052cbebfb60d9890a538738a480bc7d57f
//! - rust-mcp-credentials.cjs: dd2fddece2734791db2739999623f2182fb05e48b7a555362df48bace74d3b1d
//! - chatgpt-oauth.cjs: ade208fa3e3b6af3ee45a9a3ad59b63e7c0b37862de3c1e337b7eac0c3838497
//! - diary-connectors.cjs: d8af94b9823173aefec91ee47f2dcf639041aa811cdf96b991f534554fed8e62

/// Verbatim from core server/auth.cjs createAuth (the first db.exec).
pub const AUTH: &str = r#"
    CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS users(
      id TEXT PRIMARY KEY, username TEXT NOT NULL, username_norm TEXT NOT NULL UNIQUE,
      display_name TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('admin','member')),
      password_hash TEXT NOT NULL, webauthn_user_id TEXT NOT NULL UNIQUE,
      disabled_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sessions(
      id_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      csrf_hash TEXT NOT NULL, created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
      expires_at INTEGER NOT NULL, user_agent TEXT NOT NULL, ip TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS passkeys(
      id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      name TEXT NOT NULL, public_key BLOB NOT NULL, webauthn_user_id TEXT NOT NULL,
      counter INTEGER NOT NULL, device_type TEXT NOT NULL, backed_up INTEGER NOT NULL,
      transports TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS challenges(
      id_hash TEXT PRIMARY KEY, user_id TEXT, kind TEXT NOT NULL, challenge TEXT NOT NULL,
      expires_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS challenges_expires_at_idx ON challenges(expires_at);
    CREATE TABLE IF NOT EXISTS invitations(
      token_hash TEXT PRIMARY KEY, created_by TEXT NOT NULL REFERENCES users(id),
      role TEXT NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER, created_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS recoveries(
      token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      created_by TEXT NOT NULL REFERENCES users(id), expires_at INTEGER NOT NULL,
      used_at INTEGER, created_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS audit_events(
      id INTEGER PRIMARY KEY AUTOINCREMENT, actor_user_id TEXT, target_user_id TEXT,
      action TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS storage_connections(
      user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
      kind TEXT NOT NULL, base_url TEXT NOT NULL DEFAULT '', bucket TEXT NOT NULL DEFAULT '',
      username TEXT NOT NULL DEFAULT '',
      secret TEXT NOT NULL DEFAULT '', corpus_root TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS user_appearance(
      user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
      value TEXT NOT NULL, updated_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS user_features(
      user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
      diary_enabled INTEGER NOT NULL DEFAULT 0, onboarded INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL
    );
    INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES(1, unixepoch() * 1000);
    INSERT OR IGNORE INTO user_features(user_id, diary_enabled, updated_at)
      SELECT id, 1, unixepoch() * 1000 FROM users
      WHERE NOT EXISTS (SELECT 1 FROM schema_migrations WHERE version=2);
    INSERT OR IGNORE INTO schema_migrations(version, applied_at) VALUES(2, unixepoch() * 1000);
  "#;

/// Verbatim from core server/auth.cjs (v3).
pub const V3: &str = r#"ALTER TABLE user_features ADD COLUMN onboarded INTEGER NOT NULL DEFAULT 1;
      UPDATE user_features SET onboarded = 1;"#;

/// Verbatim from core server/device-auth.cjs ensureDeviceSchema.
pub const DEVICE: &str = r#"
    CREATE TABLE IF NOT EXISTS device_authorizations(
      device_code_hash TEXT PRIMARY KEY, user_code_hash TEXT NOT NULL UNIQUE,
      client_name TEXT NOT NULL, ip TEXT NOT NULL, user_agent TEXT NOT NULL,
      created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, interval_ms INTEGER NOT NULL,
      last_poll_at INTEGER, status TEXT NOT NULL CHECK(status IN ('pending','approved','denied')),
      user_id TEXT REFERENCES users(id) ON DELETE CASCADE, decided_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS device_authorizations_expires_idx ON device_authorizations(expires_at);
    CREATE TABLE IF NOT EXISTS device_grants(
      id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      client_name TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL,
      expires_at INTEGER NOT NULL, ip TEXT NOT NULL, user_agent TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS device_grants_user_idx ON device_grants(user_id);
    CREATE TABLE IF NOT EXISTS device_tokens(
      token_hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL REFERENCES device_grants(id) ON DELETE CASCADE,
      kind TEXT NOT NULL CHECK(kind IN ('access','refresh')), created_at INTEGER NOT NULL,
      expires_at INTEGER NOT NULL, used_at INTEGER, replaced_by TEXT
    );
    CREATE INDEX IF NOT EXISTS device_tokens_grant_idx ON device_tokens(grant_id);
  "#;

/// Verbatim from core server/app-passwords.cjs createAppPasswords.
pub const APP_PASSWORDS: &str = r#"CREATE TABLE IF NOT EXISTS app_passwords(
    id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL, scope TEXT NOT NULL CHECK(scope IN ('lan','public')),
    password_hash TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER
  ); CREATE INDEX IF NOT EXISTS app_passwords_user ON app_passwords(user_id);"#;

/// Verbatim from core server/tool-policy.cjs createToolPolicy.
pub const TOOL_POLICIES: &str = r#"CREATE TABLE IF NOT EXISTS tool_policies(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    tool TEXT NOT NULL, mode TEXT NOT NULL CHECK(mode IN ('allow','ask','block')), updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, tool));"#;

/// Verbatim from core server/dav-settings.cjs createDavSettings.
pub const DAV_SETTINGS: &str = r#"CREATE TABLE IF NOT EXISTS dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    scope TEXT NOT NULL CHECK(scope IN ('off','lan','public')), acknowledged_at INTEGER, updated_at INTEGER NOT NULL);"#;

/// Verbatim from core server/directory-mcp.cjs migrate.
pub const DIRECTORY_MCP: &str = r#"CREATE TABLE IF NOT EXISTS directory_mcp_servers(id TEXT PRIMARY KEY, registry_name TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL, url TEXT NOT NULL, added_by TEXT, added_at INTEGER NOT NULL);"#;

/// Verbatim from core server/rust-mcp-credentials.cjs SCHEMA.
pub const MCP_CREDENTIALS: &str = r#"CREATE TABLE IF NOT EXISTS mcp_oauth_clients(server_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL);
  CREATE TABLE IF NOT EXISTS mcp_oauth_tokens(user_id TEXT NOT NULL, server_id TEXT NOT NULL, data_enc TEXT NOT NULL, updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, server_id));
  CREATE TABLE IF NOT EXISTS directory_mcp_user_keys(user_id TEXT NOT NULL, server_id TEXT NOT NULL, headers_enc TEXT NOT NULL, updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, server_id));"#;

/// Verbatim from core server/chatgpt-oauth.cjs createChatGptOAuth.
pub const CHATGPT_OAUTH: &str = r#"CREATE TABLE IF NOT EXISTS chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'connected', updated_at INTEGER NOT NULL)"#;

/// Verbatim from core server/diary-connectors.cjs ensureTable.
pub const DIARY_CONNECTORS: &str = r#"CREATE TABLE IF NOT EXISTS diary_connectors(id TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,name TEXT NOT NULL,token_hash TEXT NOT NULL,created_at INTEGER NOT NULL)"#;
