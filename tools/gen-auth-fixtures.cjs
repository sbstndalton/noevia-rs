#!/usr/bin/env node
'use strict';
// Differential fixtures for crates/server-auth (full-Rust migration M2).
//
// Loads noevia-core's own auth code (server/auth.cjs createAuth/parseCookies, device-auth.cjs
// createDeviceAuth/createRequestAuth/bearerToken/hasSessionCookie/browserOnly, app-passwords.cjs
// verifyDav, features.cjs createFeatures) against synthetic databases it seeds itself, evaluates
// a fixed table of synthetic requests plus a seeded fuzz set at fixed clocks, and prints every
// verdict as JSON. crates/server-auth/tests/differential.rs requires agreement with the committed
// output, and never a Rust accept where Node rejects.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules> \
//     node tools/gen-auth-fixtures.cjs > crates/server-auth/tests/fixtures/node-auth.v1.json
//
// Deterministic: tokens are SHA-256 of fixed labels, Argon2 hashes use fixed salts, the clock is
// fixed, and nothing random Node writes itself (instance_id, setup code, applied_at) is printed.
// Nothing here touches the network or a real data directory. Synthetic data only.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout (with server/node_modules installed).\n');
  process.exit(2);
}
const server = path.join(path.resolve(checkout), 'server');
const req = (m) => require(path.join(server, m));
const { createAuth, parseCookies, digest, createRateLimiter } = req('auth.cjs');
const deviceLib = req('device-auth.cjs');
const { createFeatures } = req('features.cjs');
const { hash, Algorithm } = require(require.resolve('@node-rs/argon2', { paths: [server] }));
const Database = require(require.resolve('better-sqlite3', { paths: [server] }));

