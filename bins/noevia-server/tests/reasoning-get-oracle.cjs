'use strict';
// Run against the pre-move Core checkout (fixtures/reasoning-get-oracle.core-ref): the actual GET
// /api/reasoning-settings route over the actual reasoning-effort.cjs and provider registry. The settings table, the
// projects and the providers are synthetic. Records the answer and what `modeFor` was asked, so the native route's
// bridge call can be checked against it. Usage: CORE_ROOT=<core> node reasoning-get-oracle.cjs > out.json
console.log = console.warn = console.error = () => {};
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const { Readable } = require('node:stream');
const CORE = process.env.CORE_ROOT;
const { createReasoningSettingsRoutes } = require(CORE + '/server/routes/reasoning-settings.cjs');
const reasoningEffort = require(CORE + '/server/reasoning-effort.cjs');
const { createProviderRegistry } = require(CORE + '/server/providers.cjs');
const { createWorkspaceStore } = require(CORE + '/server/workspace.cjs');
const { createStubStore } = require(CORE + '/tests/server/project-store-stub.cjs');
const { createProjectStore } = require(CORE + '/server/projects.cjs');

const USER = '11111111-2222-4333-8444-555555555555';
const PROVIDERS = [
  { id: 'default', label: 'Default', baseUrl: 'http://engine.invalid/v1', apiKey: 'k0', capabilities: {} },
  { id: 'real', label: 'Real', baseUrl: 'http://api.invalid/v1', apiKey: 'k1', capabilities: { reasoningEffortParam: true } },
  { id: 'scoped', label: 'Scoped', baseUrl: 'http://api2.invalid/v1', apiKey: 'k2', capabilities: { reasoningEffortParam: true, reasoningEffortModels: ['m-ok'] } },
];
const { getProvider } = createProviderRegistry({ PROVIDERS, DEFAULT_PROVIDER_ID: 'default' });
const p = (id, extra = {}) => ({ id, name: id, files: [], modes: ['chat'], chats: [], ...extra });
const S = (v) => JSON.stringify(v, null, 2);
// The route's own getProject is the real one (projects.cjs: the routing repair of a never-chosen context project and the
// instruction-skills reconcile, each saved through the workspace), over the real workspace store and the project-store stand-in.
const MAIN = [
  p('plain'), p('hi', { reasoningEffort: 'high' }), p('lo', { reasoningEffort: 'low', provider: 'real', model: 'any' }),
  p('def', { reasoningEffort: 'default', provider: 'real', model: 'any' }), p('bad', { reasoningEffort: 'max', provider: 'real', model: 'any' }),
  p('upper', { reasoningEffort: 'HIGH' }), p('null-effort', { reasoningEffort: null, provider: 'scoped', model: 'm-ok' }),
  p('scoped-ok', { provider: 'scoped', model: 'm-ok' }), p('scoped-other', { provider: 'scoped', model: 'other' }),
  p('ghost-provider', { provider: 'ghost', model: 'm-ok' }), p('legacy-provider', { provider: 'lemonade', model: 'm' }),
  p('empty-provider', { provider: '', model: '' }), p('number-provider', { provider: 5, model: 7 }), p('object-provider', { provider: { id: 'real' }, model: ['m'] }),
  p('null-provider', { provider: null, model: null }), p('engine-local', { provider: 'default', model: 'synthetic-8b' }),
  p('surrogate', { reasoningEffort: 'high', provider: 'real\ud800x', model: 'm\udc00' }),
  p('dup', { reasoningEffort: 'low' }), p('dup', { reasoningEffort: 'high' }), p('café ☕'), p('a b'), p('a+b'), p('50%'),
];
// A skill source as the front writes one: its text is in its own file, found by textRef, and flagged skillHeader.
const SKILL = [{ name: 'x/SKILL.md', textRef: 'a'.repeat(64), chars: 40, skillHeader: true }];
const REPAIR = [
  p('clean', { reasoningEffort: 'high' }),
  p('cowork-chat-context-heal', { routing: 'manual', reasoningEffort: 'high' }),
  p('cowork-chat-context-chosen', { routing: 'manual', routingChosen: true, reasoningEffort: 'high' }),
  p('cowork-chat-context-auto', { routing: 'auto', reasoningEffort: 'high' }),
  p('not-a-context', { routing: 'manual', reasoningEffort: 'high' }),
  p('skill', { files: SKILL, reasoningEffort: 'high', provider: 'real', model: 'any' }),
  p('tracked-skill', { files: SKILL, instructionSkills: { 'x/SKILL.md': { enabled: true, reviewedHash: 'h' } }, reasoningEffort: 'high' }),
  p('cowork-chat-context-both', { routing: 'manual', files: SKILL, reasoningEffort: 'low' }),
  p('other-skill', { files: SKILL, reasoningEffort: 'high' }),
];
// Un-normalized stored projects, for each thing core's workspace load (workspace.cjs) normalizes or migrates. A project as the
// load leaves it needs a modes list naming chat, no null in files, the default provider id, and its source text in its own file.
const bare = (id, extra = {}) => ({ id, name: id, files: [], chats: [], reasoningEffort: 'high', ...extra });
const FILE = { name: 'a.txt', textRef: 'b'.repeat(64), chars: 3 };
const HEAL = (id) => bare(id, { modes: ['chat'], routing: 'manual' });
const MIGRATE = {
  // #1576: modes names chat first and keeps cowork / code in that order; a project without a list gets ['chat'].
  'mig-modes': S({ projects: [bare('m-none'), bare('m-code', { modes: ['code'] }), bare('m-cowork', { modes: ['cowork'] }), bare('m-order', { modes: ['code', 'cowork', 'chat', 'x'] }), bare('m-str', { modes: 'chat' }), bare('m-ok', { modes: ['chat'] }), bare('m-empty', { modes: [] }), bare('m-null', { modes: null })] }),
  // Normalizations that are not themselves saved (#1236 files, lemonade): in memory until some save carries them.
  'mig-memory-only': S({ projects: [bare('lem', { modes: ['chat'], provider: 'lemonade', model: 'm' }), bare('fnull', { modes: ['chat'], files: [null, 5, 'x', FILE, [1]] }), HEAL('cowork-chat-context-h'), bare('plain', { modes: ['chat'] })] }),
  // A null in files makes the reconcile throw unless the load dropped it first.
  'mig-files-null-skill': S({ projects: [bare('fs', { modes: ['chat'], files: [null, { name: 'x/SKILL.md', textRef: 'a'.repeat(64), chars: 40, skillHeader: true }] }), bare('ok', { modes: ['chat'] })] }),
  // #1806: text kept inline moves to its own file, with a backup of projects.json first.
  'mig-inline': S({ projects: [bare('inl', { modes: ['chat'], files: [{ name: 'a.txt', content: 'hello' }, { name: 's/SKILL.md', content: '---\nname: x\ndescription: d\n---\nBody' }, { name: 'e.txt', content: '' }, { name: 'n.txt', content: 5 }, { name: 'u.txt', content: 'café 😀 \ud800' }] }), bare('other', { modes: ['chat'] })] }),
  'mig-everything': S({ projects: [bare('all', { modes: ['code'], provider: 'lemonade', files: [null, { name: 'a.txt', content: 'hi' }] }), HEAL('cowork-chat-context-h2')] }),
  // project-modes.cjs migrate() accepts any object, an array included (it sets a property JSON never carries).
  'mig-before-reconcile-error': S({ projects: [bare('bad-files', { modes: ['code'], files: {} })] }),
  'mig-array-entry': S({ projects: [[1], bare('ok', { modes: ['chat'] })] }),
  // A backup that cannot be written: the modes stay as stored (#1839), and inline text refuses the load.
  'mig-backup-blocked': S({ projects: [bare('m-code', { modes: ['code'] }), HEAL('cowork-chat-context-h')] }),
  'mig-inline-backup-blocked': S({ projects: [bare('inl', { modes: ['chat'], files: [{ name: 'a.txt', content: 'hello' }] })] }),
};
const BLOCK_BACKUPS = new Set(['mig-backup-blocked', 'mig-inline-backup-blocked']);
const SETS = {
  ...MIGRATE,
  main: S({ projects: MAIN }),
  repair: S({ projects: REPAIR }),
  // A project that is not an object makes the workspace load throw before the route runs: a 500 with or without a query.
  'null-project': S({ projects: [p('a'), null] }),
  missing: null,
};
const SETTINGS = [
  ['missing', undefined], ['default', 'default'], ['low', 'low'], ['high', 'high'], ['empty', ''], ['null', null], ['unknown', 'max'], ['upper', 'LOW'],
  ['blob', Buffer.from('6c6f77', 'hex')], ['blob-empty', Buffer.alloc(0)],
];
(async () => {
  const cases = [];
  const realNow = Date.now;
  const mainQueries = [
    ['none', ''], ['plain', '?projectId=plain'], ['hi', '?projectId=hi'], ['lo', '?projectId=lo'], ['def', '?projectId=def'], ['bad', '?projectId=bad'],
    ['upper', '?projectId=upper'], ['null-effort', '?projectId=null-effort'], ['scoped-ok', '?projectId=scoped-ok'], ['scoped-other', '?projectId=scoped-other'],
    ['ghost-provider', '?projectId=ghost-provider'], ['legacy-provider', '?projectId=legacy-provider'], ['empty-provider', '?projectId=empty-provider'],
    ['number-provider', '?projectId=number-provider'], ['object-provider', '?projectId=object-provider'], ['null-provider', '?projectId=null-provider'],
    ['engine-local', '?projectId=engine-local'], ['surrogate', '?projectId=surrogate'], ['dup', '?projectId=dup'], ['unknown', '?projectId=ghost'], ['empty-id', '?projectId='], ['bare', '?projectId'],
    ['encoded', '?projectId=caf%C3%A9%20%E2%98%95'], ['plus-is-space', '?projectId=a+b'], ['literal-plus', '?projectId=a%2Bb'], ['percent', '?projectId=50%25'],
    ['first-wins', '?projectId=hi&projectId=lo'], ['first-unknown', '?projectId=ghost&projectId=hi'], ['other-param', '?other=1'], ['case-sensitive', '?projectid=hi'],
    ['trailing-amp', '?projectId=hi&'], ['bad-percent', '?projectId=%ZZ'],
  ];
  const repairQueries = [['none', ''], ...REPAIR.map((x) => [x.id, '?projectId=' + x.id]), ['unknown', '?projectId=ghost']];
  const plan = [];
  for (const [skind, svalue] of SETTINGS) for (const [qname, search] of mainQueries) {
    if (skind !== 'low' && skind !== 'missing' && !['none', 'plain', 'hi', 'def', 'bad', 'scoped-ok', 'unknown', 'dup'].includes(qname)) continue;
    for (const role of ['member', 'admin']) {
      if (role === 'admin' && !['none', 'plain', 'hi', 'unknown'].includes(qname)) continue;
      plan.push({ set: 'main', skind, svalue, qname, search, role });
    }
  }
  for (const [skind, svalue] of SETTINGS.filter(([k]) => ['low', 'missing', 'blob'].includes(k))) for (const [qname, search] of repairQueries) plan.push({ set: 'repair', skind, svalue, qname, search, role: 'member' });
  const migQueries = {
    'mig-modes': ['m-code', 'm-ok', 'm-none', 'ghost'],
    'mig-memory-only': ['lem', 'fnull', 'plain', 'cowork-chat-context-h'],
    'mig-files-null-skill': ['fs', 'ok'],
    'mig-inline': ['inl', 'other'],
    'mig-everything': ['all', 'cowork-chat-context-h2'],
    'mig-array-entry': ['ok'],
    'mig-before-reconcile-error': ['bad-files'],
    'mig-backup-blocked': ['m-code', 'cowork-chat-context-h'],
    'mig-inline-backup-blocked': ['inl'],
  };
  for (const [set, ids] of Object.entries(migQueries)) for (const [qname, search] of [['none', ''], ...ids.map((id) => [id, '?projectId=' + id])]) plan.push({ set, skind: 'low', svalue: 'low', qname, search, role: 'member' });
  for (const [qname, search] of [['none', ''], ['a', '?projectId=a'], ['unknown', '?projectId=ghost']]) plan.push({ set: 'null-project', skind: 'low', svalue: 'low', qname, search, role: 'member' });
  for (const [qname, search] of [['none', ''], ['plain', '?projectId=plain']]) plan.push({ set: 'missing', skind: 'high', svalue: 'high', qname, search, role: 'member' });
  for (const c of plan) {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'synthetic-reasoning-'));
    const dir = path.join(root, 'users', USER);
    fs.mkdirSync(dir, { recursive: true });
    if (SETS[c.set] !== null) fs.writeFileSync(path.join(dir, 'projects.json'), SETS[c.set]);
    if (BLOCK_BACKUPS.has(c.set)) fs.writeFileSync(path.join(dir, 'migration-backups'), 'in the way');
    const asked = [], sent = [], saves = [];
    let loadThrew = false;
    try {
      const store = createWorkspaceStore(root, { id: 'default', label: 'Default', baseUrl: '', apiKey: '' }, null, { projectStore: createStubStore(root), onStoreSave: (x) => saves.push(x) });
      let ws;
      try { ws = store.get(USER); } catch { loadThrew = true; sent.push({ status: 500, body: { error: 'Internal error' } }); }
      if (!loadThrew) {
        const projectStore = createProjectStore({ fs, path, reasoningEffort, currentWorkspace: () => ws, PROJECTS: { find: (f) => ws.projects.find(f) } });
        const routes = createReasoningSettingsRoutes({
          json: (_res, status, body) => sent.push({ status, body }),
          authService: { db: { prepare: () => ({ get: () => (c.svalue === undefined ? undefined : { value: c.svalue }) }) } },
          getProject: projectStore.getProject,
          getProvider,
          reasoningEffort: { ...reasoningEffort, modeFor: (provider, model, effort) => { asked.push({ provider: provider ? provider.id : null, model, effort }); return reasoningEffort.modeFor(provider, model, effort); } },
          DEFAULT_PROVIDER_ID: 'default',
        });
        const req = Readable.from([]); req.method = 'GET';
        try { await routes(req, {}, { path: '/api/reasoning-settings', authn: { user: { id: USER, role: c.role } }, url: new URL('http://localhost/api/reasoning-settings' + c.search) }); }
        catch { sent.push({ status: 500, body: { error: 'Internal error' } }); }
        await Promise.all(saves);
      }
    } finally { Date.now = realNow; }
    const after = (() => { try { return fs.readFileSync(path.join(dir, 'projects.json'), 'utf8'); } catch { return null; } })();
    // What else the load and the read wrote into the account's folder: the backups and the moved source texts.
    const extras = {};
    const walk = (rel) => { for (const e of fs.readdirSync(path.join(dir, rel), { withFileTypes: true })) {
      const r = path.join(rel, e.name);
      if (e.isDirectory()) walk(r);
      else if (/^(migration-backups|project-sources)\//.test(r) && !r.endsWith('.tmp')) extras[r] = fs.readFileSync(path.join(dir, r), 'utf8');
    } };
    walk('');
    const value = Buffer.isBuffer(c.svalue) ? { hex: c.svalue.toString('hex') } : c.svalue;
    cases.push({ name: `${c.set}/${c.skind}/${c.qname}/${c.role}`, set: c.set, settings: c.skind === 'missing' ? null : { kind: c.skind, value }, search: c.search, role: c.role,
      reply: sent[0], loadThrew, asked, ...(after !== SETS[c.set] ? { projectsAfter: after } : {}), ...(Object.keys(extras).length ? { extras } : {}) });
    fs.rmSync(root, { recursive: true, force: true });
  }
  process.stdout.write(JSON.stringify({ node: process.version, user: USER, sets: SETS, cases }, null, 1));
})().catch((e) => { process.stderr.write(String(e && e.stack || e) + "\n"); process.exitCode = 1; });
