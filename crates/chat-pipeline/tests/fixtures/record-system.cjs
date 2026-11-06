#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat.cjs handleChat through the system message of a turn
// (chat.cjs:599-752 and the #815/#1587 off-box rewrite): account instructions and memory, the date part, the project's
// name/goal/instructions/memories, shared Code tasks, the knowledge-file block (in the system text, or before the user
// message of a free chat's attachments), sources not ready, documents waiting, partial reads, the skills index and the
// auto-loaded skill, stored-only sources. Records the parts' inputs as core's own helpers compute them, and the system
// and last user message the first model request carries; plus the helpers called directly. Synthetic data only.
// Needs dav-parse.wasm: DAV_PARSE_WASM. The date is fixed (NOEVIA_CONTRACT_FIXED_NOW).
//   node record-system.cjs <noevia-core checkout> <output.json>
'use strict';
process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-10-12T12:00:00Z';
process.env.TZ = 'Europe/Berlin';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const { EventEmitter } = require('node:events');
const core = path.resolve(process.argv[2]);
const S = (m) => require(path.join(core, 'server', m));
const { createChatHandler } = S('chat.cjs');
const { createToolExchange } = S('tool-exchange.cjs');
const { createVisionProbe } = S('vision.cjs');
const { missingRoles, staleRolesError } = S('auto-roles-check.cjs');
const tb = S('toolboxes.cjs');
const egress = S('provider-egress.cjs');
let seed = 77;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const subset = (xs) => xs.filter(() => rnd(3) > 0);
const T = (name, desc = '') => ({ type: 'function', function: { name, description: desc, parameters: { type: 'object', properties: { q: { type: 'string' } } } } });
const long = (n) => 'Synthetic description. '.repeat(n);
const BOXES = [
  { id: 'web-search', label: 'Web search', source: 'builtin', tools: [T('web_search', 'Search the web')], reads: ['web_search'] },
  { id: 'offline-wikipedia', label: 'Wikipedia', source: 'builtin', tools: [T('wiki_search')], reads: ['wiki_search'] },
  { id: 'project-docs', label: 'Project documents', source: 'builtin', tools: [T('search_project_docs')], reads: ['search_project_docs'] },
  { id: 'diary', label: 'Diary', source: 'builtin', tools: [T('diary_search'), T('diary_write')], reads: ['diary_search'] },
  { id: 'gdrive', label: 'Drive', source: 'builtin', tools: [T('drive_list'), T('drive_upload')], reads: ['drive_list'] },
  { id: 'big', label: 'Big box', tools: Array.from({ length: 14 }, (_, k) => T(`big_${k}`, long(30 + k))), reads: ['big_0', 'big_1'] },
  { id: 'huge', tools: [T('huge_read', long(300)), T('web_search', 'dup name')], reads: ['huge_read'] },
];
const MCP = [
  { id: 'nextcloud-notes', label: 'Notes', inApp: true, server: 'nextcloud', tools: [T('nc_notes_list'), T('nc_notes_write')], reads: ['nc_notes_list'] },
  { id: 'nextcloud-files', label: 'Files', inApp: true, server: 'nextcloud', tools: [T('nc_files_list')], reads: ['nc_files_list'] },
  { id: 'nextcloud-mail', server: 'nextcloud', tools: [T('nc_mail_send')] },
  { id: 'ext-mcp', label: 'Third party', server: 'other', tools: [T('ext_read'), T('nc_notes_list')], reads: ['ext_read'] },
];

const PROVIDERS = { default: { id: 'default', baseUrl: 'http://default.invalid' }, ext: { id: 'ext', kind: 'openai', external: true, label: 'Cloud', baseUrl: 'https://cloud.invalid/v1' } };
const ix = (xs) => xs.map((_, k) => k).filter(() => rnd(3) > 0);
const NAMES = ['Plan', 'Q"uote', 'line break', 'rtl‮eman', 'nel\u0085x', 'tab\tname', 'emoji 😀', 'lone \ud800', 'back\\slash', 'mark‎', 'iso⁦x'];
const ACCOUNTS = [null, { style: 'concise' }, { style: 'detailed', advanced: { length: 'short', tone: 'formal', formatting: 'structured', emoji: 'none' }, language: 'Deutsch', text: 'Call me Sam.' },
  { style: 'nope', advanced: { length: 'long', tone: 'x' }, language: 'not a language 123', text: 'x'.repeat(4100) }, { text: '' }, { language: 'Español (MX)' }];
