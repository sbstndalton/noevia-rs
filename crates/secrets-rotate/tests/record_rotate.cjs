#!/usr/bin/env node
'use strict';
// Recorded POST /api/admin/secrets/rotate (track R12) for crates/secrets-rotate and the front's route.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/wasm/dav-parse.wasm at its pinned sha256> \
//     node crates/secrets-rotate/tests/record_rotate.cjs > crates/secrets-rotate/tests/fixtures/node-rotate.v1.json
//   ... node crates/secrets-rotate/tests/record_rotate.cjs --check crates/secrets-rotate/tests/fixtures/node-rotate.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code: the route (routes/auth.cjs createAuthRoutes, the admin
// mount), runRotation (secrets-rotate.cjs) and the key store (secrets.cjs createSecretStore over secret-envelope.cjs
// and the pinned dav-parse.wasm), over a node:sqlite database holding storage_connections and chatgpt_oauth_tokens
// (Node's own column definitions, without the users foreign key) and a google-drive-users folder.
// Stand-ins, each for a step the Rust front already does itself: the three front passes (`resealProviders`,
// `resealMcp`: `{ tables, failures }` exactly as rust-providers-reseal.cjs and rust-mcp-credentials.cjs return
// them; `gdriveReseal`: the store client's `{ ok, counts }` or `{ ok: false, status }`), and `audit` (captures
// authService.audit's arguments; the row's detail is JSON.stringify(detail)).
// Keys are SYNTHETIC (sha256 of QA labels) and every stored value is sealed with a fixed nonce, so a recording is
// reproducible; values Node writes back carry fresh nonces, so they are recorded as what they open to under the
// current key alone (plaintext and envelope version), never as bytes. Each source file's sha256 is recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { DatabaseSync } = require('node:sqlite');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const logged = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => { logged.push(a.map(String).join(' ')); };
const READ_FILES = ['server/routes/auth.cjs', 'server/secrets-rotate.cjs', 'server/secrets.cjs', 'server/secret-envelope.cjs', 'server/gdrive-seal.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');

const { createAuthRoutes } = require(path.join(root, 'server/routes/auth.cjs'));
const { runRotation } = require(path.join(root, 'server/secrets-rotate.cjs'));
const { createSecretStore } = require(path.join(root, 'server/secrets.cjs'));
const davParseWasm = require(path.join(root, 'server/dav-parse-wasm.cjs'));
const lock = davParseWasm.readLock ? davParseWasm.readLock() : null;
const wasmFile = path.join(root, 'server/wasm/dav-parse.wasm');
const wasmSha = crypto.createHash('sha256').update(fs.readFileSync(wasmFile)).digest('hex');

const key = (label) => crypto.createHash('sha256').update(`noevia-qa-r12-${label}`).digest();
const KEYS = { current: key('current'), previous: key('previous'), other: key('other') };
const ADMIN = 'aaaaaaaa-5555-4aaa-8aaa-aaaaaaaaaaaa';
const U1 = '11111111-5555-4111-8111-111111111111';
const U2 = '22222222-5555-4222-8222-222222222222';
const U3 = '33333333-5555-4333-8333-333333333333';
const nonce = (n) => crypto.createHash('sha256').update(`nonce-${n}`).digest().subarray(0, 12);

// A stored value: { seal: 'current'|'previous'|'other', bind: user id or null (v1), plain, n } or { raw }.
function stored(spec) {
  if (spec.raw !== undefined) return spec.raw;
  return davParseWasm.secretSeal(KEYS[spec.seal], nonce(spec.n), Buffer.from(spec.plain, 'utf8'), spec.bind || null);
}
// A Drive token file, in gdrive-seal.cjs's format with a fixed iv: { seal, plain, n } or { raw }.
function driveFile(spec) {
  if (spec.raw !== undefined) return spec.raw;
  const backupKey = Buffer.from(crypto.hkdfSync('sha256', KEYS[spec.seal], Buffer.alloc(0), Buffer.from('noevia:google-drive-user', 'utf8'), 32));
  const k = Buffer.from(crypto.hkdfSync('sha256', backupKey, Buffer.alloc(0), 'noevia google token v1', 32));
  const iv = nonce(`drive-${spec.n}`), c = crypto.createCipheriv('aes-256-gcm', k, iv);
  const body = Buffer.concat([c.update(spec.plain), c.final()]);
  return Buffer.concat([iv, c.getAuthTag(), body]).toString('base64');
}

