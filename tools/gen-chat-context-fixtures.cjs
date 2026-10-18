#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own GET/POST /api/chats/{chatId}/context (routes/projects.cjs
// over projects.cjs getProject/saveProjects, workspace.cjs, diary-extras.cjs, chat-lists.cjs,
// instruction-skills.cjs) on synthetic workspaces and records each answer and the projects.json it
// leaves. sbstndalton/noevia#1264, the first project-writes slice.
//
//   node tools/gen-chat-context-fixtures.cjs <noevia-core checkout> <output.json>
//
// crates/server-projects/tests/chat_context_differential.rs replays every case against the Rust port.
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const cp = require('node:child_process');

const core = path.resolve(process.argv[2] || '../noevia-core');
const out = process.argv[3];
if (!out) throw Error('usage: gen-chat-context-fixtures.cjs <core> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const req = (m) => require(path.join(core, 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');
const { createProjectRoutes } = req('routes/projects.cjs');
const diaryExtras = req('diary-extras.cjs');
const http = req('http.cjs');

const USER = '11111111-2222-4333-8444-555555555555';
const NOW = 1700000000123;
const pretty = (v) => JSON.stringify(v, null, 2);

// ── Synthetic workspaces ────────────────────────────────────────────────────
const SKILL = '---\nname: s\ndescription: d\n---\nbody';
const textRef = (t) => require('node:crypto').createHash('sha256').update(t, 'utf8').digest('hex');
const base = {
  projects: [
    { id: 'proj-a', name: 'Alpha', modes: ['chat'], chats: [], instructionSkills: {} },
    { id: 'cowork-chat-context-c1', name: 'Chat attachments c1', modes: ['chat'], routing: 'auto', toolboxes: ['core'], chats: [], instructionSkills: {} },
    { id: 'cowork-chat-context-heal', name: 'Chat attachments heal', modes: ['chat'], routing: 'manual', chats: [], instructionSkills: {} },
    { id: 'cowork-chat-context-chosen', name: 'Chat attachments chosen', modes: ['chat'], routing: 'manual', routingChosen: true, chats: [], instructionSkills: {} },
    { id: 'cowork-chat-context-skill', name: 'Chat attachments skill', modes: ['chat'], routing: 'auto', chats: [],
      files: [{ name: 'docs/SKILL.md', textRef: textRef(SKILL), chars: SKILL.length, skillHeader: true }] },
    { id: 'proj-no-skills', name: 'No skills yet', modes: ['chat'], chats: [] },
  ],
};
const ws = (mut = () => {}) => { const p = JSON.parse(JSON.stringify(base)); mut(p); return { projects: pretty(p), tombstones: JSON.stringify(['dead-1', 5]) }; };
const plain = ws();
const LONG = 'x'.repeat(100);

const cases = [];
const add = (name, w, method, chat) => cases.push({ name, ws: w, method, chat, now: NOW });
for (const method of ['GET', 'POST']) {
  const m = method.toLowerCase();
  add(`${m}-existing`, plain, method, 'c1');
  add(`${m}-heal`, plain, method, 'heal');
  add(`${m}-chosen`, plain, method, 'chosen');
  add(`${m}-reconcile`, plain, method, 'skill');
  add(`${m}-missing`, plain, method, 'n1');
  add(`${m}-tombstoned`, plain, method, 'dead-1');
  add(`${m}-invalid-dot`, plain, method, 'a.b');
  add(`${m}-invalid-space`, plain, method, 'a%20b');
  add(`${m}-too-long`, plain, method, LONG + 'y');
  add(`${m}-longest`, plain, method, LONG);
  add(`${m}-encoded`, plain, method, '%41bc');
  add(`${m}-encoded-slash`, plain, method, 'a%2Fb');
  add(`${m}-malformed-escape`, plain, method, 'bad%E0%A4%A');
  add(`${m}-no-file`, { projects: null, tombstones: null }, method, 'n2');
  add(`${m}-empty-list`, { projects: '{"projects":[]}', tombstones: null }, method, 'n3');
  add(`${m}-projects-null`, { projects: '{"projects":null}', tombstones: null }, method, 'n4');
  add(`${m}-null-entry-before`, ws((p) => { p.projects.unshift(null); }), method, 'c1');
  add(`${m}-null-entry-after`, ws((p) => { p.projects.push(null); }), method, 'c1');
  add(`${m}-tombstones-object`, { ...plain, tombstones: '{"dead-1":1}' }, method, 'dead-1');
  add(`${m}-lone-surrogate-kept`, ws((p) => { p.projects[0].name = 'A\ud83d'; }), method, 'n5');
  add(`${m}-instruction-skills-array`, ws((p) => { p.projects[0].instructionSkills = []; }), method, 'n6');
}
// #2026: a brand-new account. Its directory does not exist when the request arrives; Node's
// workspace load (workspace.cjs get, run for every signed-in request) makes it, mode 0700, before
// the route runs. The case records that as well as the answer.
const ABSENT = { projects: null, tombstones: null, absent: true };
for (const method of ['GET', 'POST']) {
  const m = method.toLowerCase();
  add(`${m}-absent-dir`, ABSENT, method, 'n1');
  add(`${m}-absent-dir-malformed-escape`, ABSENT, method, 'bad%E0%A4%A');
  add(`${m}-absent-dir-invalid`, ABSENT, method, 'a.b');
}

