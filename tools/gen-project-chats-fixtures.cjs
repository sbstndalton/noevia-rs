#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own GET/POST /api/projects/{id}/chats (routes/projects.cjs
// over projects.cjs, workspace.cjs, chat-lists.cjs, chat-framing.cjs, instruction-skills.cjs) on
// synthetic workspaces and records each answer and the projects.json it leaves.
//
//   node tools/gen-project-chats-fixtures.cjs <noevia-core checkout> <output.json> [--cases <in.json>]
//
// crates/server-projects/tests/chats_differential.rs replays every case against the Rust port.
// With --cases the cases are read from a file instead (the generated ones of
// tests/chats_property.rs, whose shape is the same: name, ws, method, project, body, admin, now).
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const cp = require('node:child_process');

const core = path.resolve(process.argv[2] || '../noevia-core');
const out = process.argv[3];
if (!out) throw Error('usage: gen-project-chats-fixtures.cjs <core> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const req = (m) => require(path.join(core, 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');
const { createProjectRoutes } = req('routes/projects.cjs');
const http = req('http.cjs');

const USER = '11111111-2222-4333-8444-555555555555';
const NOW = 1700000000123;
const pretty = (v) => JSON.stringify(v, null, 2);

// ── Synthetic workspaces ────────────────────────────────────────────────────
// sbstndalton/noevia#1806: a source's text is in its own file (core source-text.cjs); projects.json
// holds the pointer (textRef), its length and, for a front-matter skill, skillHeader. Every case's
// workspace gets these text files, so Node reads them as it does live. (Inline text would be
// migrated by Node's load, which is Node's own startup step, not the route's.)
const TEXTS = {
  'docs/SKILL.md': '---\nname: s\ndescription: d\n---\nbody',
  '10': '---\r\nDescription : x\r\n---',
  '2': 'plain',
  'kept.md': 'no header',
};
const textRef = (t) => require('node:crypto').createHash('sha256').update(t, 'utf8').digest('hex');
const skillHeader = (t) => { const h = /^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/.exec(t); return !!h && /^(?:name|description)\s*:/im.test(h[1]); };
const sourceRow = (name, text) => ({ name, textRef: textRef(text), chars: text.length, ...(skillHeader(text) ? { skillHeader: true } : {}) });
const frame = { projectId: 'proj-a', kind: 'idea', tags: ['alpha'], links: [], confirmed: true, source: 'user' };
const base = {
  projects: [
    { id: 'proj-a', name: 'Alpha', modes: ['chat'], chats: [
      { id: 'c1', title: 'One', updatedAt: 300, preview: 'p1', pinned: false, archived: false, frame, forceLocal: true, createdAt: 100 },
      { id: 'c2', title: 'Two', updatedAt: 200, preview: '', pinned: true, archived: false },
      { id: 'dead-1', title: 'Deleted', updatedAt: 250, preview: '' },
      { id: 'tie-1', title: 'Tie', updatedAt: 200, preview: '' },
      { id: 'nul', title: 'Null time', updatedAt: null, preview: '' },
      'stray-string', null, { title: 'no id' },
    ], instructionSkills: {} },
    { id: 'proj-b', name: 'Beta', modes: ['chat'], chats: [{ id: 'cb', title: 'B', updatedAt: 50, preview: '' }], instructionSkills: {} },
    { id: 'cowork-chat-context-x1', name: 'Chat attachments x1', modes: ['chat'], routing: 'manual', chats: [] },
    { id: 'proj-skill', name: 'Skills', modes: ['chat'], files: Object.entries(TEXTS).map(([name, text]) => sourceRow(name, text)), instructionSkills: { 'kept.md': { enabled: true, reviewedHash: 'h1' }, 'gone.md': { enabled: false, reviewedHash: 'h2' } }, chats: [] },
    { id: 'proj-empty', name: 'Empty', modes: ['chat'] },
  ],
};
const files = {
  base: { projects: pretty(base), free: JSON.stringify([{ id: 'free-1', title: 'F', updatedAt: 10 }, null, { id: 7 }]), tombstones: JSON.stringify(['dead-1', 5, 'gone-chat']) },
};
const withProjects = (mut, extra = {}) => {
  const p = JSON.parse(JSON.stringify(base));
  mut(p.projects);
  return { ...files.base, projects: pretty(p), ...extra };
};