let reference = null;
try { reference = execFileSync('git', ['-C', checkout, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(); } catch { reference = null; }

// The generator's console must stay clean: createAuth prints a first-run setup code for an empty
// database (synthetic, but never printed into the fixture or the log).
console.warn = () => {};
console.log = () => {};

const NOW = 1_800_000_000_000;
const IDLE = 7 * 24 * 60 * 60 * 1000;
const realNow = Date.now;
let clock = NOW;
Date.now = () => clock;

const tok = (label, prefix = '') => prefix + crypto.createHash('sha256').update(`noevia-m2-fixture:${label}`).digest('base64url');
const salt = (label) => crypto.createHash('sha256').update(`noevia-m2-salt:${label}`).digest().subarray(0, 16);
const argon = (password, label) => hash(password, { algorithm: Algorithm.Argon2id, memoryCost: 19456, timeCost: 2, parallelism: 1, salt: salt(label) });

// ---- seed -------------------------------------------------------------------------------------

const T = {
  sMember: tok('session-member'), sAdmin: tok('session-admin'), sExpired: tok('session-expired'),
  sExpiresNext: tok('session-expires-next'), sIdle: tok('session-idle'), sIdleEdge: tok('session-idle-edge'),
  sDisabled: tok('session-disabled'), sZeroDisabled: tok('session-zero-disabled'), sNoFeatures: tok('session-no-features'),
  sTextDisabled: tok('session-text-disabled'),
  cMember: tok('csrf-member'), cAdmin: tok('csrf-admin'), cOther: tok('csrf-other'),
  dAccess: tok('device-access', 'nva_'), dRefresh: tok('device-refresh', 'nvr_'), dAccessAsNva: tok('device-refresh-nva', 'nva_'),
  dExpired: tok('device-expired', 'nva_'), dGrantExpired: tok('device-grant-expired', 'nva_'), dDisabled: tok('device-disabled', 'nva_'),
  dAdmin: tok('device-admin', 'nva_'), dUnknown: tok('device-unknown', 'nva_'),
  legacy: 'legacy-token-synthetic-0001',
};

const APP = {
  lanId: 'a'.repeat(32), publicId: '0123456789abcdef0123456789abcdef', disabledId: 'b'.repeat(32),
};
APP.lan = `nv_dav_${APP.lanId}.${tok('app-lan').slice(0, 43)}`;
APP.pub = `nv_dav_${APP.publicId}.${tok('app-public').slice(0, 43)}`;
APP.disabled = `nv_dav_${APP.disabledId}.${tok('app-disabled').slice(0, 43)}`;

async function seed(db, { users = true } = {}) {
  if (!users) return;
  const userRow = db.prepare(`INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,disabled_at,created_at,updated_at,credential_epoch)
    VALUES(?,?,?,?,?,?,?,?,?,?,?)`);
  const pw = await argon('correct horse battery staple', 'user-password');
  userRow.run('u-admin-off', 'AdminOff', 'adminoff', 'Disabled first admin', 'admin', pw, 'w-0', NOW - 5000, 500, 500, 0);
  userRow.run('u-admin', 'Admin', 'admin', 'Synthetic Admin', 'admin', pw, 'w-1', null, 1000, 1000, 0);
  userRow.run('u-admin2', 'admin2', 'admin2', 'Second Admin', 'admin', pw, 'w-2', null, 2000, 2000, 3);
  userRow.run('u-member', 'Member.One', 'member.one', 'Synthetic Member', 'member', pw, 'w-3', null, 3000, 3000, 1);
  userRow.run('u-disabled', 'gone', 'gone', 'Disabled Member', 'member', pw, 'w-4', NOW - 10, 4000, 4000, 0);
  userRow.run('u-zero', 'zero', 'zero', 'Zero Disabled', 'member', pw, 'w-5', 0, 5000, 5000, 0);
  userRow.run('u-nofeat', 'nofeat', 'nofeat', 'No Features Row', 'member', pw, 'w-6', null, 6000, 6000, 0);
  userRow.run('u-textdis', 'textdis', 'textdis', 'Text Disabled', 'member', pw, 'w-7', '', 7000, 7000, 0);
  const feat = db.prepare('INSERT OR REPLACE INTO user_features(user_id,diary_enabled,onboarded,updated_at) VALUES(?,?,?,?)');
  feat.run('u-admin', 1, 1, 1); feat.run('u-admin2', 0, 0, 1); feat.run('u-member', 1, 0, 1); feat.run('u-disabled', 0, 1, 1);
  feat.run('u-zero', 0, 1, 1); feat.run('u-textdis', 1, 1, 1);
  db.prepare("DELETE FROM user_features WHERE user_id='u-nofeat'").run();
  const sess = db.prepare('INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?,?,?,?,?,?,?,?)');
  const ABS = 30 * 24 * 60 * 60 * 1000;
  sess.run(digest(T.sMember), 'u-member', digest(T.cMember), NOW - 1000, NOW - 1000, NOW - 1000 + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sAdmin), 'u-admin', digest(T.cAdmin), NOW - 2000, NOW - 60_000, NOW + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sExpired), 'u-member', digest(T.cMember), NOW - ABS, NOW - 10, NOW, 'ua', '127.0.0.1');
  sess.run(digest(T.sExpiresNext), 'u-member', digest(T.cMember), NOW - ABS, NOW - 10, NOW + 1, 'ua', '127.0.0.1');
  sess.run(digest(T.sIdle), 'u-member', digest(T.cMember), NOW - IDLE - 5, NOW - IDLE, NOW + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sIdleEdge), 'u-member', digest(T.cMember), NOW - IDLE - 5, NOW - IDLE + 1, NOW + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sDisabled), 'u-disabled', digest(T.cMember), NOW - 5, NOW - 5, NOW + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sZeroDisabled), 'u-zero', digest(T.cOther), NOW - 5, NOW - 5, NOW + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sNoFeatures), 'u-nofeat', digest(T.cOther), NOW - 5, NOW - 5, NOW + ABS, 'ua', '127.0.0.1');
  sess.run(digest(T.sTextDisabled), 'u-textdis', digest(T.cOther), NOW - 5, NOW - 5, NOW + ABS, 'ua', '127.0.0.1');
  const grant = db.prepare('INSERT INTO device_grants(id,user_id,client_name,created_at,last_used_at,expires_at,ip,user_agent) VALUES(?,?,?,?,?,?,?,?)');
  grant.run('g-member', 'u-member', 'Synthetic Mac', NOW - 100, NOW - 120_000, NOW + 86_400_000, 'ip', 'ua');
  grant.run('g-old', 'u-member', 'Old Mac', NOW - 100, NOW - 100, NOW, 'ip', 'ua');
  grant.run('g-disabled', 'u-disabled', 'Gone Mac', NOW - 100, NOW - 100, NOW + 86_400_000, 'ip', 'ua');
  grant.run('g-admin', 'u-admin', 'Admin Mac', NOW - 100, NOW - 100, NOW + 86_400_000, 'ip', 'ua');
  const dt = db.prepare('INSERT INTO device_tokens(token_hash,grant_id,kind,created_at,expires_at,used_at,replaced_by) VALUES(?,?,?,?,?,?,?)');
  dt.run(digest(T.dAccess), 'g-member', 'access', NOW - 100, NOW + 3_600_000, null, null);
  dt.run(digest(T.dRefresh), 'g-member', 'refresh', NOW - 100, NOW + 3_600_000, null, null);
  dt.run(digest(T.dAccessAsNva), 'g-member', 'refresh', NOW - 100, NOW + 3_600_000, null, null);
  dt.run(digest(T.dExpired), 'g-member', 'access', NOW - 100, NOW, null, null);
  dt.run(digest(T.dGrantExpired), 'g-old', 'access', NOW - 100, NOW + 3_600_000, null, null);
  dt.run(digest(T.dDisabled), 'g-disabled', 'access', NOW - 100, NOW + 3_600_000, null, null);
  dt.run(digest(T.dAdmin), 'g-admin', 'access', NOW - 100, NOW + 3_600_000, null, null);
  const ap = db.prepare('INSERT INTO app_passwords(id,user_id,name,scope,password_hash,created_at,last_used_at) VALUES(?,?,?,?,?,?,?)');
  ap.run(APP.lanId, 'u-member', 'Phone', 'lan', await argon(APP.lan, 'app-lan'), NOW - 50, null);
  ap.run(APP.publicId, 'u-admin', 'Laptop', 'public', await argon(APP.pub, 'app-public'), NOW - 50, null);
  ap.run(APP.disabledId, 'u-disabled', 'Old', 'lan', await argon(APP.disabled, 'app-disabled'), NOW - 50, null);
}

