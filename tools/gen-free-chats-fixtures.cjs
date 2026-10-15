#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own GET/POST /api/freechats (routes/chat-lists.cjs over
// projects.cjs, workspace.cjs, chat-lists.cjs, chat-framing.cjs, with index.cjs's arrayProxy) on
// synthetic workspaces and records each answer and the free-chats.json it leaves.
//
//   node tools/gen-free-chats-fixtures.cjs <noevia-core checkout> <output.json> [--cases <in.json>]
//
// crates/server-projects/tests/free_chats_differential.rs replays every case against the Rust port.
// With --cases the cases are read from a file instead (the generated ones of
// tests/free_chats_property.rs, whose shape is the same: name, ws, method, body, admin, now).
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const cp = require('node:child_process');
const crypto = require('node:crypto');
const { AsyncLocalStorage } = require('node:async_hooks');

const core = path.resolve(process.argv[2] || '../noevia-core');
const out = process.argv[3];
if (!out) throw Error('usage: gen-free-chats-fixtures.cjs <core> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const req = (m) => require(path.join(core, 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');
const { createChatListRoutes } = req('routes/chat-lists.cjs');
const http = req('http.cjs');

const USER = '11111111-2222-4333-8444-555555555555';
const NOW = 1700000000123;
const pretty = (v) => JSON.stringify(v, null, 2);

// ── Synthetic workspaces ────────────────────────────────────────────────────
const frame = { kind: 'idea', tags: ['alpha'], links: [], confirmed: true, source: 'user' };
const projects = {
  projects: [
    { id: 'proj-a', name: 'Alpha', modes: ['chat'], chats: [{ id: 'pa1', title: 'In A', updatedAt: 9 }, null, { id: 7 }, 'stray'] },
    { id: 'proj-b', name: 'Beta', modes: ['chat'], chats: 'pb1' },
    { id: 'cowork-chat-context-x1', name: 'Chat attachments x1', modes: ['chat'], chats: [{ id: 'ctx-held', updatedAt: 1 }] },
    { name: 'No id', modes: ['chat'], chats: [{ id: 'noid-held', updatedAt: 1 }] },
    { id: null, name: 'Null id', modes: ['chat'], chats: [{ id: 'nullid-held', updatedAt: 1 }] },
    { id: 'proj-empty', name: 'Empty', modes: ['chat'] },
  ],
};
const free = [
  { id: 'f1', title: 'One', updatedAt: 300, preview: 'p1', pinned: false, archived: false, frame, forceLocal: true, createdAt: 100 },
  { id: 'f2', title: 'Two', updatedAt: 200, preview: '', pinned: true, archived: false },
  { id: 'dead-1', title: 'Deleted', updatedAt: 250, preview: '' },
  { id: 'tie-1', title: 'Tie', updatedAt: 200, preview: '' },
  { id: 'nul', title: 'Null time', updatedAt: null, preview: '' },
  'stray-string', null, { title: 'no id' },
];
const base = { projects: pretty(projects), free: pretty(free), tombstones: JSON.stringify(['dead-1', 5, 'gone-chat']) };
const withFree = (text) => ({ ...base, free: text });
const withProjects = (mut) => { const p = JSON.parse(JSON.stringify(projects)); mut(p); return { ...base, projects: pretty(p) }; };

const cases = [];
const add = (name, ws, request) => cases.push({ name, ws, admin: true, now: NOW, ...request });
const post = (name, ws, body, extra = {}) => add(name, ws, { method: 'POST', body: typeof body === 'string' ? body : JSON.stringify(body), ...extra });
const get = (name, ws) => add(name, ws, { method: 'GET', body: null });

// GET: the stored value as Node's array proxy serialises it
get('get-list', base);
get('get-no-files', { projects: null, free: null, tombstones: null });
get('get-empty', withFree('[]'));
get('get-invalid', withFree('{oops'));
get('get-string', withFree('"ab\\ud83d\\ude00\\udc00z"'));
get('get-number', withFree('5'));
get('get-true', withFree('true'));
get('get-object', withFree('{"a":1}'));
get('get-object-length', withFree('{"length":2,"0":"x"}'));
get('get-null', withFree('null'));
get('get-lone-surrogates', withFree(pretty(free).replace('"p1"', '"p\\ud83d"').replace('"One"', '"\\udc00One"')));
get('get-integer-keys', withFree('[{"id":"ik","b":1,"2":"two","1":"one"}]'));
get('get-projects-null-entry', withProjects((p) => { p.projects.push(null); }));
get('get-projects-number-entry', withProjects((p) => { p.projects.push(5); }));
get('get-projects-file-null', { ...base, projects: 'null' });
get('get-projects-invalid', { ...base, projects: '{oops' });
get('get-projects-object', { ...base, projects: '{"projects":{}}' });
get('get-projects-zero', { ...base, projects: '{"projects":0}' });

