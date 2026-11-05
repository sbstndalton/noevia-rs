'use strict';
// Records the Node oracle for crates/chat-mirror-exec: runs noevia-core's own executors
// (chat-vault-mirror.cjs sync, chat-brain.cjs) over SYNTHETIC accounts in temp directories with a
// deterministic in-memory Diary and a fake reasoner, and writes every input, every Diary call, the
// final vault tree, the Trash and the index file bytes after each pass.
//
//   node record.cjs <core-checkout>/server > ../fixtures/oracle.v1.json
//
// No real Diary, Nextcloud data or model is touched.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const server = path.resolve(process.argv[2]);
const mirrorLib = require(path.join(server, 'chat-vault-mirror.cjs'));
const brainLib = require(path.join(server, 'chat-brain.cjs'));

/** The fake Diary: the Rust test implements the same rules. */
function fakeDiary() {
  const files = new Map(); const dirs = new Set(); const trash = []; const calls = []; const failWrite = new Set();
  let n = 0;
  const next = () => `v${++n}`;
  const err = (status) => Object.assign(Error(`status ${status}`), { status });
  const api = {
    async read(user, p) { calls.push(['read', user, JSON.stringify({ path: p })]); const f = files.get(p); if (!f) throw err(404); return { content: f.content, version: f.version }; },
    async write(user, body) { calls.push(['write', user, JSON.stringify(body)]); if (failWrite.has(body.path)) throw err(500); const version = next(); files.set(body.path, { content: body.content, version }); return { version }; },
    async mkdir(user, p) { calls.push(['mkdir', user, JSON.stringify({ path: p })]); if (dirs.has(p)) throw err(405); dirs.add(p); return {}; },
    async ops(user, body) {
      calls.push(['ops', user, JSON.stringify(body)]);
      const f = files.get(body.path);
      if (body.op === 'stat') { if (f) return { isDir: false, version: f.version }; if (dirs.has(body.path)) return { isDir: true }; throw err(404); }
      if (body.op === 'move') { if (!f) throw err(404); if (files.has(body.destination)) throw err(409); files.delete(body.path); files.set(body.destination, f); return {}; }
      if (body.op === 'delete') { if (!f) throw err(404); trash.push({ op: 'delete', path: body.path, content: f.content }); files.delete(body.path); return {}; }
      if (body.op === 'preserve') { if (!f) throw err(404); trash.push({ op: 'preserve', path: body.path, content: f.content }); return {}; }
      throw err(400);
    },
  };
  return { api, files, dirs, trash, calls, failWrite, next };
}

const sorted = (m) => Object.fromEntries([...m.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));

async function runMirror(scenario) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'mirror-oracle-'));
  const d = fakeDiary();
  const state = { lists: { freeChats: [], projects: [] }, history: {}, tombstones: [], brains: {} };
  const mirror = mirrorLib.createChatVaultMirror({
    enabled: () => true,
    lists: () => state.lists,
    deleted: () => new Set(state.tombstones),
    readHistory: (u, id) => { const h = state.history[id]; if (h === 'throw') throw Error('history unreadable'); return h || []; },
    readBrain: (u, id) => brainLib.readBrain(dir, id)?.brain || null,
    files: d.api,
    index: { read: () => mirrorLib.readIndex(dir), write: (u, s) => mirrorLib.writeIndex(dir, s) },
  });
  const out = [];
  for (const step of scenario.steps) {
    if (step.lists) state.lists = step.lists;
    if (step.history) Object.assign(state.history, step.history);
    if (step.tombstones) state.tombstones = step.tombstones;
    if (step.brains) for (const [id, b] of Object.entries(step.brains)) brainLib.writeBrain(dir, id, { brain: b, sourceUpdatedAt: 1, builtAt: 2 });
    if (step.edit) for (const [p, content] of Object.entries(step.edit)) d.files.set(p, { content, version: d.next() });
    if (step.mkdir) for (const p of step.mkdir) d.dirs.add(p);
    if (step.failWrite) { d.failWrite.clear(); for (const p of step.failWrite) d.failWrite.add(p); }
    if (step.index !== undefined) fs.writeFileSync(path.join(dir, 'chat-vault-mirror-index.json'), step.index);
    if (!step.pass) continue;
    d.calls.length = 0;
    let result;
    try { result = (await mirror.sync('u1', new Set(step.pass.dirty || []))) ? 'ok' : 'partial'; } catch (e) { result = 'threw'; }
    let index = null;
    try { index = fs.readFileSync(path.join(dir, 'chat-vault-mirror-index.json'), 'utf8'); } catch { index = null; }
    out.push({ result, calls: d.calls.slice(), files: sorted(d.files), trash: d.trash.slice(), index });
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return out;
}

