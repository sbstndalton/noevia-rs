'use strict';
// Test-only Node oracle for the front's native Google Drive tools (track R5, gdrive_tools.rs). For every
// scenario it runs core's REAL chat Drive tools at the checkout given (server/gdrive-tools.cjs createDriveTools
// with gdrive-files.cjs and gdrive-broker.cjs createDriveAccounts), with a scripted Drive API (fetchImpl), a
// scripted token broker, a connection state file and a fixed clock and multipart boundary, and records for
// every step: what was asked (execute or describeTarget, the account, the tool, the arguments, the chat and
// exchange keys), every HTTP request core made (method, address, headers, body), every sign-in it marked as
// refused, and what it answered. Synthetic data only; nothing is contacted.
//
//   node bins/noevia-server/tests/gdrive-tools-oracle.cjs <core-checkout> <out.json>
//
// tests/gdrive_tools_oracle.rs replays it through gdrive_tools::{execute, describe_target}. The sha256 of every
// core file it loads is in the fixture.
process.env.TZ = 'UTC';
console.warn = () => {};
console.log = () => {};
const fs = require('node:fs');
const os = require('node:os');
const crypto = require('node:crypto');
const path = require('node:path');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node gdrive-tools-oracle.cjs <core-checkout> <out.json>');
const SOURCES = ['gdrive-tools.cjs', 'gdrive-files.cjs', 'gdrive-broker.cjs'];
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const BOUNDARY_BYTES = Buffer.from('0123456789abcdef', 'hex');
const realRandomBytes = crypto.randomBytes;
crypto.randomBytes = (n, ...rest) => (n === 8 && !rest.length ? Buffer.from(BOUNDARY_BYTES) : realRandomBytes(n, ...rest));
const { createDriveTools } = req('gdrive-tools.cjs');
const { createDriveAccounts } = req('gdrive-broker.cjs');

const API = 'http://127.0.0.1:9/drive/v3';
const UPLOAD = 'http://127.0.0.1:9/upload/drive/v3';
const CAP = 8000;
const START = 1_800_000_000_000;
const STATES = { 'user-1': 'connected', 'user-2': 'connected', 'user-off': 'error', 'user-gone': 'connected' };
const MEMBER = (id) => ({ id, role: 'member' });

const file = (id, extra = {}) => ({ id, name: `QA ${id}.txt`, mimeType: 'text/plain', size: '12', modifiedTime: '2026-11-01T10:00:00.000Z', version: '7', webViewLink: `https://drive.example.test/${id}`, ...extra });
const json = (body, status = 200) => ({ status, json: body });
const text = (body, status = 200) => ({ status, text: body });
const bytes = (hex, status = 200) => ({ status, hex });
const status = (s) => ({ status: s, text: '' });