const SEEDED_TABLES = ['users', 'user_features', 'sessions', 'device_grants', 'device_tokens', 'app_passwords', 'settings'];
const SEEDED_SETTINGS = new Set(['public_origin', 'public_origin_admin', 'previous_origins', 'feature:nativeClientAuth']);

function dump(db) {
  const schema = db.prepare("SELECT type,name,tbl_name AS tbl,sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type DESC,name").all();
  const columns = {};
  for (const t of db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").all()) {
    columns[t.name] = db.prepare(`SELECT name FROM pragma_table_info('${t.name}') ORDER BY cid`).all().map((c) => c.name);
  }
  const rows = {};
  for (const t of SEEDED_TABLES) {
    let all = db.prepare(`SELECT * FROM ${t} ORDER BY rowid`).all();
    if (t === 'settings') all = all.filter((r) => SEEDED_SETTINGS.has(r.key));
    rows[t] = all;
  }
  const versions = db.prepare('SELECT version FROM schema_migrations ORDER BY version').all().map((r) => r.version);
  return { schema, columns, rows, migrations: versions };
}

// ---- requests ---------------------------------------------------------------------------------

function fakeReq({ cookie, authorization, origin, csrf, method = 'GET' }) {
  const headers = {};
  if (cookie !== undefined) headers.cookie = cookie;
  if (authorization !== undefined) headers.authorization = authorization;
  if (origin !== undefined) headers.origin = origin;
  if (csrf !== undefined) headers['x-csrf-token'] = csrf;
  return { headers, method, socket: { remoteAddress: '127.0.0.1' } };
}