const zero = { current: 0, rotated: 0, upgraded: 0, empty: 0, failed: 0 };
const PROVIDERS = { tables: { shared_providers: { ...zero, current: 1 }, [`user_providers:${U1}`]: { ...zero, rotated: 2 } }, failures: [] };
const PROVIDERS_FAILED = { tables: { shared_providers: { ...zero, failed: 1 } }, failures: [{ table: 'shared_providers', ref: 0, error: 'credential could not be opened' }] };
const MCP = { tables: { mcp_oauth_tokens: { ...zero, rotated: 1 }, mcp_oauth_clients: zero, directory_mcp_user_keys: zero, directory_mcp_servers: zero }, failures: [] };
const MCP_FAILED = { tables: { mcp_oauth_tokens: { ...zero, failed: 1 }, mcp_oauth_clients: zero, directory_mcp_user_keys: zero, directory_mcp_servers: zero }, failures: [{ table: 'mcp_oauth_tokens', ref: [U1, 'qa-server'], error: 'credential could not be opened' }] };
const GDRIVE_OK = { ok: true, counts: { current: 0, rotated: 1, empty: 0, failed: 0 } };

const v2 = (seal, bind, plain, n) => ({ seal, bind, plain, n });
const base = {
  previous: 'file',
  storage: [[U1, v2('current', U1, 'qa-pass-1', 1)], [U2, v2('previous', U2, 'qa-pass-2', 2)]],
  chatgpt: [[U1, v2('previous', U1, '{"access":"qa-a","refresh":"qa-r"}', 3)]],
  drive: [],
  front: { providers: PROVIDERS, mcp: MCP, gdrive: GDRIVE_OK },
};
const cases = [];
const add = (name, over) => cases.push({ name, ...base, ...over, front: { ...base.front, ...(over.front || {}) } });

add('typical', {});
add('member-refused', { role: 'member' });
add('no-previous-key', { previous: 'none' });
add('previous-env-hex', { previous: 'env-hex' });
add('previous-env-base64', { previous: 'env-base64' });
add('previous-same-as-current', { previous: 'same' });
add('statuses', { storage: [
  [U1, v2('current', U1, 'qa-current', 10)],
  [U2, v2('previous', U2, 'qa-previous', 11)],
  [U3, { seal: 'current', bind: null, plain: 'qa-v1-current', n: 12 }],
  ['44444444-5555-4444-8444-444444444444', { seal: 'previous', bind: null, plain: 'qa-v1-previous', n: 13 }],
  ['55555555-5555-4555-8555-555555555555', { raw: 'qa-plaintext-secret' }],
  ['66666666-5555-4666-8666-666666666666', v2('current', U1, 'qa-copied-from-u1', 14)],
  ['77777777-5555-4777-8777-777777777777', v2('other', '77777777-5555-4777-8777-777777777777', 'qa-unknown-key', 15)],
  ['88888888-5555-4888-8888-888888888888', { raw: 'enc:v2:!!!!' }],
  ['99999999-5555-4999-8999-999999999999', { raw: 'enc:v1:AAAA' }],
  ['', v2('current', U1, 'qa-empty-user', 16)],
  ['aaaaaaaa-6666-4aaa-8aaa-aaaaaaaaaaaa', v2('previous', 'aaaaaaaa-6666-4aaa-8aaa-aaaaaaaaaaaa', '', 17)],
  ['bbbbbbbb-6666-4bbb-8bbb-bbbbbbbbbbbb', v2('previous', 'bbbbbbbb-6666-4bbb-8bbb-bbbbbbbbbbbb', 'qa é 😀 \u0000 text', 18)],
] });
add('no-chatgpt-table', { chatgpt: null });
add('empty-tables', { storage: [], chatgpt: [] });
add('cas-conflict', { conflict: { table: 'storage_connections', user: U2 } });
add('cas-conflict-chatgpt', { conflict: { table: 'chatgpt_oauth_tokens', user: U1 } });
add('drive-files', { drive: [
  ['a.sealed', { seal: 'current', plain: '{"refresh":"qa-1"}', n: 1 }],
  ['b.sealed', { seal: 'previous', plain: '{"refresh":"qa-2"}', n: 2 }],
  ['c.sealed', { raw: '' }],
  ['d.sealed', { raw: ' \n\t' }],
  ['e.sealed', { raw: 'not sealed' }],
  ['f.sealed', { seal: 'current', plain: 'not json', n: 3 }],
  ['g.txt', { seal: 'previous', plain: '{"refresh":"qa-3"}', n: 4 }],
  ['B.sealed', { seal: 'current', plain: '{"refresh":"qa-4"}', n: 5 }],
], front: { gdrive: { ok: true, counts: { current: 2, rotated: 0, empty: 2, failed: 0 } } } });
add('drive-front-rotated-more-than-current', { drive: [['a.sealed', { seal: 'current', plain: '{}', n: 6 }]], front: { gdrive: { ok: true, counts: { current: 0, rotated: 5, empty: 0, failed: 0 } } } });
add('drive-front-failed', { drive: [['b.sealed', { seal: 'previous', plain: '{}', n: 7 }]], front: { gdrive: { ok: false, status: 503 } } });
add('drive-front-failed-nothing-failed', { drive: [['a.sealed', { seal: 'current', plain: '{}', n: 8 }]], front: { gdrive: { ok: false, status: 503 } } });
add('drive-front-unreachable', { drive: [['b.sealed', { seal: 'previous', plain: '{}', n: 9 }]], front: { gdrive: 'throws' } });
add('drive-no-folder', { drive: null });
add('provider-and-mcp-failures', { front: { providers: PROVIDERS_FAILED, mcp: MCP_FAILED } });
add('providers-throw', { front: { providers: 'throws' } });
add('mcp-throws', { front: { mcp: 'throws' } });
add('many-failures', { storage: Array.from({ length: 60 }, (_, i) => [`cccccccc-6666-4ccc-8ccc-${String(i).padStart(12, '0')}`, { raw: 'enc:v2:!!!!' }]) });

