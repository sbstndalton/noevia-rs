#!/usr/bin/env node
'use strict';
// Recorded cowork.db boot schema (track R12) for crates/boot-schema.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules (npm ci)> \
//     node crates/boot-schema/tests/record_schema.cjs > crates/boot-schema/tests/fixtures/node-boot-schema.v1.json
//   ... node crates/boot-schema/tests/record_schema.cjs --check crates/boot-schema/tests/fixtures/node-boot-schema.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code: the boot steps index.cjs runs on cowork.db, in its order, with
// its arguments: auth.cjs createAuth (with rust-auth.cjs's guard, which also runs device-auth.cjs ensureDeviceSchema
// and app-passwords.cjs), tool-policy.cjs createToolPolicy, dav-settings.cjs createDavSettings, directory-mcp.cjs
// createDirectoryMcp, rust-mcp-credentials.cjs ensureSchema, chatgpt-oauth.cjs createChatGptOAuth and
// diary-connectors.cjs ensureTable, on a fresh data directory or one seeded with an older schema (QA rows only).
// Recorded: sqlite_master, the journal mode, the schema_migrations versions, the settings (a random instance id or
// setup-code hash recorded by its shape), the rows the boot backfills, and the first-run setup-code file's shape.
// Each source file's sha256 is recorded. stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const logged = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => logged.push(a.map(String).join(' '));
const READ_FILES = ['server/index.cjs', 'server/auth.cjs', 'server/rust-auth.cjs', 'server/device-auth.cjs', 'server/app-passwords.cjs', 'server/tool-policy.cjs',
  'server/dav-settings.cjs', 'server/directory-mcp.cjs', 'server/rust-mcp-credentials.cjs', 'server/chatgpt-oauth.cjs', 'server/diary-connectors.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
const req = (f) => require(path.join(root, 'server', f));
const Database = require(path.join(root, 'server/node_modules/better-sqlite3'));

// index.cjs's boot of cowork.db, in its order (each line asserted below).
const INDEX_LINES = [
  "const rustAuth = require('./rust-auth.cjs').createRustAuthGuard();",
  "const toolPolicy = require('./tool-policy.cjs').createToolPolicy({ db: authService.db });",
  "require('./dav-settings.cjs').createDavSettings({ auth: authService });",
  "const directoryMcp = require('./directory-mcp.cjs').createDirectoryMcp({ db: authService.db, secrets: secretStore });",
  "require('./rust-mcp-credentials.cjs').ensureSchema(authService.db);",
  "require('./diary-connectors.cjs').ensureTable(authService.db);",
];
const indexText = fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8');
for (const line of INDEX_LINES) if (!indexText.split('\n').includes(line)) throw new Error(`index.cjs no longer has: ${line}`);
const order = INDEX_LINES.slice(1).map((l) => indexText.indexOf(l));
if (order.some((at, i) => i > 0 && at < order[i - 1])) throw new Error('index.cjs boot order changed');

function boot(dataDir, env) {
  const authService = req('auth.cjs').createAuth({
    rustAuth: req('rust-auth.cjs').createRustAuthGuard(), dataDir,
    publicOrigin: env.PUBLIC_ORIGIN || '', rpId: env.WEBAUTHN_RP_ID || '', legacyToken: '', legacyCompat: false, secrets: null, trustProxy: false, additionalOrigins: [],
  });
  req('tool-policy.cjs').createToolPolicy({ db: authService.db });
  req('dav-settings.cjs').createDavSettings({ auth: authService });
  req('directory-mcp.cjs').createDirectoryMcp({ db: authService.db, secrets: null });
  req('rust-mcp-credentials.cjs').ensureSchema(authService.db);
  req('chatgpt-oauth.cjs').createChatGptOAuth({ db: authService.db, broker: {} });
  req('diary-connectors.cjs').ensureTable(authService.db);
  authService.db.close();
}

const A = '11111111-1212-4111-8111-111111111111';
const M = '22222222-1212-4222-8222-222222222222';
const users = `INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,disabled_at,created_at,updated_at)
  VALUES('${A}','qa-admin','qa-admin','QA admin','admin','x','wa-qa-1',NULL,1,1),('${M}','qa-member','qa-member','QA member','member','x','wa-qa-2',NULL,1,1);`;
// The tables as an early build made them (before the guarded column adds and the later tables).
const EARLY = `
  CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
  CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
  CREATE TABLE users(
      id TEXT PRIMARY KEY, username TEXT NOT NULL, username_norm TEXT NOT NULL UNIQUE,
      display_name TEXT NOT NULL, role TEXT NOT NULL CHECK(role IN ('admin','member')),
      password_hash TEXT NOT NULL, webauthn_user_id TEXT NOT NULL UNIQUE,
      disabled_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
    );
  CREATE TABLE passkeys(
      id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
      name TEXT NOT NULL, public_key BLOB NOT NULL, webauthn_user_id TEXT NOT NULL,
      counter INTEGER NOT NULL, device_type TEXT NOT NULL, backed_up INTEGER NOT NULL,
      transports TEXT NOT NULL, created_at INTEGER NOT NULL, last_used_at INTEGER
    );
  CREATE TABLE storage_connections(
      user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
      kind TEXT NOT NULL, base_url TEXT NOT NULL DEFAULT '', username TEXT NOT NULL DEFAULT '',
      secret TEXT NOT NULL DEFAULT '', corpus_root TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL
    );
  CREATE TABLE user_features(
      user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
      diary_enabled INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL
    );
  CREATE TABLE device_tokens(
      token_hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL,
      kind TEXT NOT NULL CHECK(kind IN ('access','refresh')), created_at INTEGER NOT NULL,
      expires_at INTEGER NOT NULL, used_at INTEGER
    );
  CREATE TABLE directory_mcp_servers(id TEXT PRIMARY KEY, registry_name TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL, url TEXT NOT NULL, added_by TEXT, added_at INTEGER NOT NULL);
  ${users}
  INSERT INTO user_features VALUES('${A}',0,1);
  INSERT INTO passkeys VALUES('pk-qa-1','${A}','QA key',x'00','wa-qa-1',0,'singleDevice',0,'[]',1,NULL);
  INSERT INTO storage_connections(user_id,kind,updated_at) VALUES('${A}','webdav',1);
  INSERT INTO directory_mcp_servers VALUES('qa-dir','qa/registry','QA dir','https://mcp.qa.test',NULL,1);
`;

const cases = [
  ['fresh', {}, null, false],
  ['fresh-origin', { PUBLIC_ORIGIN: 'https://app.qa.test', WEBAUTHN_RP_ID: 'qa.test' }, null, false],
  ['early-v1', {}, `${EARLY} INSERT INTO schema_migrations VALUES(1,1);`, false],
  ['early-v2', {}, `${EARLY} INSERT INTO schema_migrations VALUES(1,1),(2,1);`, false],
  ['early-rp-from-setting', { PUBLIC_ORIGIN: 'https://app.qa.test' }, `${EARLY} INSERT INTO settings VALUES('passkey_rp_id','old.qa.test');`, false],
  ['early-rp-from-env', { PUBLIC_ORIGIN: 'https://app.qa.test', WEBAUTHN_RP_ID: 'qa.test' }, EARLY, false],
  ['early-rp-id-not-matching', { PUBLIC_ORIGIN: 'https://app.qa.test', WEBAUTHN_RP_ID: 'other.test' }, EARLY, false],
  ['early-rp-from-admin-setting', { PUBLIC_ORIGIN: 'https://app.qa.test' }, `${EARLY} INSERT INTO settings VALUES('public_origin_admin','https://new.qa.test:8443');`, false],
  ['early-rp-from-setup-setting', {}, `${EARLY} INSERT INTO settings VALUES('public_origin','http://[::1]:3000');`, false],
  ['early-no-users', {}, EARLY.replace(users, '').replace(/INSERT INTO (user_features|passkeys|storage_connections)[^;]*;/g, ''), false],
  ['setup-file-kept', {}, null, true],
  ['instance-id-kept', {}, `${EARLY} INSERT INTO settings VALUES('instance_id','qa-instance');`, false],
  ['instance-id-empty', {}, `${EARLY} INSERT INTO settings VALUES('instance_id','');`, false],
  ['partial-v4', {}, `${EARLY.replace("kind TEXT NOT NULL, base_url", "kind TEXT NOT NULL, base_url").replace("updated_at INTEGER NOT NULL\n    );\n  CREATE TABLE user_features", "updated_at INTEGER NOT NULL, region TEXT NOT NULL DEFAULT 'eu-qa-1'\n    );\n  CREATE TABLE user_features")} INSERT INTO schema_migrations VALUES(1,1),(2,1),(3,1);`, false],
  ['directory-partial', {}, `${EARLY.replace('added_by TEXT, added_at INTEGER NOT NULL);', 'added_by TEXT, added_at INTEGER NOT NULL, headers_enc TEXT);')}`, false],
];

function dump(dataDir) {
  const db = new Database(path.join(dataDir, 'cowork.db'), { readonly: true });
  const master = db.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name").all();
  const journal = db.pragma('journal_mode', { simple: true });
  const versions = db.prepare('SELECT version FROM schema_migrations ORDER BY version').all().map((r) => r.version);
  const settings = db.prepare('SELECT key, value FROM settings ORDER BY key').all().map(({ key, value }) => {
    if (key === 'instance_id' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value)) return [key, '<uuid>'];
    if (key === 'setup_code_hash' && /^[0-9a-f]{64}$/.test(value)) return [key, '<sha256>'];
    return [key, value];
  });
  const features = db.prepare('SELECT * FROM user_features ORDER BY user_id').all().map((r) => ({ ...r, updated_at: r.updated_at > 1e12 ? '<now>' : r.updated_at }));
  const passkeys = db.prepare('SELECT id, rp_id FROM passkeys ORDER BY id').all();
  const storage = db.prepare('SELECT * FROM storage_connections ORDER BY user_id').all();
  const directory = db.prepare('SELECT * FROM directory_mcp_servers ORDER BY id').all();
  const hash = db.prepare("SELECT value FROM settings WHERE key='setup_code_hash'").get()?.value || null;
  db.close();
  const file = path.join(dataDir, 'first-run-setup-code');
  let setupFile = null;
  if (fs.existsSync(file)) {
    const text = fs.readFileSync(file, 'utf8');
    const code = text.replace(/\n$/, '');
    setupFile = {
      shape: /^[A-Za-z0-9_-]{22}\n$/.test(text) ? 'base64url-16-bytes' : text === 'qa-kept\n' ? 'kept' : 'other',
      mode: (fs.statSync(file).mode & 0o777).toString(8),
      hashMatches: hash === crypto.createHash('sha256').update(code).digest('hex'),
    };
  }
  return { master, journal, versions, settings, features, passkeys, storage, directory, setupFile };
}

