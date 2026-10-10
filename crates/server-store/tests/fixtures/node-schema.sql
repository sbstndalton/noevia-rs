-- The cowork.db schema a fresh core auth.cjs createAuth leaves (with device-auth.cjs
-- ensureDeviceSchema and app-passwords.cjs), for tests only. Synthetic. server-auth's
-- differential test checks these tables and columns against the schema Node itself creates.
PRAGMA journal_mode = WAL;
CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE users(
  id TEXT PRIMARY KEY, username TEXT NOT NULL, username_norm TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('admin','member')),
  password_hash TEXT NOT NULL, webauthn_user_id TEXT NOT NULL UNIQUE,
  disabled_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  credential_epoch INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE sessions(
  id_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  csrf_hash TEXT NOT NULL, created_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL, user_agent TEXT NOT NULL, ip TEXT NOT NULL
);
CREATE TABLE passkeys(
  id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL, public_key BLOB NOT NULL, webauthn_user_id TEXT NOT NULL,
  counter INTEGER NOT NULL, device_type TEXT NOT NULL, backed_up INTEGER NOT NULL,
  transports TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER, rp_id TEXT
);
CREATE TABLE challenges(
  id_hash TEXT PRIMARY KEY, user_id TEXT, kind TEXT NOT NULL, challenge TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX challenges_expires_at_idx ON challenges(expires_at);
CREATE TABLE invitations(
  token_hash TEXT PRIMARY KEY, created_by TEXT NOT NULL REFERENCES users(id),
  role TEXT NOT NULL, expires_at INTEGER NOT NULL, used_at INTEGER, created_at INTEGER NOT NULL
);
CREATE TABLE recoveries(
  token_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_by TEXT NOT NULL REFERENCES users(id), expires_at INTEGER NOT NULL,
  used_at INTEGER, created_at INTEGER NOT NULL
);
CREATE TABLE audit_events(
  id INTEGER PRIMARY KEY AUTOINCREMENT, actor_user_id TEXT, target_user_id TEXT,
  action TEXT NOT NULL, detail TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL
);
CREATE TABLE user_features(
  user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  diary_enabled INTEGER NOT NULL DEFAULT 0, onboarded INTEGER NOT NULL DEFAULT 1, updated_at INTEGER NOT NULL,
  insights_badge INTEGER NOT NULL DEFAULT 0, insights_seen_at INTEGER
);
CREATE TABLE storage_connections(
  user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  kind TEXT NOT NULL, base_url TEXT NOT NULL DEFAULT '', bucket TEXT NOT NULL DEFAULT '',
  username TEXT NOT NULL DEFAULT '',
  secret TEXT NOT NULL DEFAULT '', corpus_root TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL,
  region TEXT NOT NULL DEFAULT 'us-east-1'
);
CREATE TABLE user_appearance(
  user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  value TEXT NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE device_authorizations(
  device_code_hash TEXT PRIMARY KEY, user_code_hash TEXT NOT NULL UNIQUE,
  client_name TEXT NOT NULL, ip TEXT NOT NULL, user_agent TEXT NOT NULL,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, interval_ms INTEGER NOT NULL,
  last_poll_at INTEGER, status TEXT NOT NULL CHECK(status IN ('pending','approved','denied')),
  user_id TEXT REFERENCES users(id) ON DELETE CASCADE, decided_at INTEGER
);
CREATE INDEX device_authorizations_expires_idx ON device_authorizations(expires_at);
CREATE TABLE device_grants(
  id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  client_name TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL, ip TEXT NOT NULL, user_agent TEXT NOT NULL
);
CREATE TABLE device_tokens(
  token_hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL REFERENCES device_grants(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK(kind IN ('access','refresh')), created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL, used_at INTEGER, replaced_by TEXT
);
CREATE INDEX device_grants_user_idx ON device_grants(user_id);
CREATE INDEX device_tokens_grant_idx ON device_tokens(grant_id);
CREATE TABLE app_passwords(
  id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL, scope TEXT NOT NULL CHECK(scope IN ('lan','public')),
  password_hash TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER
);
CREATE INDEX app_passwords_user ON app_passwords(user_id);
INSERT INTO schema_migrations(version, applied_at) VALUES(1,0),(2,0),(3,0),(4,0),(5,0);