const cases = [];
const add = (name, ws, request) => cases.push({ name, ws, admin: true, now: NOW, ...request });
const post = (name, ws, project, body, extra = {}) => add(name, ws, { method: 'POST', project, body: typeof body === 'string' ? body : JSON.stringify(body), ...extra });
const get = (name, ws, project, extra = {}) => add(name, ws, { method: 'GET', project, body: null, ...extra });

// GET
get('get-list', files.base, 'proj-a');
get('get-other', files.base, 'proj-b');
get('get-no-chats-key', files.base, 'proj-empty');
get('get-missing', files.base, 'nope');
get('get-encoded-id', files.base, 'proj%2Da');
get('get-bad-escape', files.base, 'proj%E0%A4%A');
get('get-heals-context', files.base, 'cowork-chat-context-x1');
get('get-reconciles-skills', files.base, 'proj-skill');
get('get-no-file', { projects: null, free: null, tombstones: null }, 'proj-a');
get('get-chats-string', withProjects((p) => { p[0].chats = 'abc'; }), 'proj-a');
get('get-chats-object', withProjects((p) => { p[0].chats = { id: 'x' }; }), 'proj-a');
get('get-chats-null', withProjects((p) => { p[0].chats = null; }), 'proj-a');
// #1236/#1621: a damaged projects.json with non-object file entries loads without them; the saves
// getProject (reconcile) and saveProjects make write the list without them.
const nullFiles = (p) => { p[3].files.splice(1, 0, null, 7, 'x'); p[0].files = [null]; };
get('get-null-files', withProjects(nullFiles), 'proj-skill');
get('get-null-files-other', withProjects(nullFiles), 'proj-a');
post('post-null-files', withProjects(nullFiles), 'proj-b', { chats: [{ id: 'cb2', title: 'B2', updatedAt: 60 }] });
get('get-routing-chosen-not-healed', withProjects((p) => { p[2].routingChosen = true; }), 'cowork-chat-context-x1');
get('get-lone-surrogates', { ...files.base, projects: pretty(base).replace('"p1"', '"p\\ud83d"').replace('"One"', '"\\udc00One"') }, 'proj-a');