// ── Node, one fresh workspace per case ──────────────────────────────────────
async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'chat-context-fixture-'));
  const dir = path.join(root, 'users', USER);
  if (!c.ws.absent) {
    fs.mkdirSync(dir, { recursive: true });
    if (c.ws.projects !== null) fs.writeFileSync(path.join(dir, 'projects.json'), c.ws.projects);
    if (c.ws.tombstones !== null) fs.writeFileSync(path.join(dir, 'deleted-chats.json'), c.ws.tombstones);
    const texts = path.join(dir, 'project-sources', 'cowork-chat-context-skill');
    fs.mkdirSync(texts, { recursive: true });
    fs.writeFileSync(path.join(texts, `${textRef(SKILL)}.txt`), SKILL);
  }
  const file = path.join(dir, 'projects.json');
  const before = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null;
  let status = null, text = null;
  const res = { writeHead(code) { status = code; }, setHeader() {}, end(v) { text = String(v); } };
  try {
    const workspace = createWorkspaceStore(root, { id: 'local', label: 'Local', baseUrl: '', apiKey: '' }, null).get(USER);
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
      DEFAULT_TOOLBOXES: ['core'], diaryExtras, fs, path,
    });
    const request = { method: c.method, headers: {}, async *[Symbol.asyncIterator]() {} };
    const p = `/api/chats/${c.chat}/context`;
    const authn = { user: { id: USER, role: 'admin' } };
    const handled = await routes(request, res, { path: p, authn, url: new URL(`http://h${p}`) });
    if (!handled) throw Error(`${c.name}: not handled`);
  } catch (err) {
    const failure = http.errorResponse(err);
    http.json(res, failure.status, failure.body);
  }
  const after = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null;
  const result = { status, body: text, projectsAfter: after === before ? null : after };
  if (c.ws.absent) {
    // Whether the request made the user's directory, and that it is private to its owner
    // (mode 0700 before the umask: no group or other bits), as the only thing under users/.
    const made = fs.existsSync(dir) && fs.statSync(dir).isDirectory();
    result.userDir = { made, private: made && (fs.statSync(dir).mode & 0o077) === 0, users: fs.readdirSync(path.join(root, 'users')) };
  }
  fs.rmSync(root, { recursive: true, force: true });
  return result;
}

(async () => {
  const results = [];
  for (const c of cases) {
    // Date.now() is the case's own clock for the whole request.
    const realNow = Date.now;
    Date.now = () => c.now;
    try { results.push({ ...c, expected: await run(c) }); } finally { Date.now = realNow; }
  }
  // The major only, so CI's regenerate-and-compare does not depend on the runner's Node patch level.
  const node = `v${process.versions.node.split('.')[0]}`;
  fs.writeFileSync(out, `${JSON.stringify({ core: sha, node, user: USER, cases: results }, null, 1)}\n`);
  console.log(`chat context fixtures=${results.length} core=${sha} node=${process.version}`);
})().catch((e) => { console.error(e); process.exit(1); });
