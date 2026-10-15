'use strict';
// Test-only Node oracle for server-chatgpt-status and the front's GET /api/providers/chatgpt
// (sbstndalton/noevia#1373). Synthetic tenants, synthetic tokens and keys only. For every case it
// builds a fresh UI_DATA_DIR with core's own auth.cjs createAuth (cowork.db, settings), writes a
// fixed synthetic secrets.key (and secrets.key.previous when the case rotates), creates
// secrets.cjs createSecretStore, chatgpt-oauth.cjs createChatGptOAuth (its own DDL), features.cjs
// createFeatures over settingsStore, and routes/providers.cjs createProviderRoutes wired as
// server/index.cjs wires them, then answers the GET through http.cjs json into a capturing
// response. Ciphertexts are sealed by core's secret-envelope.cjs encryptJs.
//
//   node crates/server-chatgpt-status/tests/oracle.cjs <core-checkout> <out.json>
//
// The output holds each case's inputs and Node's exact status and body bytes; sealing nonces are
// random, but no answer depends on them, so the output is deterministic. CI regenerates it and
// compares it with the committed tests/fixtures/node-chatgpt-status.v1.json; the Rust test seals
// the same plaintexts with the same keys and must produce the same bytes.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node oracle.cjs <core-checkout> <out.json>');
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createAuth } = req('auth.cjs');
const { createSecretStore } = req('secrets.cjs');
const envelope = req('secret-envelope.cjs');
const chatgpt = req('chatgpt-oauth.cjs');
const { createFeatures, settingsStore, REGISTRY } = req('features.cjs');
const { createProviderRoutes } = req('routes/providers.cjs');
const http = req('http.cjs');

const A = 'aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa';
const B = 'bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb';
const KEYS = { current: '07'.repeat(32), previous: '0b'.repeat(32), other: '0d'.repeat(32) };
const tokens = (extra) => JSON.stringify({ accessToken: 'AT-SYNTHETIC-LEAK', refreshToken: 'RT-SYNTHETIC-LEAK', accountId: 'acct-synthetic', fedRamp: false, expiresAt: 1800003600000, connectedAt: 1800000000000, ...extra });
const sealed = (plain, opts = {}) => ({ seal: plain, key: opts.key || 'current', aad: opts.aad === undefined ? 'owner' : opts.aad });

// A row's data_enc is { seal, key, aad } (aad: 'owner' = the row's own user id, 'A'/'B', or null
// for a legacy v1 envelope) or { raw } (stored as is). flag: env value or null; setting: the
// feature:chatgptOAuth settings value or null; previous: 'file' | 'env' | null.
const cases = [];
const add = (name, spec) => cases.push({ name, flagEnv: null, setting: 'true', previous: null, rows: [], ...spec });