async function record(c) {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-qa-rotate-'));
  fs.writeFileSync(path.join(data, 'secrets.key'), KEYS.current, { mode: 0o600 });
  const env = {};
  if (c.previous === 'file') fs.writeFileSync(path.join(data, 'secrets.key.previous'), KEYS.previous, { mode: 0o600 });
  if (c.previous === 'env-hex') env.SECRETS_KEY_PREVIOUS = KEYS.previous.toString('hex');
  if (c.previous === 'env-base64') env.SECRETS_KEY_PREVIOUS = ` ${KEYS.previous.toString('base64')}\n`;
  if (c.previous === 'same') fs.writeFileSync(path.join(data, 'secrets.key.previous'), KEYS.current, { mode: 0o600 });
  const secrets = createSecretStore(data, { env });
  const real = new DatabaseSync(':memory:');
  real.exec(`CREATE TABLE storage_connections(user_id TEXT PRIMARY KEY, kind TEXT NOT NULL, base_url TEXT NOT NULL DEFAULT '', bucket TEXT NOT NULL DEFAULT '',
    username TEXT NOT NULL DEFAULT '', secret TEXT NOT NULL DEFAULT '', corpus_root TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL, region TEXT NOT NULL DEFAULT 'us-east-1')`);
  if (c.chatgpt) real.exec("CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY, data_enc TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'connected', updated_at INTEGER NOT NULL)");
  const rows = { storage_connections: [], chatgpt_oauth_tokens: [] };
  for (const [user, spec] of c.storage) {
    const value = stored(spec);
    rows.storage_connections.push({ user, value });
    real.prepare("INSERT INTO storage_connections(user_id,kind,secret,updated_at) VALUES(?,'webdav',?,1)").run(user, value);
  }
  for (const [user, spec] of c.chatgpt || []) {
    const value = stored(spec);
    rows.chatgpt_oauth_tokens.push({ user, value });
    real.prepare("INSERT INTO chatgpt_oauth_tokens(user_id,data_enc,updated_at) VALUES(?,?,1)").run(user, value);
  }
  const drive = [];
  if (c.drive) {
    fs.mkdirSync(path.join(data, 'google-drive-users'));
    for (const [name, spec] of c.drive) {
      const text = driveFile(spec);
      drive.push({ name, text });
      fs.writeFileSync(path.join(data, 'google-drive-users', name), text);
    }
  }
  // The compare-and-swap loses for `conflict.user`: another writer saves that row between the read and the write.
  const db = { prepare(sql) {
    const st = real.prepare(sql);
    if (!c.conflict || !sql.startsWith(`UPDATE ${c.conflict.table} `)) return st;
    const col = c.conflict.table === 'storage_connections' ? 'secret' : 'data_enc';
    return { run: (...args) => {
      if (args[1] === c.conflict.user) real.prepare(`UPDATE ${c.conflict.table} SET ${col}='qa-changed-meanwhile' WHERE user_id=?`).run(c.conflict.user);
      return st.run(...args);
    } };
  } };
  const audits = [];
  const pass = (v) => async () => { if (v === 'throws') throw new Error('the Rust front answered the re-seal with something unexpected'); return JSON.parse(JSON.stringify(v)); };
  const gdriveReseal = async () => { if (c.front.gdrive === 'throws') throw new Error('unreachable'); return JSON.parse(JSON.stringify(c.front.gdrive)); };
  const rotateSecrets = (actorId) => runRotation({ secrets, db, dataDir: data, audit: (action, actor, target, detail) => audits.push({ action, actor, target, detail: JSON.stringify(detail) }),
    actorId, resealProviders: pass(c.front.providers), resealMcp: pass(c.front.mcp), gdriveReseal });
  const routes = createAuthRoutes({ json: (res, status, body) => { res.status = status; res.body = JSON.stringify(body); return true; }, readJson: async () => ({}), rotateSecrets });
  const res = {};
  await routes.account({ method: 'POST', headers: {} }, res, { path: '/api/admin/secrets/rotate', authn: { user: { id: ADMIN, role: c.role || 'admin' } } });
  // What each row holds now, opened under the current key alone (fresh nonces make the bytes unrepeatable).
  const after = {};
  for (const [table, col] of [['storage_connections', 'secret'], ['chatgpt_oauth_tokens', 'data_enc']]) {
    after[table] = rows[table].map(({ user, value }) => {
      const now = real.prepare(`SELECT ${col} AS v FROM ${table} WHERE user_id=?`).get(user).v;
      if (now === value) return { user, same: true };
      let opened;
      try {
        const r = davParseWasm.secretOpen([KEYS.current], user || null, now);
        opened = { keyUsed: r.keyUsed, plain: Buffer.from(r.plain).toString('utf8') };
      } catch { opened = null; }
      return { user, same: false, version: now.slice(0, 7), opened, raw: now === 'qa-changed-meanwhile' ? now : undefined };
    });
  }
  const unchangedDrive = drive.every(({ name, text }) => fs.readFileSync(path.join(data, 'google-drive-users', name), 'utf8') === text);
  fs.rmSync(data, { recursive: true, force: true });
  const { name, ...input } = c;
  return {
    name,
    input: { ...input, storage: rows.storage_connections, chatgpt: c.chatgpt ? rows.chatgpt_oauth_tokens : null, drive: c.drive ? drive : null },
    expected: { status: res.status, reply: res.body, audits, after, unchangedDrive },
  };
}

(async () => {
  const results = [];
  for (const c of cases) results.push(await record(c));
  const fixture = JSON.stringify({
    generator: 'crates/secrets-rotate/tests/record_rotate.cjs',
    keys: Object.fromEntries(Object.entries(KEYS).map(([k, v]) => [k, v.toString('hex')])),
    admin: ADMIN,
    wasmSha256: wasmSha,
    sources: Object.fromEntries(READ_FILES.map((f) => [f, sha(f)])),
    cases: results,
  }, null, 1) + '\n';
  const check = process.argv.indexOf('--check');
  if (check !== -1) {
    if (fs.readFileSync(process.argv[check + 1], 'utf8') !== fixture) { process.stderr.write('rotate fixture differs from a fresh recording\n'); process.exit(1); }
    process.stderr.write(`rotate fixture matches (${results.length} cases)\n`);
    return;
  }
  process.stdout.write(fixture);
  void lock;
})().catch((e) => { process.stderr.write(`${e.stack || e}\n${logged.join('\n')}\n`); process.exit(1); });