const GATE_TARGETS = [
  ['GET', '/api/projects'], ['POST', '/api/projects'], ['DELETE', '/api/projects/p1'], ['OPTIONS', '/api/projects'],
  ['GET', '/api/admin/users'], ['GET', '/api/profile'], ['PUT', '/api/integrations/storage'], ['GET', '/api/integrations/storage'],
  ['POST', '/api/connectors/gdrive/link'], ['GET', '/api/connectors/gdrive'], ['GET', '/api/auth/passkeysx'], ['GET', '/api/auth/passkeys/1'],
];

function verdict(authn) {
  if (!authn) return null;
  return {
    user: authn.user,
    kind: authn.device ? 'device' : authn.legacy ? 'legacy' : 'session',
    accountRole: authn.accountRole ?? null,
    device: authn.device ?? null,
  };
}

/** index.cjs handleRequestScoped's gate for an /api/ path not in publicAuthRoutes. */
function gate(requestAuth, authService, r, authn, method, p) {
  if (!authn) return 'unauthorized';
  if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && (!authService.originValid(r) || !requestAuth.csrfValid(r, authn))) return 'csrf';
  if (requestAuth.browserOnly(authn, p, method)) return 'browser_only';
  return 'allow';
}

function evaluate(ctx, input, { gates = true } = {}) {
  const { db, authService, requestAuth } = ctx;
  db.exec('SAVEPOINT fixture_case');
  try {
    clock = input.now ?? NOW;
    const r = fakeReq(input);
    const authn = requestAuth.authenticate(r);
    const out = { authn: verdict(authn) };
    if (gates) {
      out.csrf = authn ? !!requestAuth.csrfValid(r, authn) : false;
      out.origin = !!authService.originValid(r);
      out.gates = GATE_TARGETS.map(([m, p]) => gate(requestAuth, authService, { ...r, method: m }, authn, m, p));
    }
    return out;
  } finally {
    db.exec('ROLLBACK TO fixture_case; RELEASE fixture_case');
    clock = NOW;
  }
}