const MEMORIES = [null, { memories: ['likes tea', ' likes tea ', '', 'lives in Berlin'] }, { memories: ['a'], useProjectMemories: false }, { memories: [] }];
const FILES = [
  { name: 'notes.md', content: 'short text' },
  { name: 'big.txt', content: 'b'.repeat(5000) },
  { name: 'waiting.pdf', content: '', document: { state: 'queued' } },
  { name: 'failed.pdf', content: '', document: { state: 'failed' } },
  { name: 'img.png', attachment: { state: 'stored', group: 'Images' } },
  { name: 'stored.txt', attachment: { state: 'stored', group: 'Text' } },
  { name: 'old.pdf', content: 'earlier text', document: { state: 'failed' } },
  ...Array.from({ length: 11 }, (_, k) => ({ name: `w${k}.pdf`, document: { state: 'queued' } })),
];
const TASKS = [{ task: 'Fix   the\nbuild', status: 'done', branch: 'b1', updatedAt: 5 }, { task: 'x'.repeat(200), error: 'boom '.repeat(50), updatedAt: 9 }, { task: '' }, null, { task: 'old', updatedAt: 1 },
  { task: 't4', updatedAt: 2 }, { task: 't5', updatedAt: 3 }, { task: 't6', updatedAt: 4 }, { task: 't7', status: 'running' }];
