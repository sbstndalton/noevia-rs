'use strict';
// Test-only Node oracle for server-export::route and the front's GET /api/export/conversations
// (#1315). Synthetic tenants only. For every case it lays the case's files out under a fresh
// UI_DATA_DIR and answers the request with core's own modules at the checkout given (CI: the
// pinned contracts/http/core.ref): workspace.cjs createWorkspaceStore with the M4 switches on (the
// condition the front requires), projects.cjs readHistory, diary-extras.cjs internalProject,
// routes/export.cjs and http.cjs, wired as server/index.cjs wires them. A throw becomes http.cjs
// errorResponse, as index.cjs answers it.
//
//   node crates/server-export/tests/route-oracle.cjs <core-checkout> <out.json>
//
// The output is deterministic (fixed clock); CI regenerates it and compares with the committed
// tests/fixtures/node-export-route.v1.json.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node route-oracle.cjs <core-checkout> <out.json>');
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');
const { createExportRoutes } = req('routes/export.cjs');
const { internalProject } = req('diary-extras.cjs');
const http = req('http.cjs');

const A = 'aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa';
const B = 'bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb';
const NOW = Date.UTC(2026, 9, 14, 12, 34, 56, 789);

// A file is a string (UTF-8 text), { hex } (raw bytes), { dir: true } (a directory), or
// { repeat, count, prefix, suffix } (prefix + repeat * count + suffix, for the size limits).
function bytesOf(spec) {
  if (typeof spec === 'string') return Buffer.from(spec, 'utf8');
  if (spec.hex !== undefined) return Buffer.from(spec.hex, 'hex');
  if (spec.repeat !== undefined) return Buffer.from((spec.prefix || '') + spec.repeat.repeat(spec.count) + (spec.suffix || ''), 'utf8');
  return null;
}
function layout(root, users) {
  for (const [user, files] of Object.entries(users)) {
    const dir = path.join(root, 'users', user);
    fs.mkdirSync(dir, { recursive: true });
    for (const [name, spec] of Object.entries(files)) {
      const target = path.join(dir, name);
      if (spec && spec.dir) fs.mkdirSync(target, { recursive: true });
      else fs.writeFileSync(target, bytesOf(spec));
    }
  }
}

// Another tenant's data is on disk in every case: it must never appear in A's export.
const OTHER = {
  'free-chats.json': JSON.stringify([{ id: 'c1', title: 'B SECRET TITLE' }, { id: 'b-only', title: 'B only' }]),
  'projects.json': JSON.stringify({ projects: [{ id: 'pb', name: 'B project', chats: [{ id: 'c2', title: 'B project chat' }] }] }),
  'history-c1.json': JSON.stringify({ history: [{ role: 'user', content: 'B SECRET CONTENT' }] }),
  'history-b-only.json': JSON.stringify({ history: [{ role: 'user', content: 'B SECRET CONTENT' }] }),
};

const cases = [];
function add(id, files, opts = {}) {
  cases.push({ id, files, user: opts.user || A, now: opts.now ?? NOW, ...(opts.missingDir ? { missingDir: true } : {}), ...(opts.refusal ? { refusal: opts.refusal } : {}) });
}
const hist = (h) => JSON.stringify({ history: h }, null, 2);
const turn = (q, a, extra = {}) => [
  { role: 'user', content: q, at: 1 },
  { role: 'assistant', content: a, reasoning: 'SECRET_REASONING', answers: ['old'], answer: 0, branches: [{ c: 1 }], branch: 1, toolCalls: [{ name: 'Read' }, null, { name: '' }, { name: 'Write' }], ...extra },
  { role: 'system', content: 'SECRET_SYSTEM' },
  { role: 'tool', content: 'tool output' },
];