function credentialCases() {
  const c = [];
  const add = (name, input) => c.push({ name, input });
  add('nothing', {});
  for (const [k, v] of Object.entries(T)) {
    if (k.startsWith('s')) add(`cookie ${k}`, { cookie: `cowork_session=${v}` });
    if (k.startsWith('d')) add(`bearer ${k}`, { authorization: `Bearer ${v}` });
  }
  const s = T.sMember;
  add('cookie among others', { cookie: `theme=dark; cowork_session=${s}; other=1` });
  add('cookie name padded', { cookie: `  cowork_session  =${s}` });
  add('cookie value leading space', { cookie: `cowork_session= ${s}` });
  add('cookie value trailing space', { cookie: `cowork_session=${s} ` });
  add('cookie empty', { cookie: 'cowork_session=' });
  add('cookie no equals', { cookie: 'cowork_session' });
  add('cookie last wins valid', { cookie: `cowork_session=bogus; cowork_session=${s}` });
  add('cookie last wins invalid', { cookie: `cowork_session=${s}; cowork_session=bogus` });
  add('cookie percent encoded', { cookie: `cowork_session=${encodeURIComponent(s).replace(/-/g, '%2D').replace(/_/g, '%5f')}` });
  add('cookie bad percent', { cookie: `cowork_session=${s}%` });
  add('cookie bad percent utf8', { cookie: `cowork_session=${s}%C3` });
  add('cookie case', { cookie: `Cowork_Session=${s}` });
  add('cookie quoted', { cookie: `cowork_session="${s}"` });
  add('cookie semicolon only', { cookie: ';;;' });
  add('cookie proto', { cookie: `__proto__=${s}; cowork_session=${s}` });
  add('bearer lowercase scheme', { authorization: `bearer ${T.dAccess}` });
  add('bearer extra spaces', { authorization: `Bearer    ${T.dAccess}   ` });
  add('bearer tab', { authorization: `Bearer\t${T.dAccess}` });
  add('bearer nbsp', { authorization: `Bearer ${T.dAccess}` });
  add('bearer two words', { authorization: `Bearer ${T.dAccess} x` });
  add('bearer no scheme', { authorization: T.dAccess });
  add('bearer basic', { authorization: `Basic ${T.dAccess}` });
  add('bearer plus cookie', { authorization: `Bearer ${T.dAccess}`, cookie: `cowork_session=${s}` });
  add('bearer plus csrf cookie only', { authorization: `Bearer ${T.dAccess}`, cookie: 'cowork_csrf=x' });
  add('bearer plus bare cookie name', { authorization: `Bearer ${T.dAccess}`, cookie: 'cowork_session' });
  add('bearer plus other cookie', { authorization: `Bearer ${T.dAccess}`, cookie: 'theme=dark' });
  add('legacy bearer', { authorization: `Bearer ${T.legacy}` });
  add('legacy bare', { authorization: T.legacy });
  add('legacy lowercase', { authorization: `bEaReR   ${T.legacy}` });
  add('legacy trailing space', { authorization: `Bearer ${T.legacy} ` });
  add('legacy prefix', { authorization: `Bearer ${T.legacy.slice(0, -1)}` });
  add('legacy longer', { authorization: `Bearer ${T.legacy}0` });
  add('legacy double scheme', { authorization: `Bearer Bearer ${T.legacy}` });
  add('legacy nbsp', { authorization: `Bearer ${T.legacy}` });
  add('legacy with bad cookie', { authorization: `Bearer ${T.legacy}`, cookie: 'cowork_session=bogus' });
  add('legacy with good cookie', { authorization: `Bearer ${T.legacy}`, cookie: `cowork_session=${T.sMember}` });
  // CSRF and origin, on a member session and an admin session.
  const ck = (sess, csrf) => `cowork_session=${sess}; cowork_csrf=${csrf}`;
  add('csrf ok', { cookie: ck(s, T.cMember), csrf: T.cMember });
  add('csrf header missing', { cookie: ck(s, T.cMember) });
  add('csrf cookie missing', { cookie: `cowork_session=${s}`, csrf: T.cMember });
  add('csrf mismatch', { cookie: ck(s, T.cMember), csrf: T.cOther });
  add('csrf both other', { cookie: ck(s, T.cOther), csrf: T.cOther });
  add('csrf empty', { cookie: ck(s, ''), csrf: '' });
  add('csrf admin ok', { cookie: ck(T.sAdmin, T.cAdmin), csrf: T.cAdmin });
  add('csrf encoded cookie', { cookie: ck(s, encodeURIComponent(T.cMember).replace(/-/g, '%2d')), csrf: T.cMember });
  add('csrf header spaced', { cookie: ck(s, T.cMember), csrf: ` ${T.cMember}` });
  for (const origin of ['https://noevia.example.test', 'https://admin.example.test', 'https://old.example.test', 'http://192.168.1.20:8021',
    'https://evil.example.test', 'null', '', 'https://noevia.example.test/', 'HTTPS://noevia.example.test']) {
    add(`origin ${origin || '(empty)'} csrf ok`, { cookie: ck(s, T.cMember), csrf: T.cMember, origin });
    add(`origin ${origin || '(empty)'} device`, { authorization: `Bearer ${T.dAccess}`, origin });
  }
  // Clock edges.
  add('idle edge at now+1', { cookie: `cowork_session=${T.sIdleEdge}`, now: NOW + 1 });
  add('expires next at now+1', { cookie: `cowork_session=${T.sExpiresNext}`, now: NOW + 1 });
  add('member far future', { cookie: `cowork_session=${s}`, now: NOW + 40 * 86_400_000 });
  add('device at expiry', { authorization: `Bearer ${T.dAccess}`, now: NOW + 3_600_000 });
  add('device before expiry', { authorization: `Bearer ${T.dAccess}`, now: NOW + 3_599_999 });
  return c;
}

// A seeded generator (xorshift32), so the fuzz set is the same on every run.
function rng(seedValue) {
  let x = seedValue >>> 0 || 1;
  return () => { x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0; return x / 0x100000000; };
}

