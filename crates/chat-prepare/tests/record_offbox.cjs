#!/usr/bin/env node
'use strict';
// Recorded off-box wire changes for crates/chat-prepare (slice P5d, part 2): the contract src/offbox.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_offbox.cjs > crates/chat-prepare/tests/fixtures/node-offbox.v1.json
//   ... node crates/chat-prepare/tests/record_offbox.cjs --check crates/chat-prepare/tests/fixtures/node-offbox.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named (hand-made edges plus a
// seeded PRNG), passed through JSON first so Node reads exactly what is recorded:
//   turns  chat.cjs createChatHandler().handleChat, whole Automatic free-chat turns (no project, so the system text is
//          the account, time and memory parts plus the brain block) with the routingModes seams stubbed as in
//          record_modes.cjs. The linked brain block is a stub: chat-frame-steering.cjs storedFrame answers a confirmed
//          frame when the case has a brain (frameBlock answers '', so no frame part) and chat-brain.cjs linkedBrainBlock
//          answers the case's block text; brainContext.enabled() is true. provider-egress.cjs isExternalProvider and
//          stripPrivateToolboxes are wrapped (the real answer is recorded), chatToolRouter.select records the boxes it is
//          given. Recorded: the routing seams in order up to the meta event, the external answers, the boxes after the
//          egress strip and after the #1646 strip, the meta event's model and routing, the `[routing]` lines, and the
//          reply request's model, url, messages and offered tool names (a compactOnly turn: the wire chat-context.cjs
//          prepare is given); or what handleChat threw. Project turns stub rag.filesContext with the case's block.
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const logged = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) {
  console[k] = (...a) => { const line = a.map(String).join(' '); logged.push(line); process.stderr.write(`${line}\n`); };
}
const READ_FILES = ['server/chat.cjs', 'server/index.cjs', 'server/routing-modes.cjs', 'server/current-time-part.cjs', 'server/chat-brain.cjs', 'server/chat-frame-steering.cjs', 'server/chat-context.cjs', 'server/rag.cjs', 'server/source-coverage.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const serverRequire = (f) => require(path.join(root, 'server', f));
const indexText = fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8');
for (const wiring of ['    awaitChoice: awaitRouteChoice,', '    openChoice: openRouteChoice,', "    enabled: () => features.enabled('routingModes'),"]) {
  if (!indexText.includes(wiring)) throw new Error(`index.cjs no longer holds ${JSON.stringify(wiring)}`);
}
const toolboxesLib = serverRequire('toolboxes.cjs');
const { createDriveTools } = serverRequire('gdrive-tools.cjs');
const pageSave = serverRequire('specialist-page-save.cjs');
const { missingRoles, staleRolesError } = serverRequire('auto-roles-check.cjs');
const diaryExtras = serverRequire('diary-extras.cjs');
const rm = serverRequire('routing-modes.cjs');
const { createChatHandler } = serverRequire('chat.cjs');
let current = null;
// chat.cjs calls rm.preRule off the module: the real rule answers, the call is logged.
const realPreRule = rm.preRule;
rm.preRule = (text, opts) => { const flag = realPreRule(text, opts); current.seam(['preRule', String(text), flag]); return flag; };
// mappedHistory as chat.cjs built it (hasDiaryContent's argument): the history the Rust side is given.
const realHasDiary = rm.hasDiaryContent;
rm.hasDiaryContent = (history) => { if (current.log.mappedHistory === null) current.log.mappedHistory = structuredClone(history); return realHasDiary(history); };
const routePreload = serverRequire('route-preload.cjs');
routePreload.startPreload = async ({ model }) => ({ model, use() {}, cancel() {}, releaseIfUnused: async () => {}, pending: () => false, settled: Promise.resolve() });

// The brain block (#742) as a stub: a case with `brain` has a confirmed frame and that block; the frame adds no part.
const steering = serverRequire('chat-frame-steering.cjs');
const brainLib = serverRequire('chat-brain.cjs');
steering.storedFrame = () => (current.spec.brain ? { kind: 'question', confirmed: true, links: ['qa-linked'], tags: [] } : null);
steering.frameBlock = () => '';
brainLib.linkedBrainBlock = () => current.spec.brain || '';
const egress = serverRequire('provider-egress.cjs');
const realExternal = egress.isExternalProvider;
egress.isExternalProvider = (provider, ...rest) => { const out = realExternal(provider, ...rest); current.log.external.push([provider ? provider.id : null, out]); return out; };
const realStrip = egress.stripPrivateToolboxes;
egress.stripPrivateToolboxes = (selected, ...rest) => { const out = realStrip(selected, ...rest); current.log.egressBoxes = [...selected]; return out; };

// A compactOnly turn sends no reply request: the wire it compacts (context.prepare's messages, force) is recorded.
const chatContext = serverRequire('chat-context.cjs');
const realContextPrepare = chatContext.prepare;
chatContext.prepare = (args, ...rest) => { if (args && args.force === true) current.log.compactWire = structuredClone(args.messages); return realContextPrepare(args, ...rest); };

let seed = 0x2026_1114 ^ 0x5d2;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// One private-data MCP box (#1646): a turn that picks it on an ungranted hybrid cloud route moves local ('private-tools').
const MCP = [{ id: 'qa-notes', label: 'QA notes', description: 'QA notes for QA', source: 'mcp', server: 'qa-notes',
  tools: [{ type: 'function', function: { name: 'qa_notes_search', description: 'Search QA notes.', parameters: { type: 'object', properties: {} } } }], reads: ['qa_notes_search'] }];
const driveTools = createDriveTools({ accounts: { forUser: () => ({ drive: { state: () => ({ state: 'none' }) } }) }, cap: 8000 });
const TB = toolboxesLib.createToolboxes({ boxes: [driveTools.box], kiwixTools: null, driveTools, specialistTools: pageSave.createPageSave({ store: {} }),
  mcpBoxes: () => MCP, mcpTools: () => new Map([['qa_notes_search', { readOnly: true, serverId: 'qa-notes' }]]), offered: () => true, prefill: { budgetFor: () => null, rateFor: () => 0.5 } });
const PROVIDER = 'http://qa-default.invalid';
const PROVIDERS = {
  default: { id: 'default', label: 'QA local', baseUrl: PROVIDER },
  'qa-cloud': { id: 'qa-cloud', label: 'QA Cloud', baseUrl: `${PROVIDER}/qa-cloud`, shared: true },
  'qa-other': { id: 'qa-other-id', label: 'QA mismatched', baseUrl: `${PROVIDER}/qa-other`, shared: true },
};
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-offbox-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));
const FAST = 'qa-fast-4b', SMART = 'qa-smart-27b', CODE = 'qa-code-14b';
const ROLES = { fast: FAST, smart: SMART, code: CODE };
const CAT = [FAST, SMART, CODE].map((name) => ({ name, labels: [], loaded: true }));
const settled = (v) => (v && v.throws ? Promise.reject(new Error('QA seam failed')) : Promise.resolve(structuredClone(v)));