// POST: the merge
post('post-add', base, { chats: [{ id: 'n1', title: 'New', updatedAt: 400, preview: 'hello', createdAt: 5 }] });
post('post-keeps-stored-fields', base, { chats: [{ id: 'f1', title: 'One again', updatedAt: 500, preview: 'x' }] });
post('post-frame-null-clears', base, { chats: [{ id: 'f1', title: 'One', updatedAt: 300, frame: null }] });
post('post-frame-invalid-keeps', base, { chats: [{ id: 'f1', title: 'One', updatedAt: 300, frame: { kind: 'bogus' } }] });
post('post-frame-invalid-new', base, { chats: [{ id: 'f2', updatedAt: 1, frame: [1] }] });
post('post-frame-normalised', base, { chats: [{ id: 'f2', updatedAt: 210, frame: { kind: 'code', tags: [' ##a  b　c ', 'a-b-c', 7, '#', ''], links: ['x', 'x', '', 3, 'y'.repeat(129)], projectId: 'proj-b', confirmed: 'yes', source: 'other', extra: 1 } }] });
post('post-flags', base, { chats: [{ id: 'f1', updatedAt: 300, forceLocal: false, allowCloud: 'yes' }, { id: 'f2', updatedAt: 200, allowCloud: true, forceLocal: 0 }] });
post('post-created-at', base, { chats: [{ id: 'f1', updatedAt: 300, createdAt: 999 }, { id: 'n1', updatedAt: 1, createdAt: -0 }, { id: 'n2', updatedAt: 2, createdAt: '7' }] });
post('post-sanitises', base, { chats: [
  { id: 'c 4/../x', title: { a: 1 }, preview: ['p', null, 3], updatedAt: '9' },
  { id: '%%%', title: 'gone' }, 5, 'str', null, [], { title: 'no id' }, { id: 12 },
  { id: 'x'.repeat(100), title: 't'.repeat(130), preview: 'p'.repeat(250), pinned: 'true', archived: true },
  { id: 'empty-title', title: '', preview: 0, updatedAt: -0 },
] });
post('post-title-throws', base, { chats: [{ id: 'bad', title: { toString: 1 } }] });
post('post-updated-now', base, { chats: [{ id: 'later', title: 'Now' }] });
post('post-elsewhere-skipped', base, { chats: [{ id: 'pa1', updatedAt: 1 }, { id: 'ctx-held', updatedAt: 1 }, { id: 'noid-held', updatedAt: 1 }, { id: 'pa1', updatedAt: 2 }, { id: 'nullid-held', updatedAt: 3 }, { id: 'c9', updatedAt: 3 }] });
post('post-tombstoned', base, { chats: [{ id: 'dead-1', updatedAt: 999 }, { id: 'gone-chat', updatedAt: 999 }] });
post('post-duplicates', base, { chats: [{ id: 'd1', title: 'first', updatedAt: 5, frame: { kind: 'idea' }, createdAt: 3 }, { id: 'd1', title: 'second', updatedAt: 6 }] });
post('post-mode-admin', base, { chats: [{ id: 'cw', updatedAt: 7, mode: 'cowork' }, { id: 'cx', updatedAt: 8, mode: 'code' }] });
add('post-mode-member', base, { method: 'POST', admin: false, body: JSON.stringify({ chats: [{ id: 'cw', updatedAt: 7, mode: 'cowork' }] }) });
post('post-empty-list', base, { chats: [] });
post('post-cap', base, { chats: Array.from({ length: 1005 }, (_, i) => ({ id: `m${i}`, updatedAt: i % 7, title: `m${i}` })) });
post('post-lone-surrogates', base, `{"chats":[{"id":"s1","title":"${'t'.repeat(119)}\\ud83d\\ude00","preview":"\\udc00p\\ud800","updatedAt":3,"frame":{"kind":"idea","tags":["\\ud83d"]}}]}`);
post('post-emoji-cut', base, { chats: [{ id: 's2', title: `${'t'.repeat(119)}😀`, preview: `${'p'.repeat(199)}😀x`, updatedAt: 4 }] });
post('post-keeps-stored-lone-surrogate', withFree(pretty(free).replace('"p1"', '"p\\ud83d"')), { chats: [{ id: 'f2', updatedAt: 201 }] });
post('post-integer-keys', withFree('[{"id":"ik","title":"k","updatedAt":1,"2":"b","1":"a"}]'), { chats: [{ id: 'ik', updatedAt: 2 }] });