add('no-files', {});
add('no-user-directory', {}, { missingDir: true });
add('free-chats-and-histories', {
  'free-chats.json': JSON.stringify([
    { id: 'c1', title: 'Hello world', updatedAt: 1800000000000, preview: 'p' },
    { id: 'c2', title: '', updatedAt: 0 },
    { id: 'c3', title: 'Hello world' },
  ], null, 2),
  'history-c1.json': hist(turn('What is 2+2?', '  4  ')),
  'history-c2.json': hist([{ role: 'user', content: '' }, { role: 'assistant', content: { a: 1 } }, { role: 'assistant', content: [1, [2, null], 'x'] }]),
  'history-c3.json': hist(turn('second', 'same title, so a -2 name')),
});
add('projects-and-internal-projects', {
  'free-chats.json': '[]',
  'projects.json': JSON.stringify({ projects: [
    { id: 'p1', name: 'Research & Notes', files: [null, { name: 'f' }], chats: [{ id: 'pc1', title: 'In a project', updatedAt: '2026-10-01T10:00:00.000Z' }, { id: 'pc2', title: 'In a project' }] },
    { id: 'cowork-diary-extras', name: 'Diary attachments', chats: [{ id: 'hidden1', title: 'HIDDEN' }] },
    { id: 'cowork-chat-context-abc', name: 'ctx', chats: [{ id: 'hidden2', title: 'HIDDEN' }] },
    { id: 'p2', name: '', chats: [] },
    { id: 'p-3!', name: '😀', chats: [{ id: 'pc3', title: 'x' }] },
    { id: '', name: '', chats: [{ id: 'pc4' }] },
  ] }, null, 2),
  'history-pc1.json': hist(turn('project q', 'project a')),
  'history-hidden1.json': hist([{ role: 'user', content: 'HIDDEN CONTENT' }]),
});
add('free-chats-not-a-list', { 'free-chats.json': JSON.stringify({ chats: [{ id: 'x' }] }) });
add('free-chats-null', { 'free-chats.json': 'null' });
add('projects-json-null', { 'projects.json': 'null' });
add('projects-null', { 'projects.json': JSON.stringify({ projects: null }) });
add('projects-file-is-a-list', { 'projects.json': '[1,2]' });
add('history-edge-reads', {
  'free-chats.json': JSON.stringify([
    { id: 'invalid-json' }, { id: 'null-json' }, { id: 'null-history' }, { id: 'is-a-dir' }, { id: 'no-file' },
    { id: '../x' }, { id: '..' }, { id: null }, { id: 12 }, { id: 'bom' }, { id: 'string-history' }, { id: 'zero-history' },
  ]),
  'history-invalid-json.json': '{"history": [',
  'history-null-json.json': 'null',
  'history-null-history.json': '{"history": null}',
  'history-is-a-dir.json': { dir: true },
  'history-x.json': hist([{ role: 'user', content: 'reached through ../x, reduced to x' }]),
  'history-12.json': hist([{ role: 'user', content: 'numeric id' }]),
  'history-bom.json': { hex: 'efbbbf' + Buffer.from(hist([{ role: 'user', content: 'BOM' }])).toString('hex') },
  'history-zero-history.json': '{"history": 0}',
});
add('lone-surrogates-and-invalid-utf8', {
  'free-chats.json': '[{"id":"s1","title":"cut \\ud83d","updatedAt":1800000000000},{"id":"s2","title":"bytes"}]',
  'history-s1.json': '{"history":[{"role":"user","content":"half \\ude00 emoji \\ud83d\\ude00 whole"},{"role":"assistant","content":"ok","reasoning":"\\ud800"}]}',
  'history-s2.json': { hex: Buffer.from('{"history":[{"role":"user","content":"bad ').toString('hex') + 'c328ff' + Buffer.from(' end"}]}').toString('hex') },
});
// 19,999 and 19,998 chats (+ README.md and conversations.json against the 20,000-entry limit);
// one id and title throughout, so the names run through Node's -2 ... -19998 collision search.
add('too-many-chats', { 'free-chats.json': { prefix: '[', repeat: '{"id":"c","title":"same"},', count: 19998, suffix: '{"id":"c","title":"same"}]' } });
add('just-enough-chats', { 'free-chats.json': { prefix: '[', repeat: '{"id":"c","title":"same"},', count: 19997, suffix: '{"id":"c","title":"same"}]' } });
add('too-many-bytes', {
  'free-chats.json': JSON.stringify([{ id: 'big1' }, { id: 'big2' }]),
  'history-big1.json': { prefix: '{"history":[{"role":"user","content":"', repeat: 'abcdefgh', count: 2 * 1024 * 1024, suffix: '"}]}' },
  'history-big2.json': { prefix: '{"history":[{"role":"user","content":"', repeat: 'abcdefgh', count: 2 * 1024 * 1024, suffix: '"}]}' },
});
add('free-chat-null-entry', { 'free-chats.json': '[null]' });
add('title-own-toString', { 'free-chats.json': '[{"id":"t","title":{"toString":1}}]' });
add('updatedAt-out-of-range', { 'free-chats.json': '[{"id":"d","updatedAt":1e20}]' });
add('project-null-entry', { 'projects.json': '{"projects":[null]}' });
add('project-numeric-id', { 'projects.json': '{"projects":[{"id":5,"name":"n","chats":[]}]}' });
add('project-without-id', { 'projects.json': '{"projects":[{"name":"n","chats":[]}]}' });
add('other-user-asks', { 'free-chats.json': JSON.stringify([{ id: 'a1', title: 'A SECRET TITLE' }]), 'history-a1.json': hist([{ role: 'user', content: 'A SECRET CONTENT' }]) }, { user: B });
// Narrower admission (route.rs): Node answers, the front refuses.
add('free-chats-invalid-json', { 'free-chats.json': '[{"id":' }, { refusal: 'Internal' });
add('projects-invalid-json', { 'projects.json': '{"projects":' }, { refusal: 'Internal' });
add('projects-not-a-list', { 'projects.json': '{"projects":{"0":{"id":"p"}}}' }, { refusal: 'Internal' });
add('project-chats-a-string', { 'projects.json': '{"projects":[{"id":"p","name":"n","chats":"ab"}]}' }, { refusal: 'Internal' });