add('flag off by default (no setting row)', { setting: null, rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'person@example.test', plan: 'plus' })) }] });
add('flag off by setting', { setting: 'false', rows: [{ user: A, state: 'connected', data: { raw: 'enc:v2:not-even-read' } }] });
add('flag setting not exactly true', { setting: 'TRUE', rows: [] });
add('flag setting with spaces', { setting: ' true', rows: [] });
add('flag env off beats setting', { flagEnv: 'off', setting: 'true', rows: [] });
add('flag env on beats setting', { flagEnv: ' Yes ', setting: 'false', rows: [] });
add('disconnected: no row', {});
add('disconnected: only a foreign tenant row', { rows: [{ user: B, state: 'connected', data: sealed(tokens({ email: 'other@example.test', plan: 'team' })) }] });
add('connected', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'synthetic.person@example.test', plan: 'plus' })) }, { user: B, state: 'connected', data: sealed(tokens({ email: 'b@example.test', plan: 'team' })) }] });
add('connected, expired access token (status does not refresh)', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'pro', expiresAt: 1 })) }] });
add('connected, no email or plan', { rows: [{ user: A, state: 'connected', data: sealed(tokens({})) }] });
add('connected, email null, plan empty', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: null, plan: '' })) }] });
add('connected, email without @', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'nobody', plan: 'plus' })) }] });
add('connected, email non-string', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: ['a@b'], plan: 'plus' })) }] });
add('connected, email several @ and empty name', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: '@a@b@example.test', plan: 'plus' })) }] });
add('connected, email emoji first (lone high surrogate)', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: '\u{1F600}x@example.test', plan: 'plus' })) }] });
add('connected, email lone surrogate escapes', { rows: [{ user: A, state: 'connected', data: sealed('{"email":"\\udc00a@\\ud800.test","plan":"plus"}') }] });
add('connected, email cut at 120 units inside a pair', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@' + 'd'.repeat(116) + '\u{1F600}\u{1F600}', plan: 'plus' })) }] });
add('connected, email long domain', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@' + 'é'.repeat(300), plan: 'plus' })) }] });
add('connected, duplicate keys last wins', { rows: [{ user: A, state: 'connected', data: sealed('{"email":"first@x.test","plan":"plus","email":"second@y.test","plan":"team"}') }] });
add('connected, __proto__ key', { rows: [{ user: A, state: 'connected', data: sealed('{"__proto__":{"email":"x@y"},"plan":"plus"}') }] });
add('plan projection: credential-looking keys inside an object are dropped', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: { type: 'plus', name: 'Plus', tier: 'paid', access_token: 'AT-PLAN-LEAK', refresh_token: 'RT-PLAN-LEAK', apiKey: 'sk-PLAN-LEAK' } })) }] });
add('plan projection: array', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: ['plus'] })) }] });
add('plan projection: number and boolean', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 7 })) }] });
add('plan projection: 41 characters', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'a'.repeat(41) })) }] });
add('plan projection: 40 characters', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'A9_.-'.repeat(8) })) }] });
add('plan projection: odd characters', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'plus plan' })) }] });
add('plan projection: non-ASCII', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'tëam' })) }] });
add('reconnect state', { rows: [{ user: A, state: 'reconnect', data: sealed(tokens({ email: 'p@example.test', plan: 'plus' })) }] });
add('other state text', { rows: [{ user: A, state: 'Connected', data: { raw: 'enc:v2:never-opened' } }] });
add('corrupt envelope', { rows: [{ user: A, state: 'connected', data: { raw: 'enc:v2:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA' } }] });
add('truncated envelope', { rows: [{ user: A, state: 'connected', data: { raw: 'enc:v2:' } }] });
add("foreign tenant's ciphertext copied into the owner's row", { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'b@example.test', plan: 'team' }), { aad: 'B' }) }, { user: B, state: 'connected', data: sealed(tokens({ email: 'b@example.test', plan: 'team' })) }] });
add('sealed with an unknown key', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'plus' }), { key: 'other' }) }] });
add('sealed with the previous key (file)', { previous: 'file', rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'plus' }), { key: 'previous' }) }] });
add('sealed with the previous key (env)', { previous: 'env', rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'plus' }), { key: 'previous' }) }] });
add('previous key not configured', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'plus' }), { key: 'previous' }) }] });
add('legacy v1 envelope', { rows: [{ user: A, state: 'connected', data: sealed(tokens({ email: 'p@example.test', plan: 'plus' }), { aad: null }) }] });
add('legacy plaintext row', { rows: [{ user: A, state: 'connected', data: { raw: tokens({ email: 'plain@example.test', plan: 'go' }) } }] });
add('plaintext not JSON', { rows: [{ user: A, state: 'connected', data: sealed('not json') }] });
add('plaintext empty', { rows: [{ user: A, state: 'connected', data: sealed('') }] });
add('plaintext JSON null', { rows: [{ user: A, state: 'connected', data: sealed('null') }] });
add('plaintext JSON array', { rows: [{ user: A, state: 'connected', data: sealed('[{"email":"p@example.test"}]') }] });
add('plaintext JSON string', { rows: [{ user: A, state: 'connected', data: sealed('"p@example.test"') }] });
add('plaintext invalid UTF-8 becomes U+FFFD', { rows: [{ user: A, state: 'connected', data: { seal_hex: Buffer.from('{"email":"p\xff@example.test","plan":"plus"}', 'latin1').toString('hex'), key: 'current', aad: 'owner' } }] });

function sealWith(row, data) {
  const aad = data.aad === 'owner' ? row.user : data.aad === 'A' ? A : data.aad === 'B' ? B : undefined;
  const key = Buffer.from(KEYS[data.key], 'hex');
  const plain = data.seal_hex !== undefined ? Buffer.from(data.seal_hex, 'hex') : data.seal;
  return envelope.encryptJs(key, plain, aad);
}