function fuzzCases(seedValue, count) {
  const r = rng(seedValue);
  const pick = (a) => a[Math.floor(r() * a.length)];
  const valid = [T.sMember, T.sAdmin, T.sIdleEdge, T.sZeroDisabled, T.sNoFeatures, T.dAccess, T.dAdmin, T.legacy, T.cMember];
  const noise = ['', ' ', ';', '=', '%', '%2', '%zz', '%41', '%C3%A9', '%E2%82', 'é', ' ', '\t', ',', '"', 'x', 'Bearer ', 'bearer\t', 'nva_', 'cowork_session', 'cowork_csrf', '__proto__', 'constructor'];
  const mutate = (t) => {
    const i = Math.floor(r() * (t.length + 1));
    switch (Math.floor(r() * 5)) {
      case 0: return t;
      case 1: return t.slice(0, i) + pick(noise) + t.slice(i);
      case 2: return t.slice(0, i) + t.slice(i + 1);
      case 3: return t.slice(0, i) + String.fromCharCode(t.charCodeAt(i) ^ 1) + t.slice(i + 1);
      default: return t.replace(/[-_A-Z]/g, (ch) => (r() < 0.3 ? `%${ch.charCodeAt(0).toString(16)}` : ch));
    }
  };
  const out = [];
  for (let n = 0; n < count; n += 1) {
    const input = {};
    const parts = [];
    const k = Math.floor(r() * 4);
    for (let j = 0; j < k; j += 1) {
      const name = pick(['cowork_session', 'cowork_csrf', ' cowork_session', 'cowork_session ', 'theme', pick(noise)]);
      parts.push(`${name}${r() < 0.9 ? '=' : ''}${r() < 0.7 ? mutate(pick(valid)) : pick(noise)}`);
    }
    if (parts.length || r() < 0.2) input.cookie = parts.join(pick([';', '; ', ' ;', ';;']));
    if (r() < 0.5) input.authorization = `${pick(['Bearer ', 'bearer ', 'Bearer  ', 'Bearer\t', '', 'Basic ', 'Bearer'])}${mutate(pick(valid))}${pick(['', ' ', '  ', ' x'])}`;
    if (r() < 0.3) input.csrf = mutate(T.cMember);
    if (r() < 0.3) input.origin = pick(['https://noevia.example.test', 'https://evil.example.test', 'null', '']);
    if (r() < 0.2) input.now = NOW + Math.floor((r() - 0.5) * 2 * IDLE);
    out.push(input);
  }
  return out;
}

function cookieFuzz(seedValue, count) {
  const r = rng(seedValue);
  const pick = (a) => a[Math.floor(r() * a.length)];
  const atoms = ['a', 'cowork_session', 'b', '=', '==', ';', ' ', '  ', '%', '%3D', '%3B', '%20', '%2', '%C3%A9', '%C3', '%E2%82%AC', '%F0%9F%98%80', '%ED%A0%80', '%C0%AF', '%FF', 'é', '€', '"', ',', '__proto__', 'constructor', 'toString', '1', '\t'];
  const out = [];
  for (let n = 0; n < count; n += 1) {
    const len = Math.floor(r() * 12);
    let s = '';
    for (let j = 0; j < len; j += 1) s += pick(atoms);
    out.push(s);
  }
  return out;
}

// ---- scenarios --------------------------------------------------------------------------------

const SCENARIOS = [
  { name: 'default', env: { publicOrigin: 'https://noevia.example.test', additionalOrigins: [], legacyToken: '', legacyCompat: false }, nativeClientAuth: false, users: true, settings: {} },
  {
    name: 'everything-on',
    env: { publicOrigin: 'https://noevia.example.test', additionalOrigins: ['http://192.168.1.20:8021'], legacyToken: T.legacy, legacyCompat: true },
    nativeClientAuth: true, users: true,
    settings: { public_origin_admin: 'https://admin.example.test', previous_origins: JSON.stringify(['https://old.example.test', 42, 'https://noevia.example.test']), public_origin: 'https://setup.example.test', 'feature:nativeClientAuth': 'true' },
  },
  {
    name: 'env-origin-legacy-only',
    env: { publicOrigin: 'https://noevia.example.test', additionalOrigins: [], legacyToken: T.legacy, legacyCompat: true },
    nativeClientAuth: false, users: true, settings: { public_origin: 'https://setup.example.test', previous_origins: 'not json' },
  },
  { name: 'no-users-no-origin', env: { publicOrigin: '', additionalOrigins: [], legacyToken: T.legacy, legacyCompat: true }, nativeClientAuth: true, users: false, settings: {} },
  { name: 'setup-origin-only', env: { publicOrigin: '', additionalOrigins: ['http://192.168.1.20:8021'], legacyToken: '', legacyCompat: false }, nativeClientAuth: true, users: true, settings: { public_origin: 'https://setup.example.test' } },
];

