'use strict';
// Test-only Node oracle for the front's GET /api/chats/search (#1450) and GET /api/chats/{chatId}/export
// (#1471). Synthetic tenants only. For every case it lays the case's files out under a fresh UI_DATA_DIR and
// answers the request with core's own modules at the checkout given (CI: the ref in
// tests/fixtures/chat-lists-oracle.core-ref, the last core whose routes/chat-lists.cjs answers both):
// workspace.cjs createWorkspaceStore (the real load of projects.json and free-chats.json), projects.cjs
// readHistory, diary-extras.cjs, routes/chat-lists.cjs and http.cjs, wired as server/index.cjs wires them. A
// throw becomes http.cjs errorResponse, as index.cjs answers it.
//
//   node bins/noevia-server/tests/chat-lists-oracle.cjs <core-checkout> <out.json>
//
// The output is deterministic; CI regenerates it and compares with the committed
// tests/fixtures/node-chat-lists.v1.json, which tests/chat_lists_read.rs replays through the front.
// Local-time Date strings read in the process zone; the front's tests run in UTC as CI does.
process.env.TZ = 'UTC';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node chat-lists-oracle.cjs <core-checkout> <out.json>');
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');
const { createChatListRoutes } = req('routes/chat-lists.cjs');
const diaryExtras = req('diary-extras.cjs');
const http = req('http.cjs');

const A = 'aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa';
const B = 'bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb';