function handlerFor(dir, spec) {
  const S = () => current.spec, L = () => current.log;
  const seam = (entry) => L().seam(entry);
  let uuid = 0;
  const qaCrypto = Object.assign(Object.create(crypto), { randomUUID: () => { const id = `qa-choice-${uuid += 1}`; seam(['randomUUID', id]); return id; } });
  const fetch = async (url, init) => {
    if (!String(url).startsWith(PROVIDER)) throw new Error('QA: no network');
    L().requests.push({ url: String(url), body: JSON.parse(init.body) });
    return { ok: true, body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\n`); })() };
  };
  const m = () => S().modes;
  const routingModes = {
    enabled: () => { seam(['enabled']); return structuredClone(m().enabled); },
    settings: (project) => { seam(['settings', project ? project.id : null]); if (m().settings && m().settings.throws) throw new Error('QA'); return structuredClone(m().settings); },
    // `abortAt`: the response closes while that seam runs (chatSignal aborts).
    sensitivity: (input) => { seam(['sensitivity', input.complete, input.chunks.length]); if (m().abortAt === 'sensitivity') current.res.emit('close'); return settled(m().verdict); },
    awaitChoice: ({ id, userId, chatId }) => { seam(['awaitChoice', id, userId, chatId]); if (m().abortAt === 'awaitChoice') current.res.emit('close'); return settled(m().choice); },
    setChatFlags: (projectId, chatId, patch) => { seam(['setChatFlags', projectId, chatId, patch]); if (m().flagsThrow) throw new Error('QA'); },
    log: (entry) => { seam(['log', entry]); if (m().logThrows) throw new Error('QA'); },
    ...(spec.modes.openChoice === undefined ? {} : { openChoice: ({ id, userId, chatId }) => { seam(['openChoice', id, userId, chatId]); return settled(m().openChoice); } }),
  };
  return createChatHandler({
    modelManager: { enabled: true, health: async () => ({ ok: true, body: { all_models_loaded: CAT.map((r) => ({ model_name: r.name, loaded: true, recipe_options: { ctx_size: 1048576 } })) } }) },
    templateToolsGate: { allowsTools: async () => true, markUnsupported() {} }, toolGate: null, routingModes,
    reasoningEffort: serverRequire('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => true }, crypto: qaCrypto, path, fs, fetch,
    HISTORY_CAP: 1000, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: 'qa-user', dir, assetDir: () => '/qa-only' }),
    getProject: (id) => { if (L().modesOn) seam(['getProject', id]); return S().project && id === S().project.id ? structuredClone(S().project) : null; }, skillsIndexFor: () => [],
    freeChats: () => { if (L().modesOn) seam(['freeChats']); return structuredClone(S().freeChats || []); },
    getProvider: (id) => { seam(['getProvider', id]); return structuredClone(PROVIDERS[id] ?? null); }, providerHeaders: () => ({}),
    autoRoles: () => { seam(['autoRoles']); return structuredClone(S().roles ?? null); },
    visionDescriptions: new Map(), visionProbe: async () => ({ supported: false, reason: 'qa' }),
    chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true,
    chatToolRouter: { select: async (ids) => { L().routedBoxes = [...ids]; return { ids, routed: false }; } },
    brainContext: { enabled: () => true, chats: () => [], read: () => null, maxChars: () => 4000 },
    DEFAULT_TOOLBOXES: TB.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: TB.CONNECTOR_BOXES, connectedBoxes: TB.connectedBoxes, allToolboxes: TB.allToolboxes,
    resolveTools: TB.resolveTools, isWriteTool: TB.isWriteTool,
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => (S().noUser ? {} : { workspace: { userId: 'qa-user' } }) },
    rag: { filesContext: async () => (S().filesBlock === undefined ? null : S().filesBlock) }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras,
    DIARY_BASE: 'http://qa-diary.invalid', TOOL_RESULT_CAP: 8000,
    json: (res, status, body) => { L().refused = { status, body }; },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
    lastLoadedModel: () => null, modelsInstalled: async () => structuredClone(CAT), servedCatalogue: async () => structuredClone(CAT),
    classifyFastOrSmart: async () => { seam(['classify']); return structuredClone(S().classification ?? { role: 'fast' }); },
    missingRoles, staleRolesError,
    chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
    codeTasksFor: () => [], sourceJobs: { settle: async () => ({ notReady: [], aborted: false }) },
    skillHistory: { scrub: ({ messages }) => ({ messages, removed: [] }), record() {} },
    chatFramingEnabled: () => false,
  });
}

const turns = [];
async function addTurn(name, raw) {
  const spec = viaJson({ message: 'QA question', roles: ROLES, tz: 'UTC', ...raw });
  process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-11-13T23:30:00Z';
  process.env.TZ = spec.tz;
  const log = { requests: [], calls: [], meta: false, refused: null, modesOn: false, mappedHistory: null, external: [], egressBoxes: null, routedBoxes: null, compactWire: null };
  log.seam = (entry) => { if (entry[0] === 'enabled') log.modesOn = true; if (!log.meta) log.calls.push(entry); };
  current = { spec, log, seam: log.seam };
  const rawEvents = [];
  const res = new EventEmitter();
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => { const s = String(line); rawEvents.push(s); if (s.includes('"type":"meta"')) log.meta = true; return true; };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  current.res = res;
  const { handleChat } = handlerFor(fs.mkdtempSync(path.join(tmp, 'qa-')), spec);
  const body = { message: spec.message, ...(spec.project ? { projectId: spec.project.id } : { spaceId: 'free' }), ...(spec.body || {}) };
  const from = logged.length;
  let thrown = null;
  const [now, random] = [Date.now, Math.random];
  Date.now = () => 1763076600000; Math.random = () => 0.5;
  try { await handleChat({ headers: {} }, res, body, { user: { id: 'qa-user' } }); } catch (e) { thrown = e.constructor.name; process.stderr.write(`QA ${name} threw: ${e.stack}\n`); }
  finally { Date.now = now; Math.random = random; }
  await new Promise((resolve) => setImmediate(resolve));
  const events = rawEvents.join('').split('\n\n').filter((x) => x.startsWith('data: ')).map((x) => JSON.parse(x.slice(6)));
  const meta = events.find((e) => e.type === 'meta') || null;
  const lines = logged.slice(from).filter((l) => l.startsWith('[routing]'));
  const reply = log.requests.find((r) => r.body.stream === true) || null;
  if (!thrown && !meta && !log.refused) throw new Error(`${name}: no meta event and no refusal; events: ${rawEvents.join('').slice(0, 600)}`);
  turns.push({ name, spec, body, ended: !!res.writableEnded, calls: log.calls, ...(thrown ? { throws: thrown } : {}), refused: log.refused,
    external: log.external, egressBoxes: log.egressBoxes, routedBoxes: log.routedBoxes, compactWire: log.compactWire,
    meta: meta ? { model: meta.model, route: meta.route ?? null, routing: meta.routing ?? null } : null, lines,
    reply: reply ? { model: reply.body.model, url: reply.url, messages: reply.body.messages, tools: (reply.body.tools || []).map((t) => t.function.name) } : null });
}

const CLOUD = { providerId: 'qa-cloud', fast: 'qa-cloud-fast', smart: 'qa-cloud-smart', code: '' };
const NO_CLOUD = { providerId: '', fast: '', smart: '', code: '' };
const S = (mode, extra = {}) => ({ mode, whenSensitive: 'ask', cloud: CLOUD, ...extra });
const CLEAR = { flagged: false, flag: null };
const SENS = { flagged: true, flag: 'router' };
const CHAT = 'qa-chat-1';
const BRAIN = 'Linked chat notes (untrusted data):\nQA linked chat: QA summary of a QA plan.';
// A history with what the off-box filter changes (#1703): a private tool's result, an applied and a declined write,
// a public-looking unknown tool, and plain turns.
const HIST = [
  { role: 'user', content: 'QA find my note' },
  { role: 'tool', name: 'qa_notes_search', content: 'QA private note text' },
  { role: 'assistant', content: 'QA found it' },
  { role: 'user', content: 'QA save it' },
  { role: 'tool', name: 'qa_notes_search', content: 'QA saved', applied: true, target: 'QA target', args: '{"qa":1}' },
  { role: 'tool', name: 'qa_unknown_tool', content: 'QA not run', declined: true },
  { role: 'assistant', content: 'QA done' },
];
const free = (flags = {}, body = {}) => ({ freeChats: [{ id: CHAT, title: 'QA chat', ...flags }], body: { chatId: CHAT, ...body } });
const PRIV = { turnToolboxes: ['qa-notes'] };

(async () => {
  // No routing modes: on the box, nothing changes.
  await addTurn('local_flag_off', { ...free(), brain: BRAIN, modes: { enabled: false } });
  await addTurn('local_flag_off_history', { ...free({}, { history: HIST }), tz: 'Europe/Berlin', modes: { enabled: false } });
  await addTurn('local_mode_brain', { ...free({}, { history: HIST }), brain: BRAIN, tz: 'America/New_York', modes: { enabled: true, settings: S('local') } });
  // A cloud route: the brain block is dropped, the time part loses its zone, the history is rebuilt off-box.
  await addTurn('cloud_plain', { ...free(), modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_brain', { ...free(), brain: BRAIN, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_brain_zone', { ...free(), brain: BRAIN, tz: 'Europe/Berlin', modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_history', { ...free({}, { history: HIST }), tz: 'Asia/Tokyo', modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_history_brain', { ...free({}, { history: HIST }), brain: BRAIN, tz: 'America/New_York', modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_private_box', { ...free({}, PRIV), modes: { enabled: true, settings: S('cloud') } });
  // Hybrid: granted cloud keeps the private box; ungranted cloud moves local (#1646) or, without a local model, strips it.
  await addTurn('hybrid_clear_ungranted', { ...free({}, { history: HIST }), brain: BRAIN, tz: 'Europe/Berlin', modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('hybrid_clear_allow_cloud', { ...free({ allowCloud: true }, { history: HIST, ...PRIV }), modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('hybrid_user_choice_private', { ...free({}, PRIV), modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_remembered_private', { ...free({}, PRIV), modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud', remember: true } } });
  await addTurn('move_private', { ...free({}, { history: HIST, ...PRIV }), brain: BRAIN, tz: 'Europe/Berlin', modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('move_private_smart', { ...free({}, PRIV), classification: { role: 'smart' }, modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('move_private_code_role', { ...free({}, PRIV), classification: { role: 'code' }, modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('move_private_only_code', { ...free({}, PRIV), roles: { code: CODE }, modes: { enabled: true, settings: S('hybrid', { cloud: { ...CLOUD, code: 'qa-cloud-code' } }), verdict: CLEAR } });
  // No local chat model to move to: the turn stays on the cloud route and the private box is taken out.
  await addTurn('strip_no_local_model', { ...free({}, { history: HIST, ...PRIV }), roles: { vision: FAST }, modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('strip_no_local_model_code', { ...free({}, PRIV), roles: { vision: SMART, creative: CODE }, classification: { role: 'code' }, modes: { enabled: true, settings: S('hybrid', { cloud: { ...CLOUD, code: 'qa-cloud-code' } }), verdict: CLEAR } });
  await addTurn('move_log_throws', { ...free({}, PRIV), modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR, logThrows: true } });
  await addTurn('hybrid_no_cloud_private', { ...free({}, PRIV), modes: { enabled: true, settings: S('hybrid', { cloud: NO_CLOUD }), verdict: CLEAR } });
  await addTurn('hybrid_local_choice_private', { ...free({}, { history: HIST, ...PRIV }), brain: BRAIN, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'local' } } });
  await addTurn('hybrid_compact', { ...free({}, { compactOnly: true, history: HIST, ...PRIV }), message: '', modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('cloud_compact', { ...free({}, { compactOnly: true, history: HIST }), message: '', brain: BRAIN, tz: 'Europe/Berlin', modes: { enabled: true, settings: S('cloud') } });

  // Project turns (#1646): the project's own private box is carried but not wanted (Manual tools, no gate), so an
  // ungranted cloud route keeps the route and loses the box; project files make read_project_file count as stripped.
  // The skill-index lead and both read-file hints ride in the project's instructions and the files block, where
  // dropReader finds them (the first match of each, in every message).
  const SKILL_LEAD = 'Available skills (load the full file with the read_project_file tool when a task matches; do not guess their contents):';
  const COVERAGE_HINT = ' read_project_file with an offset can read more of a file.';
  const RAG_HINT = ' Use read_project_file with PDF startPage/endPage and offset for pages beyond the summary.';
  const PROJ = (flags = {}, extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', routing: 'auto', toolsMode: 'manual', toolboxes: ['core', 'qa-notes'],
    instructions: `QA rules.\n${SKILL_LEAD}\n- qa-skill: QA steps.\nQA long file.${COVERAGE_HINT} QA again.${COVERAGE_HINT}`,
    files: [{ name: 'QA notes.txt', content: 'QA source text' }], chats: [{ id: CHAT, title: 'QA chat', ...flags }], ...extra });
  const inProj = (flags, body = {}) => ({ project: PROJ(flags), filesBlock: `QA excerpt from QA notes.txt.${RAG_HINT}`, body: { chatId: CHAT, ...body } });
  await addTurn('project_hybrid_strip_hints', { ...inProj({}, { history: [{ role: 'user', content: `QA earlier ${SKILL_LEAD}` }, { role: 'assistant', content: 'QA ok' }] }),
    brain: BRAIN, tz: 'Europe/Berlin', modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('project_hybrid_granted_hints', { ...inProj({ allowCloud: true }), modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('project_hybrid_strip_code', { ...inProj({}), classification: { role: 'code' }, modes: { enabled: true, settings: S('hybrid', { cloud: { ...CLOUD, code: 'qa-cloud-code' } }), verdict: CLEAR } });
  await addTurn('project_cloud_mode_hints', { ...inProj({}), classification: { role: 'code' }, modes: { enabled: true, settings: S('cloud', { cloud: { ...CLOUD, code: 'qa-cloud-code' } }) } });
  await addTurn('project_local_hints', { ...inProj({}, { history: HIST }), brain: BRAIN, modes: { enabled: true, settings: S('local') } });
  // A Manual turn on a provider other than the local default one: off the box with no cloud route.
  const MANUAL = (extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', routing: 'manual', provider: 'qa-other', model: 'qa-other-model', chats: [], ...extra });
  await addTurn('manual_other_provider', { project: MANUAL(), body: { chatId: CHAT, history: HIST }, brain: BRAIN, tz: 'Asia/Tokyo', modes: { enabled: true, settings: S('cloud') } });
  await addTurn('manual_other_provider_plain', { project: MANUAL(), body: { history: HIST.slice(2) }, tz: 'America/New_York', modes: { enabled: false } });
  await addTurn('manual_default_provider', { project: MANUAL({ provider: 'default', model: FAST }), body: { history: HIST }, brain: BRAIN, tz: 'Europe/Berlin', modes: { enabled: false } });

  const MSGS = ['QA question', 'QA hi', 'QA plan the QA week'];
  const TZS = ['UTC', 'Europe/Berlin', 'America/New_York', 'Asia/Tokyo', 'Australia/Adelaide'];
  for (let n = 0; n < 60; n += 1) {
    const flags = pick([{}, {}, { allowCloud: true }, { forceLocal: true }]);
    const mode = pick(['local', 'cloud', 'hybrid', 'hybrid', 'hybrid', null]);
    const roles = rand() < 0.15 ? { code: CODE } : ROLES;
    const history = rand() < 0.6 ? HIST.slice(Math.floor(rand() * 3)) : [];
    await addTurn(`random_${n}`, {
      ...free(flags, { history, ...(rand() < 0.5 ? PRIV : {}) }), roles, message: pick(MSGS), tz: pick(TZS),
      ...(rand() < 0.5 ? { brain: BRAIN } : {}), classification: { role: pick(['fast', 'smart', 'code']) },
      modes: { enabled: rand() < 0.9, settings: S(mode, { cloud: pick([CLOUD, CLOUD, NO_CLOUD]) }), verdict: pick([CLEAR, SENS]),
        choice: pick([{ choice: 'cloud' }, { choice: 'cloud', remember: true }, { choice: 'local' }]) },
    });
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  const names = [...new Set([...TB.allToolboxes().flatMap((b) => (b.tools || []).map((t) => t.function.name)), 'qa_unknown_tool', 'read_project_file'])].sort();
  const toolModes = serverRequire('tools-mode.cjs');
  const facts = { providers: PROVIDERS,
    public: names.filter((n) => toolModes.isPublicToolName(n, TB.allToolboxes())),
    known: names.filter((n) => toolModes.PRIVATE_CORE_TOOLS.has(n) || TB.allToolboxes().some((b) => (b.tools || []).some((t) => t.function.name === n))),
    privateBoxes: TB.allToolboxes().map((b) => b.id).filter((id) => toolModes.isPrivateDataBox(id)) };
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_offbox.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sourcesRead())},\n"facts":${JSON.stringify(facts)},\n`
    + `"turns":[\n${lines(turns)}\n],\n"differences":[]}\n`;
  const check = process.argv.indexOf('--check');
  const moved = turns.filter((t) => t.meta && t.meta.routing && t.meta.routing.detail === 'private-tools').length;
  const counts = `${turns.length} turns (${turns.filter((t) => t.throws).length} throw, ${turns.filter((t) => t.refused).length} refused, `
    + `${turns.filter((t) => t.meta && t.meta.routing && t.meta.routing.route === 'cloud').length} cloud, ${moved} moved local)`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) {
    // A host that cannot run Node takes the fresh recording from CI (chat-prepare.yml uploads RECORD_FRESH).
    if (process.env.RECORD_FRESH) fs.writeFileSync(process.env.RECORD_FRESH, text);
    process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`, () => process.exit(0));
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text, () => process.exit(0));
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