function header(headers, name) {
  const key = Object.keys(headers || {}).find((k) => k.toLowerCase() === name);
  return key === undefined ? null : String(headers[key]);
}

async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-export-oracle-'));
  try {
    // B's files always; A's (the case's) unless the case has no directory for A.
    layout(root, { [B]: OTHER, ...(c.missingDir ? {} : { [A]: c.files }) });
    const store = createWorkspaceStore(root, { id: 'default', label: 'Default', baseUrl: 'http://127.0.0.1:9/v1', apiKey: '', shared: true }, null, { rustProjects: true, rustFreeChats: true });
    const warn = console.warn; console.warn = () => {};
    const res = { status: null, headers: null, body: null };
    res.writeHead = (status, headers) => { res.status = status; res.headers = headers; };
    res.end = (data) => { res.body = Buffer.isBuffer(data) ? data : Buffer.from(String(data ?? ''), 'utf8'); };
    const audits = [];
    try {
      let ws = null;
      const currentWorkspace = () => (ws ??= store.get(c.user));
      const projects = createProjectStore({ fs, path, currentWorkspace, PROJECTS: [], FREE_CHATS: [] });
      const routes = createExportRoutes({
        json: http.json,
        workspace: () => ({ freeChats: Array.from(currentWorkspace().freeChats), projects: currentWorkspace().projects.filter((proj) => !internalProject(proj)) }),
        readHistory: (id) => projects.readHistory(id),
        audit: (action, actor, detail) => audits.push([action, actor, actor, JSON.stringify(detail)]),
        now: () => c.now,
      });
      try { await routes({ method: 'GET' }, res, { path: '/api/export/conversations', authn: { user: { id: c.user } } }); }
      catch (err) { if (process.env.ORACLE_DEBUG) console.error(c.id, err); const r = http.errorResponse ? http.errorResponse(err) : { status: 500, body: { error: 'Internal error' } }; http.json(res, r.status, r.body); }
    } finally { console.warn = warn; }
    const zip = header(res.headers, 'content-type') === 'application/zip';
    return {
      status: res.status,
      headers: { 'content-type': header(res.headers, 'content-type'), 'content-disposition': header(res.headers, 'content-disposition'), 'cache-control': header(res.headers, 'cache-control'), 'content-length': header(res.headers, 'content-length') },
      ...(zip ? { zipSha256: crypto.createHash('sha256').update(res.body).digest('hex'), zipLength: res.body.length, ...(res.body.length <= 256 * 1024 ? { zip: res.body.toString('base64') } : {}) } : { body: res.body.toString('utf8') }),
      audits,
    };
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}

(async () => {
  const outCases = [];
  for (const c of cases) outCases.push({ ...c, node: await run(c) });
  fs.writeFileSync(out, JSON.stringify({ v: 1, users: { a: A, b: B }, other: OTHER, cases: outCases }, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