// POST: the merge
post('post-add', files.base, 'proj-a', { chats: [{ id: 'c3', title: 'Three', updatedAt: 400, preview: 'hello', createdAt: 5 }] });
post('post-keeps-stored-fields', files.base, 'proj-a', { chats: [{ id: 'c1', title: 'One again', updatedAt: 500, preview: 'x' }] });
post('post-frame-null-clears', files.base, 'proj-a', { chats: [{ id: 'c1', title: 'One', updatedAt: 300, frame: null }] });
post('post-frame-invalid-keeps', files.base, 'proj-a', { chats: [{ id: 'c1', title: 'One', updatedAt: 300, frame: { kind: 'bogus' } }] });
post('post-frame-invalid-new', files.base, 'proj-a', { chats: [{ id: 'c2', updatedAt: 1, frame: [1] }] });
post('post-frame-normalised', files.base, 'proj-a', { chats: [{ id: 'c2', updatedAt: 210, frame: { kind: 'code', tags: [' ##a  b　c ', 'a-b-c', 7, '#', ''], links: ['x', 'x', '', 3, 'y'.repeat(129)], projectId: 'proj-b', confirmed: 'yes', source: 'other', extra: 1 } }] });
post('post-flags', files.base, 'proj-a', { chats: [{ id: 'c1', updatedAt: 300, forceLocal: false, allowCloud: 'yes' }, { id: 'c2', updatedAt: 200, allowCloud: true, forceLocal: 0 }] });
post('post-created-at', files.base, 'proj-a', { chats: [{ id: 'c1', updatedAt: 300, createdAt: 999 }, { id: 'n1', updatedAt: 1, createdAt: -0 }, { id: 'n2', updatedAt: 2, createdAt: '7' }] });
post('post-sanitises', files.base, 'proj-a', { chats: [
  { id: 'c 4/../x', title: { a: 1 }, preview: ['p', null, 3], updatedAt: '9' },
  { id: '%%%', title: 'gone' }, 5, 'str', null, [], { title: 'no id' }, { id: 12 },
  { id: 'x'.repeat(100), title: 't'.repeat(130), preview: 'p'.repeat(250), pinned: 'true', archived: true },
  { id: 'empty-title', title: '', preview: 0, updatedAt: -0 },
] });
post('post-title-throws', files.base, 'proj-a', { chats: [{ id: 'bad', title: { toString: 1 } }] });
post('post-updated-now', files.base, 'proj-a', { chats: [{ id: 'later', title: 'Now' }] });
post('post-elsewhere-skipped', files.base, 'proj-a', { chats: [{ id: 'free-1', updatedAt: 1 }, { id: 'cb', updatedAt: 1 }, { id: 'free-1', updatedAt: 2 }, { id: 'c9', updatedAt: 3 }] });
post('post-own-list-not-elsewhere', files.base, 'proj-b', { chats: [{ id: 'cb', title: 'B2', updatedAt: 60 }] });
post('post-tombstoned', files.base, 'proj-a', { chats: [{ id: 'dead-1', updatedAt: 999 }, { id: 'gone-chat', updatedAt: 999 }] });
post('post-duplicates', files.base, 'proj-a', { chats: [{ id: 'd1', title: 'first', updatedAt: 5, frame: { kind: 'idea' }, createdAt: 3 }, { id: 'd1', title: 'second', updatedAt: 6 }] });
post('post-mode-admin', files.base, 'proj-a', { chats: [{ id: 'cw', updatedAt: 7, mode: 'cowork' }, { id: 'cx', updatedAt: 8, mode: 'code' }] });
add('post-mode-member', files.base, { method: 'POST', project: 'proj-a', admin: false, body: JSON.stringify({ chats: [{ id: 'cw', updatedAt: 7, mode: 'cowork' }] }) });
post('post-empty-list', files.base, 'proj-a', { chats: [] });
post('post-no-chats-key', files.base, 'proj-empty', { chats: [{ id: 'e1', updatedAt: 1 }] });
post('post-heals-and-saves', files.base, 'cowork-chat-context-x1', { chats: [{ id: 'k1', updatedAt: 1 }] });
post('post-reconciles-every-project', files.base, 'proj-b', { chats: [] });
// #1806: a save of the list keeps every source pointer (textRef, chars, skillHeader) as stored.
post('post-keeps-source-pointers', files.base, 'proj-skill', { chats: [{ id: 'sp', title: 'Pointers', updatedAt: 9 }] });
post('post-pointer-skill-flag-dropped', withProjects((p) => { delete p[3].files[1].skillHeader; p[3].instructionSkills = {}; }), 'proj-skill', { chats: [] });
post('post-cap', files.base, 'proj-a', { chats: Array.from({ length: 1005 }, (_, i) => ({ id: `m${i}`, updatedAt: i % 7, title: `m${i}` })) });
post('post-lone-surrogates', files.base, 'proj-a', `{"chats":[{"id":"s1","title":"${'t'.repeat(119)}\\ud83d\\ude00","preview":"\\udc00p\\ud800","updatedAt":3,"frame":{"kind":"idea","tags":["\\ud83d"]}}]}`);
post('post-emoji-cut', files.base, 'proj-a', { chats: [{ id: 's2', title: `${'t'.repeat(119)}😀`, preview: `${'p'.repeat(199)}😀x`, updatedAt: 4 }] });
post('post-keeps-stored-lone-surrogate', { ...files.base, projects: pretty(base).replace('"p1"', '"p\\ud83d"') }, 'proj-a', { chats: [{ id: 'c2', updatedAt: 201 }] });
post('post-integer-keys', withProjects((p) => { p[0].chats.push({ id: 'ik', title: 'k', updatedAt: 1, 2: 'b', 1: 'a' }); }), 'proj-a', { chats: [{ id: 'ik', updatedAt: 2 }] });