const results = [];
for (const [name, env, seed, keepSetupFile] of cases) {
  const dataDir = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-qa-schema-')), 'ui-data');
  fs.mkdirSync(dataDir, { mode: 0o700 });
  if (seed) {
    const db = new Database(path.join(dataDir, 'cowork.db'));
    db.exec(seed);
    db.close();
  }
  if (keepSetupFile) fs.writeFileSync(path.join(dataDir, 'first-run-setup-code'), 'qa-kept\n', { mode: 0o600 });
  logged.length = 0;
  boot(dataDir, env);
  const first = dump(dataDir);
  // A second start changes nothing.
  boot(dataDir, env);
  const second = dump(dataDir);
  results.push({ name, env, seed, keepSetupFile, expected: first, idempotent: JSON.stringify(first) === JSON.stringify(second), warned: logged.filter((l) => /FIRST-RUN SETUP CODE/.test(l)).length });
  fs.rmSync(path.dirname(dataDir), { recursive: true, force: true });
}

const fixture = JSON.stringify({ generator: 'crates/boot-schema/tests/record_schema.cjs', sources: Object.fromEntries(READ_FILES.map((f) => [f, sha(f)])), cases: results }, (k, v) => (Buffer.isBuffer(v) ? v.toString('hex') : v), 1) + '\n';
const check = process.argv.indexOf('--check');
if (check !== -1) {
  if (fs.readFileSync(process.argv[check + 1], 'utf8') !== fixture) { process.stderr.write('boot schema fixture differs from a fresh recording\n'); process.exit(1); }
  process.stderr.write(`boot schema fixture matches (${results.length} cases)\n`);
} else process.stdout.write(fixture);