const ts = 1730000000000;
const long = (n, s) => s.repeat(Math.ceil(n / s.length)).slice(0, n);
const hist = (...turns) => turns.map(([role, content, extra]) => ({ role, content, ...(extra || {}) }));
const brainA = { brain_schema: 1, summary: '  A plan <for> the trip & more.  ', decisions: ['Go by train'], facts: [], open_questions: ['Which day?'], entities: ['Lisbon', 'Ana'] };

const MIRROR = [
  { name: 'basic', steps: [
    { lists: {
      freeChats: [
        { id: 'c-b', title: 'Trip plans', createdAt: ts + 2, updatedAt: ts + 50, frame: { kind: 'plan', tags: ['travel', '', 7, 'x"y'], links: ['c-a', 'c-a', 'c-b', 'missing', 3] } },
        { id: 'c-a', title: 'Trip plans', createdAt: ts + 2, updatedAt: ts + 10 },
        { id: 'C-a', title: 'con', createdAt: ts + 2 },
        { id: '_x', title: '  ..hidden: a/b\\c*?"<>|#^[]%{}  name..  ', updatedAt: ts + 1 },
        { id: '42', title: 'Café ' + long(130, 'é😀'), createdAt: ts + 3, updatedAt: ts + 3 },
        { id: '7', title: '', createdAt: 0, updatedAt: 0 },
        { id: 'tabs', title: 'a\u0000b\tc  d', createdAt: 'x', updatedAt: ts + 4 },
        null, { title: 'no id' },
      ],
      projects: [
        { id: 'p1', name: 'Inbox', chats: [{ id: 'p1c1', title: 'Notes', createdAt: ts + 5, updatedAt: ts + 6, frame: { links: ['c-b'] } }] },
        { id: 'p2', name: 'Work/Home', chats: [{ id: 'p2c1', title: 'Notes', createdAt: ts + 5, updatedAt: ts + 7 }] },
        { id: 'p3', chats: [{ id: 'p3c1', title: 'x', createdAt: ts + 9 }] },
        { id: 'p4', name: 'Empty' },
      ] },
      history: {
        'c-b': hist(['user', '  Plan a trip  '], ['assistant', 'Sure.', { toolCalls: [{ name: 'search' }, null, { name: '' }, { name: 'web' }] }], ['system', 'hidden'], ['assistant', 'partial', { failure: 'stopped' }], ['assistant', '', { failure: '  ' }], ['user', 12], ['assistant', '', { failure: 'x' }]),
        'p1c1': hist(['user', 'hi']),
      },
      brains: { 'c-b': brainA },
      pass: {} },
    { pass: {} },
    { pass: { dirty: ['c-a'] } },
  ] },
  { name: 'rename-move-edit-trash', steps: [
    { lists: { freeChats: [{ id: 'a', title: 'First', createdAt: ts, updatedAt: ts }, { id: 'b', title: 'Second', createdAt: ts + 1, updatedAt: ts + 1 }, { id: 'c', title: 'Third', createdAt: ts + 2, updatedAt: ts + 2 }], projects: [] },
      edit: { 'Chats/Inbox/Second.md': 'someone else wrote this' }, pass: {} },
    { lists: { freeChats: [{ id: 'a', title: 'First renamed', createdAt: ts, updatedAt: ts + 9 }, { id: 'c', title: 'Third', createdAt: ts + 2, updatedAt: ts + 2 }], projects: [{ id: 'p', name: 'Proj', chats: [{ id: 'b', title: 'Second', createdAt: ts + 1, updatedAt: ts + 1 }] }] },
      edit: { 'Chats/Inbox/Third.md': 'edited in the vault' }, pass: {} },
    { tombstones: ['c', 'zzz'], lists: { freeChats: [{ id: 'a', title: 'First renamed', createdAt: ts, updatedAt: ts + 9 }], projects: [] }, pass: {} },
    { lists: { freeChats: [], projects: [] }, tombstones: ['a', 'b'], pass: {} },
    { lists: { freeChats: [{ id: 'z', title: 'Fresh', createdAt: ts }], projects: [] }, tombstones: ['a', 'b'], pass: {} },
  ] },
  { name: 'partial-failure', steps: [
    { lists: { freeChats: [{ id: 'a', title: 'One', createdAt: ts }, { id: 'b', title: 'Two', createdAt: ts + 1 }, { id: 'c', title: 'Three', createdAt: ts + 2 }], projects: [] },
      history: { c: 'throw' }, failWrite: ['Chats/Inbox/One.md'], mkdir: ['Chats'], pass: {} },
    { failWrite: [], history: { c: [] }, pass: {} },
  ] },
  { name: 'moved-dest-ours', steps: [
    { lists: { freeChats: [{ id: 'a', title: 'Old', createdAt: ts }], projects: [] }, pass: {} },
    { lists: { freeChats: [{ id: 'a', title: 'New', createdAt: ts }], projects: [] }, pass: {} },
    { index: '{"keep":1,"notes":{"a":{"path":"Chats/Inbox/Old.md","version":"v4","meta":"x"},"taken:Chats/Inbox/Gone.md":{"path":"Chats/Inbox/Gone.md","foreign":true},"taken:../x":{"path":"../x"}},"after":true}',
      edit: { 'Chats/Inbox/Old.md': 'old copy' }, pass: {} },
  ] },
  { name: 'corrupt-index', steps: [
    { index: 'not json', lists: { freeChats: [{ id: 'a', title: 'A', createdAt: ts }], projects: [] }, pass: {} },
    { index: '{"notes":[{"path":"Chats/Inbox/A.md"}]}', pass: {} },
    { index: '{"notes":{"a":null}}', pass: {} },
  ] },
  { name: 'long-note', steps: [
    { lists: { freeChats: [{ id: 'big', title: 'Big', createdAt: ts, updatedAt: ts, frame: { links: [] } }], projects: [] },
      history: { big: hist(['user', long(300000, 'ab€')], ['assistant', long(300000, '😀x')]) }, pass: {} },
  ] },
  { name: 'threw-project-chats', steps: [
    { lists: { freeChats: [{ id: 'a', title: 'A', createdAt: ts }], projects: [{ name: 'Bad', chats: { not: 'a list' } }] }, pass: {} },
  ] },
];