async function buildScenario(sc, tmp) {
  const dataDir = path.join(tmp, sc.name);
  fs.mkdirSync(dataDir, { recursive: true });
  // Settings must exist before createAuth reads them, so the database is created and seeded first
  // by a throwaway createAuth (which runs every migration), then reopened.
  const first = createAuth({ dataDir, publicOrigin: '', rpId: '' });
  await seed(first.db, { users: sc.users });
  const put = first.db.prepare('INSERT OR REPLACE INTO settings(key,value) VALUES(?,?)');
  for (const [k, v] of Object.entries(sc.settings)) put.run(k, v);
  const authService = createAuth({ dataDir, publicOrigin: sc.env.publicOrigin, rpId: '', legacyToken: sc.env.legacyToken, legacyCompat: sc.env.legacyCompat,
    trustProxy: true, additionalOrigins: sc.env.additionalOrigins });
  const deviceAuth = deviceLib.createDeviceAuth({ db: authService.db, audit: () => {}, publicUser: authService.publicUser, rate: createRateLimiter(),
    clientAddress: () => '127.0.0.1', origin: () => authService.origin, now: () => clock, addressesTrusted: true });
  const requestAuth = deviceLib.createRequestAuth({ enabled: () => sc.nativeClientAuth, deviceAuth, authService });
  return { db: authService.db, authService, deviceAuth, requestAuth, dataDir };
}