// POST: the refusals and their order
for (const [n, body] of [['invalid', '{'], ['empty', ''], ['null', 'null'], ['array', '[]'], ['string', '"x"'], ['number', '5'], ['chats-object', '{"chats":{}}'], ['no-chats', '{}'], ['chats-null', '{"chats":null}'], ['bom', '﻿{"chats":[]}'], ['trailing', '{"chats":[]} x'], ['infinity', '{"chats":[{"id":"i","updatedAt":1e400}]}']]) {
  post(`post-body-${n}`, base, body);
}
add('post-over-limit', base, { method: 'POST', bodyRepeat: { text: 'x', count: 1024 * 1024 + 1 } });
add('post-over-limit-projects-null-entry', withProjects((p) => { p.projects.push(null); }), { method: 'POST', bodyRepeat: { text: 'x', count: 1024 * 1024 + 1 } });
{
  // Exactly readBody's limit, valid JSON.
  const head = '{"chats":[{"id":"big","updatedAt":1,"preview":"', tail = '"}]}';
  add('post-at-limit', base, { method: 'POST', bodyRepeat: { head, text: 'p', count: 1024 * 1024 - head.length - tail.length, tail } });
}
post('post-no-files', { projects: null, free: null, tombstones: null }, { chats: [{ id: 'first', updatedAt: 1 }] });
post('post-free-invalid', withFree('{oops'), { chats: [{ id: 'a', updatedAt: 1 }] });
post('post-free-object', withFree('{"a":1}'), { chats: [{ id: 'a', updatedAt: 1 }] });
post('post-free-null', withFree('null'), { chats: [{ id: 'a', updatedAt: 1 }] });
post('post-free-string', withFree('"f1"'), { chats: [{ id: 'a', updatedAt: 1 }] });
post('post-free-number', withFree('5'), { chats: [{ id: 'a', updatedAt: 1 }] });
post('post-free-bad-body-first', withFree('null'), '{');
post('post-tombstones-object', { ...base, tombstones: '{"dead-1":1}' }, { chats: [{ id: 'dead-1', updatedAt: 1 }] });
post('post-tombstones-invalid', { ...base, tombstones: '[' }, { chats: [{ id: 'dead-1', updatedAt: 1 }] });
post('post-other-chats-object', withProjects((p) => { p.projects[0].chats = { id: 'x' }; }), { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-other-chats-number', withProjects((p) => { p.projects[0].chats = 3; }), { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-projects-null-entry', withProjects((p) => { p.projects.push(null); }), { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-projects-string-entry', withProjects((p) => { p.projects.push('s'); }), { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-projects-array-entry', withProjects((p) => { p.projects.push([]); }), { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-projects-file-null', { ...base, projects: 'null' }, { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-projects-invalid', { ...base, projects: '{oops' }, { chats: [{ id: 'pa1', updatedAt: 1 }] });
post('post-projects-string', { ...base, projects: '{"projects":"ab"}' }, { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-projects-zero', { ...base, projects: '{"projects":0}' }, { chats: [{ id: 'pa1', updatedAt: 1 }] });
post('post-stored-updated-true', withFree('[{"id":"bt","updatedAt":true},{"id":"b0","updatedAt":0.5}]'), { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-stored-updated-string', withFree('[{"id":"bs","updatedAt":"x"},{"id":"b0","updatedAt":2}]'), { chats: [{ id: 'q', updatedAt: 1 }] });

// ── Node, one fresh workspace per case ──────────────────────────────────────
async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'free-chats-fixture-'));
  const dir = path.join(root, 'users', USER);
  fs.mkdirSync(dir, { recursive: true });
  if (c.ws.projects !== null) fs.writeFileSync(path.join(dir, 'projects.json'), c.ws.projects);
  if (c.ws.free !== null) fs.writeFileSync(path.join(dir, 'free-chats.json'), c.ws.free);
  if (c.ws.tombstones !== null) fs.writeFileSync(path.join(dir, 'deleted-chats.json'), c.ws.tombstones);
  const file = path.join(dir, 'free-chats.json');
  const before = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null;
  let status = null, text = null;
  const res = { writeHead(code) { status = code; }, setHeader() {}, end(v) { text = String(v); } };
  // index.cjs: the request's workspace is loaded (store.get) before any route runs; a throw there
  // is the request's error answer.
  const scope = new AsyncLocalStorage();
  const currentWorkspace = () => { const w = scope.getStore()?.workspace; if (!w) throw new Error('authenticated workspace context required'); return w; };
  // index.cjs arrayProxy, verbatim.
  const arrayProxy = (field) => new Proxy([], {
    get(_target, property) {
      const value = currentWorkspace()[field][property];
      return typeof value === 'function' ? value.bind(currentWorkspace()[field]) : value;
    },
    set(_target, property, value) { currentWorkspace()[field][property] = value; return true; },
    ownKeys() { return Reflect.ownKeys(currentWorkspace()[field]); },
    getOwnPropertyDescriptor() { return { enumerable: true, configurable: true }; },
  });
  const PROJECTS = arrayProxy('projects'), FREE_CHATS = arrayProxy('freeChats');
  const projectStore = createProjectStore({
    fs, path, currentWorkspace, PROJECTS, FREE_CHATS, sanitizeToolboxes: () => null, defaultToolboxes: () => ['core'],
    reasoningEffort: {}, projectAppearance: () => ({}), rag: {}, storageClient: {}, documentSources: {}, authService: {},
    PROJECT_ROOT_FOLDER: 'Projects', createProjectFolder: async () => null, projectSweep: { afterDelete: async () => [] },
  });
  const routes = createChatListRoutes({
    json: http.json, readBody: http.readBody, currentWorkspace, PROJECTS, FREE_CHATS, diaryExtras: { internalProject: () => false }, crypto,
    STORED_HISTORY_BYTES: 1, STORED_HISTORY_CAP: 1, chatLists: () => ({ freeChats: [], projects: [] }), removeChat: () => false, store: projectStore,
  });
  const r = c.bodyRepeat;
  const body = r ? (r.head || '') + r.text.repeat(r.count) + (r.tail || '') : c.body;
  const request = { method: c.method, headers: {}, async *[Symbol.asyncIterator]() { if (body !== null && body !== '') yield Buffer.from(body, 'utf8'); } };
  const p = '/api/freechats';
  const authn = { user: { id: USER, role: c.admin ? 'admin' : 'member' } };
  try {
    const workspace = createWorkspaceStore(root, { id: 'local', label: 'Local', baseUrl: '', apiKey: '' }, null).get(USER);
    await scope.run({ workspace }, async () => {
      const handled = await routes(request, res, { path: p, authn, url: new URL(`http://h${p}`) });
      if (!handled) throw Error(`${c.name}: not handled`);
    });
  } catch (err) {
    const failure = http.errorResponse(err);
    http.json(res, failure.status, failure.body);
  }
  const after = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null;
  fs.rmSync(root, { recursive: true, force: true });
  return { status, body: text, freeAfter: after === before ? null : after };
}

(async () => {
  const results = [];
  const external = process.argv[4] === '--cases' ? JSON.parse(fs.readFileSync(process.argv[5], 'utf8')) : null;
  for (const c of external || cases) {
    // Date.now() is the case's own clock for the whole request.
    const realNow = Date.now;
    Date.now = () => c.now;
    try { results.push({ ...c, expected: await run(c) }); } finally { Date.now = realNow; }
  }
  // The major only, so CI's regenerate-and-compare does not depend on the runner's Node patch level.
  const node = `v${process.versions.node.split('.')[0]}`;
  fs.writeFileSync(out, `${JSON.stringify({ core: sha, node, user: USER, cases: results }, null, 1)}\n`);
  console.log(`free chats fixtures=${results.length} core=${sha} node=${process.version}`);
})().catch((e) => { console.error(e); process.exit(1); });