// ── Brain ───────────────────────────────────────────────────────────────────────────────────
const OUTPUTS = [
  JSON.stringify(brainA), '```json\n' + JSON.stringify(brainA) + '\n```', '```\n' + JSON.stringify(brainA) + '```', '``` \n\n' + JSON.stringify(brainA) + '\n\n```',
  '```JSON\n{}\n```', '', '   ', 'not json', '{"brain_schema":2}', '{"brain_schema":1,"summary":"x","decisions":[],"facts":[],"open_questions":[],"entities":[],"extra":1}',
  JSON.stringify({ ...brainA, summary: '' }), JSON.stringify({ ...brainA, summary: long(601, 'a') }), JSON.stringify({ ...brainA, summary: '😀'.repeat(300) }), JSON.stringify({ ...brainA, summary: '😀'.repeat(301) }),
  JSON.stringify({ ...brainA, facts: ['two\nlines'] }), JSON.stringify({ ...brainA, facts: ['zero​width'] }), JSON.stringify({ ...brainA, facts: ['tab\tok'] }),
  JSON.stringify({ ...brainA, entities: Array(25).fill('e') }), JSON.stringify({ ...brainA, entities: [long(81, 'e')] }), JSON.stringify({ ...brainA, decisions: [3] }),
  JSON.stringify({ ...brainA, decisions: Array(12).fill(long(300, 'é')), facts: Array(16).fill(long(300, 'é')), open_questions: Array(12).fill(long(300, 'é')) }),
  JSON.stringify({ entities: [], brain_schema: 1, summary: 's', decisions: [], facts: [], open_questions: [] }),
  '{"brain_schema":1,"brain_schema":1,"summary":"dup","decisions":[],"facts":[],"open_questions":[],"entities":[]}',
  long(24577, ' '), JSON.stringify(brainA) + long(24577 - JSON.stringify(brainA).length, ' '),
];