const SKILLS = [
  { file: 'skills/a.md', name: 'Alpha', description: 'Does alpha', version: '1', content: '---\nname: Alpha\n---\nDo alpha things.', valid: true },
  { file: 'skills/b.md', name: 'B'.repeat(200), description: 'd'.repeat(600), content: 'No front matter body', valid: true },
  { file: '', name: 'bad' }, { file: 'f'.repeat(501) }, { file: 'skills/c.md', content: '---\r\nx: 1\r\n---\r\n' + 'z'.repeat(12100), name: 'Big' },
  ...Array.from({ length: 40 }, (_, k) => ({ file: `skills/m${k}.md`, name: `M${k}`, description: 'x'.repeat(150) })),
];
const MSGS = ['What does the plan say?', 'hi', 'Summarise big.txt'];
const scenarios = [];
for (let i = 0; i < 900; i++) {
  const kind = pick(['free', 'project', 'project', 'attach']);
  const project = kind === 'free' ? null : {
    id: kind === 'attach' ? 'cowork-chat-context-qa1' : 'fixture-project', name: rnd(5) ? pick(NAMES) : '', goal: rnd(2) ? 'Ship the synthetic thing' : undefined,
    instructions: rnd(2) ? 'Answer in bullet points.\nBe brief.' : undefined, memories: rnd(2) ? ['project fact', 'likes tea', 7] : undefined,
    sharedContext: rnd(3) === 0 ? { chat: true } : undefined, files: rnd(4) ? ix(FILES) : [],
  };
  scenarios.push({ kind, project, account: pick(ACCOUNTS), memory: pick(MEMORIES), providerId: pick(['default', 'default', 'ext']),
    message: pick(MSGS), filesBlock: rnd(3) ? pick(['[file notes.md]\nshort text', 'excerpt two']) : null, injectBig: rnd(2) ? pick([0, 1200, 6000]) : null,
    tasks: rnd(4) === 0 ? 'throw' : subset(TASKS), notReady: rnd(4) === 0 ? subset([{ name: 'new.pdf', reason: 'failed' }, { name: 'n 2.docx', reason: 'timeout' }]) : [],
    skills: rnd(3) ? ix(SKILLS) : [], autoSkill: rnd(2) });
}
const DONE = Symbol('upstream');
async function run(sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-system-'));
  try {
    if (sc.account) fs.writeFileSync(path.join(dir, 'account-instructions.json'), JSON.stringify(sc.account));
    if (sc.memory) fs.writeFileSync(path.join(dir, 'account-memory.json'), JSON.stringify(sc.memory));
    const events = [], res = new EventEmitter();
    res.writeHead = () => {}; res.setHeader = () => {}; res.write = (line) => events.push(JSON.parse(line.slice(6))); res.end = () => { res.writableEnded = true; res.emit('finish'); };
    let answer = null, upstream = null, partialReads = [], autoPicked = [];
    const fetch = async (url, init) => { const b = JSON.parse(init.body); upstream = { system: b.messages[0]?.role === 'system' ? b.messages[0].content : null, last: b.messages[b.messages.length - 1] }; throw DONE; };
    const project = sc.project && { routing: 'manual', ...sc.project, files: sc.project.files.map((k) => FILES[k]), model: 'synth-70b', provider: sc.providerId, assets: [], chats: [{ id: 'fixture-chat' }] };
    const sources = S('chat-sources.cjs'), coverage = S('source-coverage.cjs');
    const rag = { filesContext: async (_id, readable, _m, _u, onSources) => {
      const placed = sc.injectBig === null ? [] : [{ file: 'big.txt', body: 'b'.repeat(sc.injectBig) }];
      onSources(placed, placed); partialReads = coverage.partialSources(readable, placed); return sc.filesBlock; } };
    const { handleChat } = createChatHandler({
      modelManager: { enabled: false }, reasoningEffort: S('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => true, getStorage: () => null },
      crypto: require('node:crypto'), path, fs, fetch, HISTORY_CAP: 20, DEFAULT_PROVIDER_ID: 'default', createToolExchange,
      currentWorkspace: () => ({ userId: 'u-synth', dir, assetDir: () => '/synthetic-only' }),
      sourceJobs: { settle: async () => ({ aborted: false, notReady: sc.notReady }) },
      codeTasksFor: () => { if (sc.tasks === 'throw') throw new Error('code off'); return sc.tasks; },
      getProject: (id) => (project && id === project.id ? project : null), getProvider: (id) => PROVIDERS[id] || null, providerHeaders: () => ({}),
      autoRoles: () => (project ? null : { fast: 'synth-70b', smart: 'synth-70b' }), classifyFastOrSmart: async () => 'smart', // Smart: no low-effort hint (a later stage) servedCatalogue: async () => [{ name: 'synth-70b' }], modelsInstalled: async () => [{ name: 'synth-70b' }],
      missingRoles, staleRolesError, lastLoadedModel: () => 'synth-70b', skillsIndexFor: () => sc.skills.map((k) => SKILLS[k]), visionDescriptions: new Map(), visionProbe: createVisionProbe({ fetchImpl: fetch }),
      chatSkillRouter: { select: async (candidates) => { autoPicked = sc.autoSkill && candidates.length ? [candidates[0]] : []; autoPicked = autoPicked.map((c) => SKILLS.indexOf(c)); return { loaded: autoPicked.map((k) => SKILLS[k]) }; } },
      oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
      chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: ['core'], CONNECTOR_BOXES: new Set(), connectedBoxes: () => [],
      toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({ workspace: { userId: 'u-synth' }, authn: { user: { id: 'u-synth' } } }) },
      resolveTools: () => ({ tools: [], dropped: [], routes: [] }), isWriteTool: () => false, allToolboxes: () => [],
      rag, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras: S('diary-extras.cjs'),
      DIARY_BASE: 'http://fixture.invalid', TOOL_RESULT_CAP: 8000, json: (_res, status, payload) => { answer = answer || { status, payload }; }, saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
      freeChats: () => [{ id: 'fixture-chat' }],
      grantContext: async (raw) => require(path.join(core, 'tests', 'server', 'chat-grant-fixture.cjs')).context(raw, { holder: { kind: 'absent' } }),
      chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'decline',
    });
    const body = { chatId: 'fixture-chat', message: sc.message, history: [], ...(project ? { projectId: project.id } : {}) };
    let thrown = null;
    try { await handleChat({ headers: {} }, res, body, { user: { id: 'u-synth' } }); } catch (e) { if (e !== DONE) thrown = `${e.constructor.name}: ${e.message}`; }
    const ai = S('account-instructions.cjs'), am = S('account-memory.cjs'), tp = S('current-time-part.cjs');
    const inputs = { accountPart: ai.systemPart(ai.read(dir)), memoryPart: am.systemPart(am.read(dir), project?.memories), timePart: tp.systemPart(), zoneless: tp.systemPart(undefined, undefined, { zoneName: false }),
      partialReads, autoSkills: autoPicked, codeTasks: project && S('shared-context.cjs').read(project).chat ? (sc.tasks === 'throw' ? [] : sc.tasks) : null };
    return { inputs, upstream, refused: answer, thrown, other: upstream || answer || thrown ? undefined : events.map((e) => (e.type === 'error' ? `error:${e.code || ''}:${e.text}` : e.type)).join(',') };
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}
(async () => {
  const out = { FILES, SKILLS, core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), units: {}, cases: [] };
  const log = console.log, warn = console.warn; console.log = () => {}; console.warn = () => {};
  const q = S('quoted-name.cjs'), si = S('skill-index.cjs'), sr = S('source-readability.cjs'), cov = S('source-coverage.cjs'), csr = S('chat-skill-routing.cjs'), sh = S('shared-context.cjs');
  out.units.quote = NAMES.map((n) => [n, q.quoteName(n)]);
  out.units.index = []; for (let k = 0; k < 80; k++) { const s = ix(SKILLS); out.units.index.push([s, si.formatSkillIndex(s.map((j) => SKILLS[j]))]); }
  out.units.waiting = []; for (let k = 0; k < 80; k++) { const f = ix(FILES).map((j) => FILES[j]); out.units.waiting.push([f.map((x) => FILES.indexOf(x)), sr.waitingNotice(f), sr.readable(f).map((x) => x.name)]); }
  out.units.coverage = [[[], cov.coverageNote([])], ...[[{ name: 'a"b', length: 1234567, read: 0 }], [{ name: 'x', length: 5000, read: 1200 }, { name: 'y ', length: 99999, read: 2500 }]].map((p) => [p, cov.coverageNote(p)])];
  out.units.skillBlock = SKILLS.slice(0, 5).map((s) => [s, csr.skillBlock([s])]);
  out.units.pinned = SKILLS.slice(0, 5).map((s) => { const r = { record: { file: s.file, name: s.name, contentHash: 'ab'.repeat(32) }, content: String(s.content || '') }; return [r, csr.pinnedSkillBlock(r)]; });
  out.units.tasks = []; for (let k = 0; k < 60; k++) { const t = subset(TASKS); out.units.tasks.push([t, sh.forChat({ sharedContext: { chat: true } }, t)]); }
  const sha = (t) => require('node:crypto').createHash('sha256').update(t).digest('hex');
  for (const [k, sc] of scenarios.entries()) {
    const o = await run(sc);
    // The system text by hash (the first cases in full, for reading a difference).
    if (o.upstream) o.upstream = { systemSha: o.upstream.system === null ? null : sha(o.upstream.system), ...(k < 40 ? { system: o.upstream.system } : {}), last: o.upstream.last };
    out.cases.push({ ...sc, out: o });
  }
  console.log = log; console.warn = warn;
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const kinds = {};
  for (const c of out.cases) { const o = c.out; const k = o.thrown ? `thrown ${o.thrown}` : o.other ? `other ${o.other.slice(0, 100)}` : o.refused ? `refused ${o.refused.status} ${String(o.refused.payload.error).slice(0, 60)}` : `upstream sys=${o.upstream.system ? 'y' : 'n'}`; kinds[k] = (kinds[k] || 0) + 1; }
  console.log(JSON.stringify(kinds, null, 1));
})();