async function main() {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-m2-auth-'));
  try {
    const out = { version: 1, generator: 'tools/gen-auth-fixtures.cjs', core: reference, now: NOW, scenarios: [] };
    for (const sc of SCENARIOS) {
      const ctx = await buildScenario(sc, tmp);
      const db = dump(ctx.db);
      const cases = credentialCases().map(({ name, input }) => ({ name, input, expect: evaluate(ctx, input) }));
      const fuzz = fuzzCases(0x5eed + sc.name.length, 1500).map((input) => ({ input, expect: evaluate(ctx, input, { gates: false }) }));
      const app = ctx.authService.appPasswords;
      const davInputs = [
        ['member.one', APP.lan, 'lan'], ['MEMBER.ONE', APP.lan, 'lan'], ['member.one', APP.lan, 'public'], ['admin', APP.lan, 'lan'],
        ['admin', APP.pub, 'public'], ['Admin', APP.pub, 'public'], ['admin', APP.pub, 'lan'], ['gone', APP.disabled, 'lan'],
        ['member.one', APP.lan.slice(0, -1) + (APP.lan.endsWith('A') ? 'B' : 'A'), 'lan'], ['member.one', `${APP.lan}\n`, 'lan'],
        ['member.one', APP.lan.replace('nv_dav_', 'NV_DAV_'), 'lan'], ['member.one', APP.lan.toUpperCase(), 'lan'],
        ['member.one', `nv_dav_${'A'.repeat(32)}.${'x'.repeat(43)}`, 'lan'], ['member.one', '', 'lan'], ['', APP.lan, 'lan'],
        ['x'.repeat(33), APP.lan, 'lan'], ['member.one', APP.lan, 'LAN'], ['member.one', APP.lan, ''], ['nobody', `nv_dav_${'c'.repeat(32)}.${'y'.repeat(43)}`, 'lan'],
      ];
      const dav = [];
      for (const [username, password, scope] of davInputs) dav.push({ username, password, scope, expect: await app.verifyDav(username, password, scope) });
      out.scenarios.push({ name: sc.name, env: sc.env, nativeClientAuth: sc.nativeClientAuth, db, cases, fuzz, dav });
      ctx.db.close();
    }
    // Pure helpers, tabled once.
    out.parseCookies = cookieFuzz(0xc00c1e, 2500).map((header) => ({ header, expect: parseCookies({ headers: { cookie: header } }) }));
    out.bearerToken = ['', 'Bearer nva_x', 'Bearer nva_', 'bearer nva_x', 'Bearer  nva_x  ', 'Bearer nva_x y', 'Bearer\tnva_x', 'Bearer nva_x', 'Bearernva_x',
      'Bearer nvr_x', 'nva_x', 'Bearer 　nva_x', 'Bearer nva_é', 'Bearer nva_x ', 'Bearer nva_x ', ' Bearer nva_x']
      .map((authorization) => ({ authorization, expect: deviceLib.bearerToken({ headers: { authorization } }) }));
    out.hasSessionCookie = ['', 'cowork_session', 'cowork_session=', ' cowork_csrf =1', 'a=cowork_session', 'cowork_sessionx=1', 'x; cowork_csrf', 'Cowork_session=1']
      .map((cookie) => ({ cookie, expect: deviceLib.hasSessionCookie({ headers: { cookie } }) }));
    const paths = ['/api/admin', '/api/admin/', '/api/admin/users', '/api/adminx', '/api/profile', '/api/profile/', '/api/profile/app-passwords/1',
      '/api/integrations/storage', '/api/integrations/storage/test', '/api/integrations/storage/files', '/api/integrations/storage/nextcloud/login',
      '/api/connectors/', '/api/connectors/x', '/api/connectors', '/api/mcp-keys', '/api/mcp-oauth/cb', '/api/providers/chatgpt', '/api/providers', '/api/auth/device/approve', ''];
    out.browserOnly = [];
    for (const p of paths) for (const m of ['GET', 'HEAD', 'POST', 'put', 'DELETE']) out.browserOnly.push({ path: p, method: m, expect: deviceLib.browserOnly(p, m) });
    // nativeClientAuth resolution (features.cjs): env override, the stored admin setting, TRUST_PROXY availability.
    out.features = [];
    for (const envValue of [undefined, '', 'true', 'TRUE', ' on ', 'yes', '1', 'false', 'off', '0', 'no', 'maybe']) {
      for (const trust of [undefined, 'true', 'TRUE']) {
        for (const stored of [undefined, 'true', 'false', 'yes']) {
          const env = {}; if (envValue !== undefined) env.NOEVIA_FEATURE_NATIVE_CLIENT_AUTH = envValue; if (trust !== undefined) env.TRUST_PROXY = trust;
          const store = { get: (k) => (k === 'feature:nativeClientAuth' ? stored : undefined), set: () => {} };
          let expect;
          try { expect = createFeatures({ env, store, log: () => {} }).enabled('nativeClientAuth'); } catch { expect = 'error'; }
          out.features.push({ env: envValue ?? null, trustProxy: trust ?? null, stored: stored ?? null, expect });
        }
      }
    }
    // JSON.stringify bytes for server-store's json.rs (object keys already in sorted order, as a
    // serde_json Map iterates).
    const values = [null, true, 0, -0, 1, -17, 0.5, 1234.5, 1e20, 123456789012, 'plain', 'q"b\\s/\b\f\n\r\t\u0001\u001f\u007f\u2028\u2029é€😀',
      [], {}, [1, [2, []], {}], { a: 1, b: [true, null, 'x'], c: {}, d: { e: [{}] } }, { 'k\n': '\u0000' }];
    out.jsonStringify = values.map((value) => ({ value, pretty: JSON.stringify(value, null, 2), compact: JSON.stringify(value) }));
    process.stdout.write(`${JSON.stringify(out, null, 1)}\n`);
  } finally {
    Date.now = realNow;
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch((e) => { process.stderr.write(`gen-auth-fixtures failed: ${e && e.stack ? e.stack.split('\n')[0] : 'error'}\n`); process.exit(1); });