async function runBrain() {
  const parsed = OUTPUTS.map((o) => { const r = brainLib.parseBrain(o); return r.ok ? { ok: true, brain: JSON.stringify(r.brain), md: brainLib.renderBrainMarkdown(r.brain) } : { ok: false, reason: r.reason }; });
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'brain-oracle-'));
  const stored = [];
  for (const id of ['c1', 'é😀', '42']) {
    brainLib.writeBrain(dir, id, { brain: brainA, sourceUpdatedAt: ts, builtAt: ts + 1 });
    const file = path.join('chat-brains', `${require('node:crypto').createHash('sha256').update(id).digest('hex')}.json`);
    stored.push({ id, file, bytes: fs.readFileSync(path.join(dir, file), 'utf8'), read: JSON.stringify(brainLib.readBrain(dir, id)) });
  }
  fs.rmSync(dir, { recursive: true, force: true });

  // Builder with a fake reasoner: record the exact call.
  const builds = [];
  const cases = [
    { name: 'built', model: '  reasoner-x ', loaded: 'answer-y', admit: null, reply: '```json\n' + JSON.stringify(brainA) + '\n```' },
    { name: 'budget', model: 'r', loaded: null, admit: 'budget', reply: '' },
    { name: 'no-model', model: '  ', loaded: null, admit: null, reply: '' },
    { name: 'error', model: 'r', loaded: null, admit: null, reply: null },
    { name: 'schema', model: 'r', loaded: null, admit: null, reply: '{"brain_schema":1}' },
    { name: 'empty', model: 'r', loaded: null, admit: null, reply: '', history: hist(['assistant', 'x', { failure: 'f' }], ['user', '  ']) },
    { name: 'long', model: 'r', loaded: null, admit: null, reply: JSON.stringify(brainA), history: hist(['user', long(15000, 'ab😀')], ['assistant', long(15000, 'c<untrusted>d')]) },
  ];
  for (const c of cases) {
    const seen = {};
    const builder = brainLib.createBrainBuilder({
      enabled: () => true, model: () => c.model, loadedModel: () => c.loaded,
      admit: async (m, ctx) => { seen.admit = [m, ctx]; return c.admit; },
      complete: async (req) => { seen.call = JSON.stringify({ model: req.model, schema: req.schema, messages: req.messages }); if (c.reply === null) throw Error('down'); return c.reply; },
    });
    const history = c.history || hist(['user', 'Plan a trip to <Lisbon>'], ['assistant', 'Train is best.', { toolCalls: [{ name: 't' }] }], ['assistant', 'oops', { failure: 'cut' }]);
    const r = await builder.build({ chat: { title: 'Trip & co' }, history });
    builds.push({ name: c.name, model: c.model, loaded: c.loaded, admit: c.admit, reply: c.reply, history: JSON.stringify(history), result: r.ok ? JSON.stringify(r.brain) : r.reason, seen: JSON.stringify(seen) });
  }
  return { parsed, outputs: OUTPUTS, stored, builds, schema: JSON.stringify(brainLib.BRAIN_JSON_SCHEMA), system: brainLib.SYSTEM };
}

(async () => {
  const mirror = [];
  for (const s of MIRROR) mirror.push({ name: s.name, steps: JSON.stringify(s.steps), passes: await runMirror(s) });
  const brain = await runBrain();
  process.stdout.write(JSON.stringify({ core: process.env.ORACLE_CORE || 'unknown', node: process.version, mirror, brain }, null, 1) + '\n');
})();
