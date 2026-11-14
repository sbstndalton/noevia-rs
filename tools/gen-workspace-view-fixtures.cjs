#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own workspace load (workspace.cjs createWorkspaceStore
// `get`, over rust-projects.cjs, source-text.cjs and project-modes.cjs) and projects.cjs `getProject`
// on synthetic, QA-named workspaces, and records what a chat turn reads from them: the projects as
// JSON, each source's text, the free chats, the preferences, the saved roles, and for each looked-up
// id the project getProject answers and whether it saved. Track R14 step 1.
//
//   node tools/gen-workspace-view-fixtures.cjs <noevia-core checkout> <output.json>
//   node tools/gen-workspace-view-fixtures.cjs <noevia-core checkout> --check <fixture.json>
//     records again and fails unless the result is byte-identical to the committed file.
//
// crates/server-projects/tests/workspace_view.rs replays every case against workspace_view.rs.
// Nothing from node_modules is loaded (checked). The Rust project store Node writes through is a
// stub that records each write and keeps the file, so a load's own saves and getProject's are seen.
// Each core file loaded is listed with its sha256: a changed file needs a fresh recording.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const checkAt = process.argv[3] === '--check' ? process.argv[4] : null;
const out = checkAt ? null : process.argv[3];
if (!process.argv[2] || (!out && !checkAt)) throw Error('usage: gen-workspace-view-fixtures.cjs <core> <output.json> | --check <fixture.json>');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};
const req = (m) => require(path.join(core, 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');

const USER = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee';
const DEFAULT = { id: 'local', label: 'QA local', baseUrl: 'http://qa-engine.invalid/v1', apiKey: '', shared: true };
const ref = (t) => crypto.createHash('sha256').update(t, 'utf8').digest('hex');
const pretty = (v) => JSON.stringify(v, null, 2);

// ── Synthetic workspaces ────────────────────────────────────────────────────
const NOTE = 'QA note: the synthetic project text.';
const SKILL = '---\nname: qa-skill\ndescription: a QA skill\n---\nQA skill body';
const SURR = 'QA lone \ud83d surrogate and astral \u{1f600}';
const base = () => ({
  projects: [
    { id: 'qa-proj-a', name: 'QA Alpha', modes: ['chat', 'code'], provider: 'local', routing: 'auto', chats: [{ id: 'qa-c1', title: 'QA one', updatedAt: 5 }],
      files: [{ name: 'notes.md', textRef: ref(NOTE), chars: NOTE.length }, null, 7, 'x', { name: 'gone.md', textRef: ref('missing text') }] },
    { id: 'qa-proj-b', name: 'QA Beta', modes: ['cowork'], provider: 'lemonade', chats: [] },
    { id: 'qa-proj-c', name: 'QA Gamma', chats: [], files: [{ name: 'bad-ref.txt', textRef: 'nothex' }, { name: 'num.txt', content: 5 }, { name: 'no-ref.txt' }] },
    { id: 'cowork-chat-context-qa-heal', name: 'Chat attachments qa-heal', modes: ['chat'], routing: 'manual', chats: [], instructionSkills: {} },
    { id: 'cowork-chat-context-qa-bare', name: 'Chat attachments qa-bare', modes: ['chat'], routing: 'manual', chats: [] },
    { id: 'cowork-chat-context-qa-chosen', name: 'Chat attachments qa-chosen', modes: ['chat'], routing: 'manual', routingChosen: true, chats: [], instructionSkills: {} },
    { id: 'qa-skill-proj', name: 'QA skills', modes: ['chat'], chats: [], files: [{ name: 'docs/SKILL.md', textRef: ref(SKILL), chars: SKILL.length, skillHeader: true }] },
    { id: 'qa-skill-done', name: 'QA skills done', modes: ['chat'], chats: [], files: [{ name: 'docs/SKILL.md', textRef: ref(SKILL), chars: SKILL.length, skillHeader: true }],
      instructionSkills: { 'docs/SKILL.md': { enabled: false, reviewedHash: null } } },
    { id: 'qa-surr', name: SURR, modes: ['chat', 'chat'], chats: [], files: [{ name: 'surr.md', textRef: ref(SURR), chars: SURR.length }] },
    { id: 'qa-archived', name: 'QA archived', modes: ['code', 'chat'], archived: true, provider: 'qa-cloud', files: 'not a list', chats: [] },
  ],
});
const SOURCES = { 'qa-proj-a': [NOTE], 'qa-skill-proj': [SKILL], 'qa-skill-done': [SKILL], 'qa-surr': [SURR] };
const ALL_IDS = ['qa-proj-a', 'qa-proj-b', 'qa-proj-c', 'cowork-chat-context-qa-heal', 'cowork-chat-context-qa-bare', 'cowork-chat-context-qa-chosen', 'qa-skill-proj', 'qa-skill-done', 'qa-dup', 'qa-surr', 'qa-archived', 'qa-none', ''];
const FREE = JSON.stringify([{ id: 'qa-free-1', title: 'QA free', updatedAt: 3 }, 'stray', null]);
const ws = (mut = () => {}) => { const p = base(); mut(p); return pretty(p); };

const cases = [];
const add = (name, files, extra = {}) => cases.push({ name, files, queries: ALL_IDS, ...extra });
add('full', { 'projects.json': ws(), 'free-chats.json': FREE, 'preferences.json': '{"defaultRouting":"auto","qa":1}', 'auto-roles.json': '{"chat":"qa-model"}' });
add('absent-dir', {}, { absent: true });
add('empty-dir', {});
add('projects-not-json', { 'projects.json': '{"projects": [' });
add('projects-null', { 'projects.json': 'null' });
add('projects-field-null', { 'projects.json': '{"projects":null}' });
add('projects-field-object', { 'projects.json': '{"projects":{"a":1}}' });
add('projects-number', { 'projects.json': '5' });
add('projects-array-doc', { 'projects.json': '[{"id":"qa-x"}]' });
add('projects-empty', { 'projects.json': '{"projects":[]}' });
add('null-project', { 'projects.json': ws((p) => { p.projects.splice(1, 0, null); }) });
add('number-project', { 'projects.json': ws((p) => { p.projects.push(4); }) });
add('string-project', { 'projects.json': ws((p) => { p.projects.push('qa'); }) });
add('free-object', { 'projects.json': ws(), 'free-chats.json': '{"chats":[]}' });
add('free-not-json', { 'projects.json': ws(), 'free-chats.json': '[{' });
add('free-null', { 'free-chats.json': 'null' });
add('prefs-null', { 'preferences.json': 'null' });
add('prefs-false', { 'preferences.json': 'false' });
add('prefs-zero', { 'preferences.json': '0' });
add('prefs-array', { 'preferences.json': '[1,2]' });
add('prefs-string', { 'preferences.json': '"qa"' });
add('prefs-not-json', { 'preferences.json': '{' });
add('roles-not-json', { 'auto-roles.json': '{x' });
add('roles-null', { 'auto-roles.json': 'null' });
add('roles-list', { 'auto-roles.json': '[{"role":"qa"}]' });
add('modes-variants', { 'projects.json': pretty({ projects: [
  { id: 'qa-m1', modes: [] }, { id: 'qa-m2', modes: 'chat' }, { id: 'qa-m3', modes: ['chat'] }, { id: 'qa-m4', modes: ['code', 'cowork'] },
  { id: 'qa-m5' }, { id: 'qa-m6', modes: ['chat', 'other'] }, { id: 'qa-m7', modes: null }, { id: 'qa-m8', modes: ['chat', 'cowork', 'code'] },
  { id: 'qa-m9', provider: 'lemonade', files: [] }, { id: 'qa-m10', provider: 'local', files: [[], {}, false, 0] },
] }) }, { queries: ['qa-m1', 'qa-m4', 'qa-m9', 'qa-m10'] });
add('invalid-user', {}, { user: 'not-a-user' });
// Two projects with one id, in a file the load does not save: getProject answers the first, the list
// keeps both. (After a load that saves, Node's list also loses the second: the store merge keys by id.)
add('duplicate-ids', { 'projects.json': pretty({ projects: [{ id: 'qa-dup', name: 'QA first of two', modes: ['chat'] }, { id: 'qa-dup', name: 'QA second of two', modes: ['chat'] }] }) },
  { queries: ['qa-dup'] });
// A source whose text is still inline (from before #1806): Node moves it to its file on load (and
// saves); the view keeps it inline. The test compares the texts, and the projects without the
// fields that move (INLINE).
add('inline-text', { 'projects.json': pretty({ projects: [{ id: 'qa-inline', modes: ['chat'], files: [{ name: 'inline.txt', content: 'QA inline text' }, { name: 'num.txt', content: 5 }, { name: 'obj.txt', content: { a: 1 } }, { name: 'list.txt', content: [1, null, 'x'] }, { name: 'zero.txt', content: 0 }] }] }) },
  { queries: ['qa-inline'] });

// ── Node, one fresh workspace per case ──────────────────────────────────────
function stubStore(root, writes) {
  const file = (user, name) => path.join(root, 'users', user, name);
  return {
    async read(user, name) { try { return { version: 1, text: fs.readFileSync(file(user, name), 'utf8') }; } catch { return { version: 1, text: null }; } },
    async write(user, name, _version, text) { writes.push({ name, text }); fs.writeFileSync(file(user, name), text); return { ok: true }; },
    grantContext() { throw Error('not used'); }, chatMove() { throw Error('not used'); },
  };
}
const settle = () => new Promise((r) => setTimeout(r, 0));

async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-workspace-view-'));
  const user = c.user || USER;
  const dir = path.join(root, 'users', USER);
  if (!c.absent) {
    fs.mkdirSync(dir, { recursive: true });
    for (const [name, text] of Object.entries(c.files)) fs.writeFileSync(path.join(dir, name), text);
    for (const [id, texts] of Object.entries(SOURCES)) {
      fs.mkdirSync(path.join(dir, 'project-sources', id), { recursive: true });
      for (const t of texts) fs.writeFileSync(path.join(dir, 'project-sources', id, `${ref(t)}.txt`), t);
    }
  }
  const writes = [];
  const result = {};
  try {
    const store = createWorkspaceStore(root, DEFAULT, null, { projectStore: stubStore(root, writes) });
    const workspace = store.get(user);
    for (let i = 0; i < 5; i += 1) await settle();
    while (workspace.pendingStoreWrites()) await workspace.pendingStoreWrites();
    result.loadWrites = writes.length;
    result.projects = JSON.stringify(workspace.projects);
    result.texts = [];
    workspace.projects.forEach((p, pi) => {
      if (!p || typeof p !== 'object' || !Array.isArray(p.files)) return;
      p.files.forEach((f, fi) => { result.texts.push([pi, fi, String(f.content || '')]); });
    });
    result.freeChats = JSON.stringify(workspace.freeChats);
    result.preferences = JSON.stringify(workspace.preferences);
    result.autoRoles = JSON.stringify(workspace.autoRoles);
    const currentWorkspace = () => workspace;
    const PROJECTS = new Proxy([], {
      get(_t, prop) { const v = workspace.projects[prop]; return typeof v === 'function' ? v.bind(workspace.projects) : v; },
      ownKeys() { return Reflect.ownKeys(workspace.projects); },
      getOwnPropertyDescriptor(_t, prop) { return Reflect.getOwnPropertyDescriptor(workspace.projects, prop); },
    });
    const { getProject } = createProjectStore({ fs, path, currentWorkspace, PROJECTS, FREE_CHATS: [] });
    result.queries = [];
    for (const id of c.queries) {
      const before = writes.length;
      let project = getProject(id);
      project = project === null ? null : JSON.stringify(project);
      for (let i = 0; i < 5; i += 1) await settle();
      while (workspace.pendingStoreWrites()) await workspace.pendingStoreWrites();
      // The project as getProject's save wrote it (the first entry with that id in the text written).
      const written = writes.slice(before).filter((w) => w.name === 'projects.json').map((w) => JSON.parse(w.text).projects.find((p) => p && p.id === id));
      result.queries.push({ id, project, saved: writes.length > before, written: written.length ? JSON.stringify(written[written.length - 1]) : null });
    }
  } catch (e) {
    result.threw = String(e && e.message ? e.message : e).slice(0, 120);
  }
  fs.rmSync(root, { recursive: true, force: true });
  return result;
}

function sources() {
  const all = Object.keys(require.cache);
  if (all.some((k) => k.includes(`${path.sep}node_modules${path.sep}`))) throw Error('a module from node_modules was loaded');
  const loaded = all.filter((k) => k.startsWith(core + path.sep));
  return Object.fromEntries(loaded.map((k) => path.relative(core, k).split(path.sep).join('/')).sort()
    .map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(core, f))).digest('hex')]));
}

(async () => {
  const recorded = [];
  for (const c of cases) recorded.push({ name: c.name, user: c.user || USER, absent: !!c.absent, files: c.files, queries: c.queries, node: await run(c) });
  const doc = { version: 1, generator: 'tools/gen-workspace-view-fixtures.cjs', defaultProvider: DEFAULT.id, sources: SOURCES, sourcesRead: sources(), cases: recorded };
  const text = `${pretty(doc)}\n`;
  if (checkAt) {
    if (fs.readFileSync(checkAt, 'utf8') !== text) { process.stderr.write(`${checkAt} differs from a fresh recording\n`); process.exit(1); }
    process.stderr.write(`${checkAt}: identical\n`);
  } else fs.writeFileSync(out, text);
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