// POST: the refusals and their order
post('post-missing-project', files.base, 'nope', { chats: [] });
post('post-missing-project-bad-json', files.base, 'nope', '{');
post('post-bad-escape', files.base, 'proj%E0%A4%A', { chats: [] });
for (const [n, body] of [['invalid', '{'], ['empty', ''], ['null', 'null'], ['array', '[]'], ['string', '"x"'], ['number', '5'], ['chats-object', '{"chats":{}}'], ['no-chats', '{}'], ['chats-null', '{"chats":null}'], ['bom', '﻿{"chats":[]}'], ['trailing', '{"chats":[]} x'], ['infinity', '{"chats":[{"id":"i","updatedAt":1e400}]}']]) {
  post(`post-body-${n}`, files.base, 'proj-a', body);
}
post('post-body-invalid-heals', files.base, 'cowork-chat-context-x1', 'null');
add('post-over-limit', files.base, { method: 'POST', project: 'proj-a', bodyRepeat: { text: 'x', count: 1024 * 1024 + 1 } });
add('post-over-limit-missing', files.base, { method: 'POST', project: 'nope', bodyRepeat: { text: 'x', count: 1024 * 1024 + 1 } });
{
  // Exactly readBody's limit, valid JSON.
  const head = '{"chats":[{"id":"big","updatedAt":1,"preview":"', tail = '"}]}';
  add('post-at-limit', files.base, { method: 'POST', project: 'proj-a', bodyRepeat: { head, text: 'p', count: 1024 * 1024 - head.length - tail.length, tail } });
}
post('post-free-chats-object', { ...files.base, free: '{"a":1}' }, 'proj-a', { chats: [{ id: 'z', updatedAt: 1 }] });
post('post-free-chats-null', { ...files.base, free: 'null' }, 'proj-a', { chats: [{ id: 'z', updatedAt: 1 }] });
post('post-free-chats-invalid', { ...files.base, free: '{oops' }, 'proj-a', { chats: [{ id: 'free-1', updatedAt: 1 }] });
post('post-free-chats-string', { ...files.base, free: '"free-1"' }, 'proj-a', { chats: [{ id: 'free-1', updatedAt: 1 }] });
post('post-no-side-files', { ...files.base, free: null, tombstones: null }, 'proj-a', { chats: [{ id: 'dead-1', updatedAt: 1 }] });
post('post-tombstones-object', { ...files.base, tombstones: '{"dead-1":1}' }, 'proj-a', { chats: [{ id: 'dead-1', updatedAt: 1 }] });
post('post-other-chats-object', withProjects((p) => { p[1].chats = { id: 'x' }; }), 'proj-a', { chats: [] });
post('post-other-chats-string', withProjects((p) => { p[1].chats = 'cb'; }), 'proj-a', { chats: [{ id: 'cb', updatedAt: 1 }] });
post('post-own-chats-string', withProjects((p) => { p[0].chats = 'abc'; }), 'proj-a', { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-own-chats-object', withProjects((p) => { p[0].chats = { a: 1 }; }), 'proj-a', { chats: [{ id: 'q', updatedAt: 1 }] });
post('post-stored-updated-true', withProjects((p) => { p[0].chats.push({ id: 'bt', updatedAt: true }); }), 'proj-a', { chats: [{ id: 'q', updatedAt: 1 }] });

// ── Node, one fresh workspace per case ──────────────────────────────────────
async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'chats-fixture-'));
  const dir = path.join(root, 'users', USER);
  fs.mkdirSync(dir, { recursive: true });
  if (c.ws.projects !== null) fs.writeFileSync(path.join(dir, 'projects.json'), c.ws.projects);
  const texts = path.join(dir, 'project-sources', 'proj-skill');
  fs.mkdirSync(texts, { recursive: true });
  for (const text of Object.values(TEXTS)) fs.writeFileSync(path.join(texts, `${textRef(text)}.txt`), text);
  if (c.ws.free !== null) fs.writeFileSync(path.join(dir, 'free-chats.json'), c.ws.free);
  if (c.ws.tombstones !== null) fs.writeFileSync(path.join(dir, 'deleted-chats.json'), c.ws.tombstones);
  {
    const store = createWorkspaceStore(root, { id: 'local', label: 'Local', baseUrl: '', apiKey: '' }, null);
    const workspace = store.get(USER);
    const currentWorkspace = () => workspace;
    const proxy = (field) => new Proxy([], {
      get(_t, prop) { const v = currentWorkspace()[field][prop]; return typeof v === 'function' ? v.bind(currentWorkspace()[field]) : v; },
      set(_t, prop, v) { currentWorkspace()[field][prop] = v; return true; },
      ownKeys() { return Reflect.ownKeys(currentWorkspace()[field]); },
      getOwnPropertyDescriptor(_t, prop) { return Reflect.getOwnPropertyDescriptor(currentWorkspace()[field], prop); },
      has(_t, prop) { return prop in currentWorkspace()[field]; },
    });
    const PROJECTS = proxy('projects'), FREE_CHATS = proxy('freeChats');
    const projectStore = createProjectStore({
      fs, path, currentWorkspace, PROJECTS, FREE_CHATS, sanitizeToolboxes: () => null, defaultToolboxes: () => ['core'],
      reasoningEffort: {}, projectAppearance: () => ({}), rag: {}, storageClient: {}, documentSources: {}, authService: {},
      PROJECT_ROOT_FOLDER: 'Projects', createProjectFolder: async () => null, projectSweep: { afterDelete: async () => [] },
    });
    const routes = createProjectRoutes({
      json: http.json, readBody: http.readBody, readJson: http.readJson, store: projectStore, PROJECTS, currentWorkspace,
      DEFAULT_TOOLBOXES: ['core'], diaryExtras: {}, fs, path,
    });
    const before = c.ws.projects;
    const r = c.bodyRepeat;
    const body = r ? (r.head || '') + r.text.repeat(r.count) + (r.tail || '') : c.body;
    const request = { method: c.method, headers: {}, async *[Symbol.asyncIterator]() { if (body !== null && body !== '') yield Buffer.from(body, 'utf8'); } };
    let status = null, text = null;
    const res = { writeHead(code) { status = code; }, setHeader() {}, end(v) { text = String(v); } };
    const p = `/api/projects/${c.project}/chats`;
    const authn = { user: { id: USER, role: c.admin ? 'admin' : 'member' } };
    try {
      const handled = await routes(request, res, { path: p, authn, url: new URL(`http://h${p}`) });
      if (!handled) throw Error(`${c.name}: not handled`);
    } catch (err) {
      const failure = http.errorResponse(err);
      http.json(res, failure.status, failure.body);
    }
    const file = path.join(dir, 'projects.json');
    const after = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null;
    fs.rmSync(root, { recursive: true, force: true });
    return { status, body: text, projectsAfter: after === before ? null : after };
  }
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
  console.log(`project chats fixtures=${results.length} core=${sha} node=${process.version}`);
})().catch((e) => { console.error(e); process.exit(1); });