async function run(c) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-chatgpt-status-oracle-'));
  const warn = console.warn;
  console.warn = () => {};
  try {
    fs.writeFileSync(path.join(dir, 'secrets.key'), Buffer.from(KEYS.current, 'hex'), { mode: 0o600 });
    if (c.previous === 'file') fs.writeFileSync(path.join(dir, 'secrets.key.previous'), Buffer.from(KEYS.previous, 'hex'), { mode: 0o600 });
    const env = c.previous === 'env' ? { SECRETS_KEY_PREVIOUS: KEYS.previous } : {};
    const secrets = createSecretStore(dir, { env });
    const auth = createAuth({ dataDir: dir, publicOrigin: 'http://127.0.0.1:8021', rpId: '127.0.0.1', secrets });
    const db = auth.db;
    const oauth = chatgpt.createChatGptOAuth({ db, secrets, fetchImpl: async () => { throw new Error('status never fetches'); }, now: () => 1800000000000 });
    const ddl = db.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name='chatgpt_oauth_tokens'").get().sql;
    const store = settingsStore(db);
    if (c.setting !== null) store.set('feature:chatgptOAuth', c.setting);
    const featureEnv = c.flagEnv === null ? {} : { NOEVIA_FEATURE_CHATGPT_OAUTH: c.flagEnv };
    const features = createFeatures({ env: featureEnv, store, registry: { chatgptOAuth: REGISTRY.chatgptOAuth }, log: () => {} });
    const rows = c.rows.map((r) => ({ user: r.user, state: r.state, data_enc: r.data.raw !== undefined ? r.data.raw : sealWith(r, r.data) }));
    for (const r of rows) db.prepare('INSERT INTO chatgpt_oauth_tokens(user_id, data_enc, state, updated_at) VALUES(?,?,?,1)').run(r.user, r.data_enc, r.state);
    const routes = createProviderRoutes({
      json: http.json, readBody: async () => '', readJson: async () => ({}), fetchJson: async () => { throw new Error('no fetch'); }, endpointApproved: () => false,
      PROVIDERS: [], PROJECTS: [], DEFAULT_PROVIDER_ID: 'default', modelManager: { enabled: false }, currentWorkspace: () => ({}), saveProjects: () => { throw new Error('status never saves'); },
      registry: { saveProviders: () => { throw new Error('status never saves'); }, saveSharedProviders: () => { throw new Error('status never saves'); }, maskKey: () => null },
      chatgptOAuth: oauth, chatgptEnabled: () => features.enabled('chatgptOAuth'),
    });
    const before = JSON.stringify(db.prepare('SELECT * FROM chatgpt_oauth_tokens ORDER BY user_id').all());
    const res = { status: 0, headers: null, body: '', writeHead(s, h) { this.status = s; this.headers = h; }, end(b) { this.body = String(b); } };
    const url = new URL('http://127.0.0.1/api/providers/chatgpt');
    const handled = await routes({ method: 'GET', url: url.pathname, headers: {} }, res, { path: url.pathname, url, authn: { user: { id: A, role: 'member' } } });
    if (!handled) throw new Error(`${c.name}: route not handled`);
    if (JSON.stringify(db.prepare('SELECT * FROM chatgpt_oauth_tokens ORDER BY user_id').all()) !== before) throw new Error(`${c.name}: status wrote a row`);
    if (/LEAK/.test(res.body)) throw new Error(`${c.name}: a stored token or plan secret reached the answer`);
    auth.db.close?.();
    return { ddl, status: res.status, body: res.body };
  } finally {
    console.warn = warn;
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

(async () => {
  const results = [];
  let ddl = null;
  for (const c of cases) {
    const r = await run(c);
    if (ddl !== null && ddl !== r.ddl) throw new Error('chatgpt_oauth_tokens DDL changed between cases');
    ddl = r.ddl;
    results.push({ ...c, expected: { status: r.status, body: r.body } });
  }
  const doc = { v: 1, owner: A, foreign: B, keys: KEYS, chatgptDdl: ddl, cases: results };
  fs.writeFileSync(out, JSON.stringify(doc, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