// A file is a string (UTF-8 text), { hex } (raw bytes) or { dir: true } (a directory).
function bytesOf(spec) {
  if (typeof spec === 'string') return Buffer.from(spec, 'utf8');
  if (spec.hex !== undefined) return Buffer.from(spec.hex, 'hex');
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

const hist = (h) => JSON.stringify({ history: h });
const said = (text) => [{ role: 'user', content: text }];
// Another tenant's data under the same ids, in every case: it must never appear in A's answers.
const OTHER = {
  'free-chats.json': JSON.stringify([{ id: 'f1', title: 'B SECRET TITLE' }, { id: 'b-only', title: 'B only' }]),
  'projects.json': JSON.stringify({ projects: [{ id: 'pb', name: 'B project', chats: [{ id: 'p1', title: 'B project chat' }] }] }),
  'history-f1.json': hist(said('B SECRET CONTENT needle')),
  'history-b-only.json': hist(said('B SECRET CONTENT needle')),
  'history-p1.json': hist(said('B SECRET CONTENT needle')),
};

const cases = [];
function add(id, files, urlPath, opts = {}) {
  cases.push({ id, files, path: urlPath, ...(opts.refusal ? { refusal: opts.refusal } : {}) });
}
const search = (q) => `/api/chats/search${q === undefined ? '' : `?q=${q}`}`;
const enc = encodeURIComponent;

// ── GET /api/chats/search ────────────────────────────────────────────────────────────────────────
const LISTS = {
  'free-chats.json': JSON.stringify([
    { id: 'f1', title: 'Free one' }, null, 'f-str', { id: 7 }, { title: 'no id' },
    { id: 'f2', title: 'Free two' }, { id: 'dead', title: 'Deleted' }, { id: 'f3' }, { id: '..' },
  ]),
  'projects.json': JSON.stringify({ projects: [
    { id: 'p', name: 'Project', chats: [{ id: 'p1', title: 'In a project' }, { id: 'f2', title: 'Twice' }, null] },
    { id: 'cowork-diary-extras', name: 'Diary', chats: [{ id: 'hidden1' }] },
    { id: 'cowork-chat-context-x', name: 'ctx', chats: [{ id: 'hidden2' }] },
    { id: 'empty', name: 'Empty' },
    { id: 'falsy', name: 'Falsy', chats: 0 },
    { id: 'str', name: 'String chats', chats: 'ab' },
    { id: 'q', name: 'Q', chats: [{ id: 'q1' }] },
  ] }),
  'deleted-chats.json': JSON.stringify(['dead', 12, null]),
  'history-f1.json': hist([{ role: 'user', content: 'The Needle in a haystack' }, { role: 'assistant', content: 'found it' }]),
  'history-f2.json': hist([{ role: 'system', content: 'needle' }, { role: 'tool', content: 'needle' }, { role: 'assistant', content: ['needle'] }, { role: 'assistant', content: 'NEEDLE again' }]),
  'history-dead.json': hist(said('needle in a deleted chat')),
  'history-f3.json': hist([{ role: 'system', content: 'needle only in system' }, null, 5, { role: 'user' }]),
  'history-p1.json': hist([{ role: 'assistant', content: 'a project needle' }]),
  'history-hidden1.json': hist(said('needle')),
  'history-hidden2.json': hist(said('needle')),
  'history-q1.json': '{"history":',
};
add('search-no-query', LISTS, search());
add('search-empty', LISTS, search(''));
add('search-one-letter', LISTS, search('n'));
add('search-one-letter-after-trim', LISTS, search(enc('  n \t')));
add('search-matches-in-list-order', LISTS, search('needle'));
add('search-trimmed-unicode-space', LISTS, search(enc('　﻿needle ')));
add('search-plus-is-space', LISTS, search('a+project'));
add('search-first-q-only', LISTS, search('needle&q=haystack'));
add('search-malformed-escape', { 'free-chats.json': JSON.stringify([{ id: 'm' }]), 'history-m.json': hist(said('x�%A y')) }, search('%E0%A4%A'));
add('search-no-match', LISTS, search('nothing-matches-this'));
add('search-200-units', LISTS, search(enc('😀'.repeat(100))));
add('search-201-units', LISTS, search(enc('😀'.repeat(100) + 'a')));
add('search-201-ascii', LISTS, search('a'.repeat(201)));
add('search-201-before-trim', LISTS, search(enc(` ${'a'.repeat(199)} `)));
add('search-other-tenant-never', { 'free-chats.json': '[]' }, search('needle'));
add('search-no-files', {}, search('needle'));
add('search-lists-not-json', { 'free-chats.json': '[{', 'projects.json': '{"projects":', 'history-f1.json': hist(said('needle')) }, search('needle'));
add('search-tombstones-not-json', { 'free-chats.json': JSON.stringify([{ id: 'f1' }]), 'deleted-chats.json': '[', 'history-f1.json': hist(said('needle')) }, search('needle'));
add('search-history-edge-reads', {
  'free-chats.json': JSON.stringify([{ id: 'nj' }, { id: 'nul' }, { id: 'nh' }, { id: 'dir' }, { id: 'zero' }, { id: 'ok' }]),
  'history-nj.json': 'not json', 'history-nul.json': 'null', 'history-nh.json': '{"history":null}', 'history-dir.json': { dir: true },
  'history-zero.json': '{"history":0}', 'history-ok.json': '﻿' + hist(said('needle')),
}, search('needle'));
add('search-history-a-string', { 'free-chats.json': JSON.stringify([{ id: 's' }]), 'history-s.json': '{"history":"needle"}' }, search('needle'));
add('search-history-an-object', { 'free-chats.json': JSON.stringify([{ id: 's' }]), 'history-s.json': '{"history":{"0":{"role":"user","content":"needle"}}}' }, search('needle'));
add('search-project-chats-an-object', { 'projects.json': JSON.stringify({ projects: [{ id: 'p', chats: { id: 'x' } }] }) }, search('needle'));
add('search-project-null-entry', { 'projects.json': '{"projects":[null]}' }, search('needle'));
add('search-project-numeric-id', { 'projects.json': '{"projects":[{"id":5,"chats":[]}]}' }, search('needle'));
// Case folding as JS toLowerCase: dotted capital I, sharp s, final sigma, the Kelvin sign.
const FOLD = {
  'free-chats.json': JSON.stringify(['i1', 'i2', 's1', 's2', 'g1', 'g2', 'k1'].map((id) => ({ id }))),
  'history-i1.json': hist(said('İSTANBUL')), 'history-i2.json': hist(said('istanbul')),
  'history-s1.json': hist(said('STRASSE')), 'history-s2.json': hist(said('Straße')),
  'history-g1.json': hist(said('ΣΟΦΟΣ')), 'history-g2.json': hist(said('σοφοσ')),
  'history-k1.json': hist(said('5 K')),
};
for (const [name, q] of [['dotted-i', 'İst'], ['plain-i', 'ist'], ['sharp-s', 'ß'], ['ss', 'SS'], ['final-sigma', 'ΟΣ'], ['medial-sigma', 'σοφ'], ['sigma-word', 'σοφοσ'], ['kelvin', '5 k']]) {
  add(`search-fold-${name}`, FOLD, search(enc(q)));
}
add('search-lone-surrogate', { 'free-chats.json': JSON.stringify([{ id: 'u' }]), 'history-u.json': '{"history":[{"role":"user","content":"\\ud800needle"}]}' }, search('needle'));
// The bounds: 100 matches, 500 chats read, tombstoned chats not counted.
const many = (n, { match = () => false, dead = 0 } = {}) => {
  const files = {};
  const chats = [];
  for (let i = 0; i < n + dead; i++) {
    const id = `c${i}`;
    chats.push({ id });
    if (match(i)) files[`history-${id}.json`] = hist(said(`needle ${i}`));
  }
  files['free-chats.json'] = JSON.stringify(chats);
  if (dead) files['deleted-chats.json'] = JSON.stringify(chats.slice(0, dead).map((c) => c.id));
  return files;
};
add('search-100-matches', many(100, { match: () => true }), search('needle'));
add('search-101-matches', many(101, { match: () => true }), search('needle'));
add('search-500-read', many(500, { match: (i) => i === 499 }), search('needle'));
add('search-501-read', many(501, { match: (i) => i === 500 }), search('needle'));
add('search-500-read-after-tombstones', many(500, { match: (i) => i === 509, dead: 10 }), search('needle'));

// ── GET /api/chats/{chatId}/export ───────────────────────────────────────────────────────────────
const turn = (q, a, extra = {}) => [
  { role: 'user', content: q, at: 1 },
  { role: 'assistant', content: a, reasoning: 'SECRET_REASONING', toolCalls: [{ name: 'Read' }, null, { name: '' }, { name: 'Write' }], ...extra },
  { role: 'system', content: 'SECRET_SYSTEM' },
  { role: 'tool', content: 'tool output' },
];
const EXP = {
  'free-chats.json': JSON.stringify([
    { id: 'f1', title: 'Café Ünïcode — notes', updatedAt: 1800000000000 },
    { id: 'f2', title: '' }, { id: 'f3', title: '!!!' }, { id: 'f4', title: 42 }, { id: 'f5' },
    { id: 'f6', title: `${'Long title words '.repeat(5)}end` }, { id: 'f7', title: 'ﬁle №5 日本語 😀' },
    { id: 'f8', title: ['a', 'b'] }, { id: 'dead', title: 'Deleted' }, { id: 'c1', title: 'Spaced id' }, { id: 'a.b', title: 'Dotted' },
    { id: 'fail', title: 'Failed reply' }, { id: 'nohist', title: 'No transcript' }, { id: 'lone', title: '\ud800 lone' },
  ]),
  'projects.json': JSON.stringify({ projects: [
    { id: 'cowork-diary-extras', name: 'Diary', chats: [{ id: 'hidden1', title: 'HIDDEN' }] },
    { id: 'cowork-chat-context-abc', name: 'ctx', chats: [{ id: 'hidden2', title: 'HIDDEN' }] },
    { id: 'p', name: 'Project', chats: [null, { id: 'p1', title: 'In a project' }, { id: 'pdead', title: 'Deleted in a project' }] },
    { id: 'p2', name: 'Second', chats: [{ id: 'p1', title: 'Shadowed' }, { id: 'ab', title: 'Reduced' }] },
  ] }),
  'deleted-chats.json': JSON.stringify(['dead', 'pdead']),
  'history-f1.json': hist(turn('What is 2+2?', '  4  ')),
  'history-f2.json': hist(turn('Untitled?', 'yes')),
  'history-p1.json': hist(turn('Project question', 'Project answer')),
  'history-c1.json': hist(said('spaced')),
  'history-ab.json': hist(said('dotted')),
  'history-dead.json': hist(said('deleted')),
  'history-fail.json': hist([{ role: 'user', content: 'Q' }, { role: 'assistant', content: 'partial text', failure: 'The model stopped answering.' }, { role: 'assistant', content: '', failure: '   ' }]),
  'history-lone.json': '{"history":[{"role":"user","content":"\\udc00 x"}]}',
  'history-hidden1.json': hist(said('hidden')),
};
const exp = (id) => `/api/chats/${id}/export`;
for (const id of ['f1', 'f2', 'f3', 'f4', 'f5', 'f6', 'f7', 'f8', 'p1', 'fail', 'nohist', 'lone', 'c%201', 'a.b', 'ab']) add(`export-${id.replace(/[^a-z0-9]/gi, '_')}`, EXP, exp(id));
for (const id of ['dead', 'pdead', 'hidden1', 'hidden2', 'nope', 'b-only']) add(`export-404-${id}`, EXP, exp(id));
for (const [name, id] of [['bad-escape', '%E0%A4%A'], ['space', '%20'], ['symbols', '!!'], ['percent', '%']]) add(`export-400-${name}`, EXP, exp(id));
add('export-no-files', {}, exp('f1'));
add('export-lists-not-json', { 'free-chats.json': '[{', 'projects.json': '{"projects":', 'history-f1.json': hist(said('x')) }, exp('f1'));
add('export-tombstones-not-json', { 'free-chats.json': JSON.stringify([{ id: 'f1', title: 'T' }]), 'deleted-chats.json': '{', 'history-f1.json': hist(said('x')) }, exp('f1'));
add('export-history-not-json', { 'free-chats.json': JSON.stringify([{ id: 'f1', title: 'T' }]), 'history-f1.json': 'nope' }, exp('f1'));
add('export-history-a-dir', { 'free-chats.json': JSON.stringify([{ id: 'f1', title: 'T' }]), 'history-f1.json': { dir: true } }, exp('f1'));
add('export-history-a-string', { 'free-chats.json': JSON.stringify([{ id: 'f1', title: 'T' }]), 'history-f1.json': '{"history":"text"}' }, exp('f1'));
add('export-project-null-entry-free-hit', { 'free-chats.json': JSON.stringify([{ id: 'f1', title: 'T' }]), 'projects.json': '{"projects":[null]}' }, exp('f1'));
add('export-project-numeric-id-free-hit', { 'free-chats.json': JSON.stringify([{ id: 'f1', title: 'T' }]), 'projects.json': '{"projects":[{"id":5,"chats":[]}]}' }, exp('f1'));
add('export-project-numeric-id-walked', { 'projects.json': '{"projects":[{"id":5,"chats":[{"id":"f1"}]}]}' }, exp('f1'));
add('export-project-chats-a-string', { 'projects.json': '{"projects":[{"id":"p","chats":"f1"}]}' }, exp('f1'));
add('export-project-chats-falsy-then-hit', { 'projects.json': '{"projects":[{"id":"p","chats":0},{"id":"q","chats":[{"id":"f1","title":"Q"}]}]}' }, exp('f1'));
add('export-tombstoned-skips-bad-projects', { 'projects.json': '{"projects":[{"id":"p","chats":"x"}]}', 'deleted-chats.json': '["f1"]' }, exp('f1'));
// Review of S2: dates are `new Date(x).toISOString()` over the whole range, and only the fields
// chatMarkdown reads are bounded (an ignored field may nest deeper than the workspace export admits).
const dated = (updatedAt) => ({ 'free-chats.json': JSON.stringify([{ id: 'd', title: 'T', updatedAt }]), 'history-d.json': hist(said('x')) });
for (const [name, v] of [
  ['extended-year', 253402300800000], ['negative-extended-year', -62198755200001], ['max', 8.64e15], ['past-max', 8.64e15 + 1],
  ['fraction', 1.9], ['true', true], ['date-only', '2026-01-01'], ['year-only', '2026'], ['plus-year', '+010000-01-01'],
  ['minus-zero-year', '-000000-01-01'], ['offset', '2026-01-01T10:00:00.1234+01:30'], ['hour-24', '2026-01-01T24:00Z'],
  ['local-iso', '2026-01-01T10:00'], ['rfc', 'Thu, 01 Jan 2026 10:00:00 GMT'], ['legacy-pm', 'Jan 1 2026 10:00 PM EST'],
  ['local-month-day', 'Jan 5'], ['local-slashes', '1/2/2026'], ['overflow-day', '2026-02-31'], ['words', 'soon'], ['array', [2026, 1, 1]],
  ['object', { a: 1 }], ['own-toString', { toString: 1 }],
]) add(`export-date-${name}`, dated(v), exp('d'));
let deep = null;
for (let i = 0; i < 40; i++) deep = { x: deep };
add('export-deep-ignored-metadata', {
  'free-chats.json': JSON.stringify([{ id: 'm', title: 'T', meta: deep }]),
  'history-m.json': hist([{ role: 'user', content: 'needle', meta: deep }]),
}, exp('m'));
add('export-title-own-toString', { 'free-chats.json': '[{"id":"t","title":{"toString":1}}]' }, exp('t'));

function header(headers, name) {
  const key = Object.keys(headers || {}).find((k) => k.toLowerCase() === name);
  return key === undefined ? null : String(headers[key]);
}

async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-chat-lists-oracle-'));
  try {
    layout(root, { [B]: OTHER, [A]: c.files });
    // The front's project store writes; nothing here writes, so an unused client stands in for it.
    const store = createWorkspaceStore(root, { id: 'default', label: 'Default', baseUrl: 'http://127.0.0.1:9/v1', apiKey: '', shared: true }, null, { projectStore: {} });
    const warn = console.warn; console.warn = () => {};
    const res = { status: null, headers: null, body: null };
    res.writeHead = (status, headers) => { res.status = status; res.headers = headers; };
    res.end = (data) => { res.body = Buffer.isBuffer(data) ? data : Buffer.from(String(data ?? ''), 'utf8'); };
    let handled = null;
    try {
      const url = new URL(c.path, 'http://localhost');
      try {
        const ws = store.get(A);
        const currentWorkspace = () => ws;
        const projects = createProjectStore({ fs, path, currentWorkspace, PROJECTS: ws.projects, FREE_CHATS: ws.freeChats });
        const routes = createChatListRoutes({
          json: http.json, readBody: async () => '', currentWorkspace, PROJECTS: ws.projects, FREE_CHATS: ws.freeChats,
          diaryExtras, STORED_HISTORY_BYTES: 1024 * 1024, store: projects,
        });
        handled = await routes({ method: 'GET', url: c.path, headers: {} }, res, { path: url.pathname, authn: { user: { id: A, role: 'member' } }, url });
      } catch (err) {
        if (process.env.ORACLE_DEBUG) console.error(c.id, err);
        const r = http.errorResponse(err);
        http.json(res, r.status, r.body);
        handled = true;
      }
    } finally { console.warn = warn; }
    if (!handled) throw new Error(`${c.id}: core did not answer ${c.path}`);
    return {
      status: res.status,
      headers: Object.fromEntries(['content-type', 'content-disposition', 'cache-control', 'content-length'].map((h) => [h, header(res.headers, h)])),
      body: res.body.toString('base64'),
    };
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}

(async () => {
  const outCases = [];
  for (const c of cases) outCases.push({ ...c, node: await run(c) });
  fs.writeFileSync(out, JSON.stringify({ v: 1, users: { a: A, b: B }, other: OTHER, cases: outCases }, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