// A step: { op: 'execute' | 'target', user, name, args, chatKey?, exchangeKey?, advance?, http: [response], sessions?: [answer] }.
// A response: { status, json | text | hex }. A session answer (instead of the default token rotation):
// { refuse: { status, error, code? } } or 'throw'.
const X = { exchangeKey: 'ex-1' };
const SCENARIOS = [
  { name: 'search and list', steps: [
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: "qa's \\notes", limit: 5 }, ...X, http: [json({ files: [file('a1'), file('a2', { size: '', mimeType: '', modifiedTime: null, name: 'QA 😀 deck' }), file('a3', { size: 0 })] })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: '  qa  ' }, ...X, http: [json({ files: [] })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'qa', limit: '3' }, ...X, http: [json({})] },
    ...[3.7, 0, -2, 100, 'abc', true, null, [7], '0x10', ' 4 ', 1e400 === Infinity ? 'Infinity' : 9].map((limit) => (
      { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'qa', limit }, ...X, http: [json({ files: [file('l1')] })] })),
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 5 }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: {}, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'x'.repeat(201) }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: '😀'.repeat(100) }, ...X, http: [json({ files: [] })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: '😀'.repeat(101) }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'qa' }, ...X, http: [json({ files: 'oops' })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'qa' }, ...X, http: [json({ files: { a: 1 } })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'qa' }, ...X, http: [json({ files: [null] })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_search_files', args: { query: 'qa' }, ...X, http: [text('not json')] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_list_recent', args: {}, ...X, http: [json({ files: [] })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_list_recent', args: { limit: 2 }, ...X, http: [json({ files: [file('r1'), file('r2', { size: 1234 })] })] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_list_recent', args: {}, ...X, http: [status(500)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_list_recent', args: {}, ...X, http: [status(204)] },
  ] },
  { name: 'metadata', steps: [
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: ' m1 ' }, ...X, http: [json(file('m1'))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'm2' }, ...X, http: [json(file('m2', { webViewLink: '' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'notes/x.md' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: {}, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'x'.repeat(201) }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'm3' }, ...X, http: [status(404)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'm3' }, ...X, http: [status(403)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'm4' }, ...X, http: [json(null)] },
  ] },
  { name: 'read, then update only what was read in full in this exchange', steps: [
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'f1' }, ...X, http: [json(file('f1')), text('qa line one\nqa line two é 😀', 206)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'f1', content: 'qa replaced é' }, ...X, http: [json(file('f1')), json(file('f1', { version: '8' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'f1', content: 'again' }, ...X, http: [json(file('f1', { version: '8' })), json(file('f1', { version: '9' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'f1', content: 'again' }, exchangeKey: 'ex-2', http: [] },
    { op: 'execute', user: MEMBER('user-2'), name: 'drive_update_file', args: { fileId: 'f1', content: 'other account' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'f1', content: 'changed elsewhere' }, ...X, http: [json(file('f1', { version: '12' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'big' }, ...X, http: [json(file('big')), text('y'.repeat(9000))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'big', content: 'x' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'fit' }, ...X, http: [json(file('fit', { name: 'fit' })), text('z'.repeat(CAP - 5))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'over' }, ...X, http: [json(file('over', { name: 'over' })), text('z'.repeat(CAP - 5))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'huge' }, ...X, http: [json(file('huge', { name: 'h' })), text('w'.repeat(70_000))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'doc' }, ...X, http: [json(file('doc', { mimeType: 'application/vnd.google-apps.document' })), text('qa doc text')] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'doc', content: 'x' }, ...X, http: [json(file('doc', { mimeType: 'application/vnd.google-apps.document' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'sheet' }, ...X, http: [json(file('sheet', { mimeType: 'application/vnd.google-apps.spreadsheet' })), text('a,b\n1,2')] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'json1' }, ...X, http: [json(file('json1', { mimeType: 'application/json' })), text('{"qa":1}')] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'img' }, ...X, http: [json(file('img', { mimeType: 'image/png' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'bin' }, ...X, http: [json(file('bin', { mimeType: '' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'bad8' }, ...X, http: [json(file('bad8')), bytes('71610aff fe c3 28 e2 82 f0 9f 98'.replace(/ /g, ''))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'forb' }, ...X, http: [json(file('forb')), status(403)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'nov' }, ...X, http: [json(file('nov', { version: undefined })), text('qa')] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'nov', content: 'x' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'not/a/drive/id', content: 'x' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'ttl' }, chatKey: 'chat-1', http: [json(file('ttl')), text('qa')] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'ttl', content: 'in time' }, chatKey: 'chat-1', advance: 30 * 60 * 1000, http: [json(file('ttl')), json(file('ttl', { version: '8' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'ttl', content: 'too late' }, chatKey: 'chat-1', advance: 30 * 60 * 1000 + 1, http: [] },
  ] },
  { name: 'create and trash', steps: [
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: '  QA notes.md ', content: 'qa é 😀', mimeType: 'text/markdown' }, ...X, http: [json(file('c1', { name: 'QA notes.md', mimeType: 'text/markdown' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'c1', content: 'edited after create' }, ...X, http: [json(file('c1', { name: 'QA notes.md', mimeType: 'text/markdown' })), json(file('c1', { version: '8' }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: 'QA.bin', content: 5, mimeType: 'image/png' }, ...X, http: [json(file('c2', { webViewLink: undefined, version: undefined }))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: 'QA.json', content: '{}', mimeType: 'application/json' }, ...X, http: [json(file('c3'))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: '   ', content: 'x' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: 'x'.repeat(256), content: 'x' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: 'QA.txt', content: 'x' }, ...X, http: [status(204)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_trash_file', args: { fileId: 't1' }, ...X, http: [json(file('t1'))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_trash_file', args: { fileId: 'a b' }, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_unknown', args: {}, ...X, http: [] },
  ] },
  { name: 'a refused token is refreshed past once, a second refusal marks that sign-in', steps: [
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'k1' }, ...X, http: [status(401), json(file('k1'))] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_get_metadata', args: { fileId: 'k1' }, ...X, http: [status(401), status(401)] },
    { op: 'execute', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'k2' }, ...X, http: [json(file('k2')), status(401), text('after refresh')] },
  ] },
  { name: 'who may use Drive at all', steps: [
    { op: 'execute', user: MEMBER('user-off'), name: 'drive_list_recent', args: {}, ...X, http: [] },
    { op: 'execute', user: MEMBER('user-nobody'), name: 'drive_list_recent', args: {}, ...X, http: [] },
    { op: 'execute', user: null, name: 'drive_list_recent', args: {}, ...X, http: [] },
    { op: 'execute', user: MEMBER('bad id!'), name: 'drive_list_recent', args: {}, ...X, http: [] },
    ...[{ status: 409, error: 'x', code: 'disconnected' }, { status: 409, error: 'x', code: 'reconnect' }, { status: 502, error: 'x', code: 'transient' },
      { status: 500, error: 'Some broker words' }, { status: 503 }].map((refuse) => (
      { op: 'execute', user: MEMBER('user-gone'), name: 'drive_list_recent', args: {}, ...X, http: [], sessions: [{ refuse }] })),
    { op: 'execute', user: MEMBER('user-gone'), name: 'drive_list_recent', args: {}, ...X, http: [], sessions: ['throw'] },
  ] },
  { name: 'the approval card names the file a write would change', steps: [
    { op: 'target', user: MEMBER('user-1'), name: 'drive_read_file', args: { fileId: 'f1' }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: '  QA new.txt  ' }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: 'é'.repeat(300) }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_create_file', args: { name: '' }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: ' f9 ' }, http: [json(file('f9'))] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_trash_file', args: '{"fileId":"f8"}', http: [json(file('f8', { name: 'QA "eight"' }))] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_trash_file', args: '{not json', http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_trash_file', args: ['f8'], http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'noevia projects/qa/notes.md' }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'x'.repeat(250) }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_update_file', args: {}, http: [] },
    { op: 'target', user: MEMBER('user-off'), name: 'drive_update_file', args: { fileId: 'f9' }, http: [] },
    { op: 'target', user: null, name: 'drive_update_file', args: { fileId: 'f9' }, http: [] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'gone' }, http: [status(404)] },
    { op: 'target', user: MEMBER('user-1'), name: 'drive_update_file', args: { fileId: 'gone' }, http: [status(500)] },
    { op: 'target', user: MEMBER('user-1'), name: 'not_a_drive_tool', args: {}, http: [] },
  ] },
];

function response(r) {
  const headers = {};
  let body = null;
  if (r.json !== undefined) { body = JSON.stringify(r.json); headers['content-type'] = 'application/json'; }
  else if (r.hex !== undefined) body = Buffer.from(r.hex, 'hex');
  else if (r.text !== undefined && r.status !== 204) body = r.text;
  return new Response(r.status === 204 ? null : body, { status: r.status, headers });
}

async function run(scenario, dir) {
  const signalFile = path.join(dir, 'gdrive-connections.json');
  fs.writeFileSync(signalFile, JSON.stringify({ backup: { usable: false, state: 'disconnected', owner: null }, personal: STATES }));
  const clock = { now: START };
  let requests = [];
  let rejected = [];
  let script = null;
  const tokens = new Map(); // user -> [n]
  const tokenOf = (user, n) => `qa-token-${user}-${n}`;
  const broker = {
    async gdriveSession(user, admin, rejectedToken) {
      const scripted = script.sessions.shift();
      if (scripted === 'throw') throw new Error('store down');
      if (scripted && scripted.refuse) return { ok: false, ...scripted.refuse };
      let n = tokens.get(user) || 1;
      if (rejectedToken === tokenOf(user, n)) n += 1;
      tokens.set(user, n);
      return { ok: true, session: { accessToken: tokenOf(user, n), expiresAt: null, generation: `gen-${user}-${n}` } };
    },
    async gdriveReject(user, admin, generation) { rejected.push({ user, admin, generation }); return { ok: true, dropped: true }; },
  };
  const fetchImpl = async (url, init = {}) => {
    const body = init.body === undefined ? null : Buffer.isBuffer(init.body) ? init.body.toString('utf8') : String(init.body);
    requests.push({ method: init.method || 'GET', url, headers: { ...(init.headers || {}) }, body });
    const r = script.http.shift();
    if (!r) throw new Error(`oracle: no scripted response in ${scenario.name}`);
    return response(r);
  };
  const accounts = createDriveAccounts({ broker, dataDir: dir, apiBase: API, uploadBase: UPLOAD, fetchImpl, now: () => clock.now, signalFile, warn: () => {} });
  const tools = createDriveTools({ accounts, cap: CAP, now: () => clock.now });
  const steps = [];
  for (const step of scenario.steps) {
    if (step.advance) clock.now += step.advance;
    script = { http: [...step.http], sessions: [...(step.sessions || [])] };
    requests = []; rejected = [];
    let answer;
    if (step.op === 'execute') {
      answer = await tools.execute(step.user, step.name, step.args, { chatKey: step.chatKey ?? null, exchangeKey: step.exchangeKey ?? null });
    } else {
      answer = await tools.describeTarget(step.user, step.name, step.args);
    }
    if (script.http.length || script.sessions.length) throw new Error(`oracle: unused script in ${scenario.name} / ${step.name}`);
    steps.push({
      at: clock.now,
      request: { op: step.op, user: step.user, name: step.name, arguments: step.args, chatKey: step.chatKey ?? null, exchangeKey: step.exchangeKey ?? null, cap: CAP },
      script: { http: step.http, sessions: step.sessions || [] },
      requests, rejected,
      answer: answer === undefined ? null : answer,
    });
  }
  return { name: scenario.name, steps };
}

(async () => {
  const scenarios = [];
  for (const s of SCENARIOS) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-gdrive-oracle-'));
    try { scenarios.push(await run(s, dir)); } finally { fs.rmSync(dir, { recursive: true, force: true }); }
  }
  const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(path.resolve(core), 'server', f))).digest('hex');
  const fixture = {
    version: 1,
    generator: 'noevia-rs bins/noevia-server/tests/gdrive-tools-oracle.cjs (from noevia-core server/gdrive-tools.cjs, gdrive-files.cjs, gdrive-broker.cjs)',
    sources: Object.fromEntries(SOURCES.map((f) => [`server/${f}`, sha(f)])),
    coreRef: process.env.NOEVIA_CORE_REF || null,
    api: API, upload: UPLOAD, boundary: `noevia${BOUNDARY_BYTES.toString('hex')}`, states: STATES, cap: CAP,
    scenarios,
  };
  fs.writeFileSync(out, JSON.stringify(fixture, null, 1) + '\n');
})().catch((err) => { process.stderr.write(`${err.stack || err}\n`); process.exit(1); });
